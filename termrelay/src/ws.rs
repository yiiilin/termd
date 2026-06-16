use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;
use termd_proto::{MessageType, Nonce, RelayClientId, RelayControlEnvelope, RouteRole, ServerId};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, trace, warn};

mod http_tunnel;
mod pipe_pump;
mod policy;
mod registry;
mod route_binder;
mod route_prelude;

#[cfg(test)]
use self::http_tunnel::{
    RelayHttpTunnelRequestBodyDeadline, relay_http_tunnel_forward_request_body,
    relay_http_tunnel_request_body_deadline,
};
use self::pipe_pump::{
    DataQueueByteBudget, FrameSender, PipePump, PumpDataReceiver, RelayDataSendError, RelayOutbound,
};
#[cfg(test)]
use self::policy::{
    OutboundFramePressureLevel, websocket_idle_ping_due, websocket_outbound_frame_pressure_level,
    websocket_receive_failed_is_noisy_client_disconnect,
};
#[cfg(test)]
use self::policy::{
    ROUTE_PRELUDE_TIMEOUT, WEBSOCKET_IDLE_PING_INTERVAL, WEBSOCKET_PONG_DEADLINE,
    WEBSOCKET_SEND_DEADLINE,
};
pub(crate) use self::policy::{WEBSOCKET_MAX_FRAME_SIZE, WEBSOCKET_MAX_MESSAGE_SIZE};
use self::policy::{
    WebSocketReceiveDebug, log_websocket_receive_failed, reject_oversized_frame,
    websocket_message_bytes, websocket_message_kind,
};
use self::registry::{
    ConnectionRegistration, ForwardReport, RelayError, RelayForwardOutcome, RelayRegistry,
};
use self::route_binder::bind_socket_route;
#[cfg(test)]
use self::route_binder::route_prelude_error_is_noisy_client_disconnect;

// 中文注释：relay 是 dumb pipe，不能长期替慢浏览器缓存终端流。
// 预算按 100ms 千兆链路的 BDP 量级设置；健康连接可以填满管道，
// 慢 client 仍会在预算耗尽后关闭并让前端重连拿 snapshot。
const DATA_CHANNEL_CAPACITY: usize = 32 * 1024;
const DATA_CHANNEL_BYTE_BUDGET: usize = 16 * 1024 * 1024;
const CONTROL_CHANNEL_CAPACITY: usize = 256;
const HTTP_TUNNEL_BODY_CHANNEL_CAPACITY: usize = 1;
#[cfg(not(test))]
const HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT: Duration = Duration::from_millis(50);
// 中文注释：route_ready 之后 client 可能先发少量 E2EE 数据，但 daemon data 必须及时接上；
// 超时后只关闭 relay transport，daemon session 本身仍由 daemon 管理。
#[cfg(not(test))]
const PENDING_CLIENT_PAIR_DEADLINE: Duration = Duration::from_secs(5);
#[cfg(test)]
const PENDING_CLIENT_PAIR_DEADLINE: Duration = Duration::from_millis(250);
const PENDING_CLIENTS_PER_ROOM_LIMIT: usize = 64;
// 中文注释：client route_ready 先于 daemon data 反连完成返回时，browser 可能立刻发送
// E2EE hello/auth/attach。relay 只做短暂 opaque 缓冲，避免公网反连慢几百毫秒就让前端超时。
const PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES: usize = 256;
const PRE_PAIR_CLIENT_BUFFER_MAX_BYTES: usize = 4 * 1024 * 1024;
const PRE_PAIR_ROOM_BUFFER_MAX_BYTES: usize = PRE_PAIR_CLIENT_BUFFER_MAX_BYTES * 2;
const RELAY_AUTH_TOKEN_MIN_BYTES: usize = 8;
// 中文注释：daemon_data 源 socket 需要一个本地短缓冲，把“读 daemon 输出”和“写浏览器”
// 两条链路拆开，避免单个慢 client 直接把源读循环卡住。这里仍然保持有界缓存，预算耗尽后
// 就关闭当前 transport，让上层按既有 snapshot/reconnect 路径恢复。
const DAEMON_DATA_INGRESS_FRAME_CAPACITY: usize = 2048;
const DAEMON_DATA_INGRESS_BYTE_BUDGET: usize = WEBSOCKET_MAX_FRAME_SIZE;
#[cfg(not(test))]
const DAEMON_DATA_INGRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(test)]
const DAEMON_DATA_INGRESS_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
type ConnectionId = u64;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RelayTrafficBucket {
    calls: u64,
    bytes: u64,
}

impl RelayTrafficBucket {
    fn record(&mut self, bytes: usize) {
        self.calls = self.calls.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    fn is_empty(self) -> bool {
        self.calls == 0 && self.bytes == 0
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RelayConnectionTraffic {
    in_text: RelayTrafficBucket,
    in_binary: RelayTrafficBucket,
    in_ping: RelayTrafficBucket,
    in_pong: RelayTrafficBucket,
    in_close: RelayTrafficBucket,
    forwarded_attempted: u64,
    forwarded_delivered: u64,
    forwarded_dropped: u64,
}

impl RelayConnectionTraffic {
    fn record_inbound(&mut self, message: &Message) {
        match message {
            Message::Text(raw) => self.in_text.record(raw.len()),
            Message::Binary(raw) => self.in_binary.record(raw.len()),
            Message::Ping(payload) => self.in_ping.record(payload.len()),
            Message::Pong(payload) => self.in_pong.record(payload.len()),
            Message::Close(_) => self.in_close.record(0),
        }
    }

    fn record_forward(&mut self, report: ForwardReport) {
        self.forwarded_attempted = self
            .forwarded_attempted
            .saturating_add(report.attempted as u64);
        self.forwarded_delivered = self
            .forwarded_delivered
            .saturating_add(report.delivered as u64);
        self.forwarded_dropped = self.forwarded_dropped.saturating_add(report.dropped as u64);
    }

    fn has_activity(self) -> bool {
        !self.in_text.is_empty()
            || !self.in_binary.is_empty()
            || !self.in_ping.is_empty()
            || !self.in_pong.is_empty()
            || !self.in_close.is_empty()
            || self.forwarded_attempted > 0
            || self.forwarded_delivered > 0
            || self.forwarded_dropped > 0
    }
}

/// relay 只区分连接方向，不表达 operator 或任何终端业务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRole {
    DaemonControl,
    DaemonData,
    Client,
}

impl ConnectionRole {
    fn from_route_role(role: RouteRole) -> Result<Self, RoutePreludeError> {
        match role {
            RouteRole::Client => Ok(Self::Client),
            RouteRole::DaemonControl => Ok(Self::DaemonControl),
            RouteRole::DaemonData => Ok(Self::DaemonData),
            RouteRole::DaemonMux => Err(RoutePreludeError::UnsupportedLegacyDaemonMux),
        }
    }
}

/// 被转发的业务 frame。这里刻意只保留 text/binary 两类可原样转发的数据。
#[derive(Clone, PartialEq, Eq)]
pub enum OpaqueFrame {
    Text(String),
    Binary(Vec<u8>),
}

impl fmt::Debug for OpaqueFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug 输出也只暴露元数据，避免未来误用 `?frame` 时把业务明文或密文写进日志。
        formatter
            .debug_struct("OpaqueFrame")
            .field("kind", &self.kind())
            .field("len", &self.len())
            .finish()
    }
}

impl OpaqueFrame {
    fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Binary(_) => "binary",
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Text(value) => value.len(),
            Self::Binary(value) => value.len(),
        }
    }
}

impl From<OpaqueFrame> for Message {
    fn from(frame: OpaqueFrame) -> Self {
        match frame {
            OpaqueFrame::Text(value) => Message::Text(value),
            OpaqueFrame::Binary(value) => Message::Binary(value),
        }
    }
}

fn relay_auth_token_has_safe_length(token: &str) -> bool {
    token.as_bytes().len() >= RELAY_AUTH_TOKEN_MIN_BYTES
}

fn relay_auth_token_constant_time_eq(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    let max_len = expected.len().max(provided.len());
    let mut diff = expected.len() ^ provided.len();

    // 中文注释：这里按最大长度完整扫描，避免普通字符串比较在首个不同字节提前返回。
    for index in 0..max_len {
        let expected_byte = expected.get(index).copied().unwrap_or(0);
        let provided_byte = provided.get(index).copied().unwrap_or(0);
        diff |= usize::from(expected_byte ^ provided_byte);
    }

    diff == 0
}

#[derive(Clone)]
pub struct RelayState {
    inner: Arc<RelayRegistry>,
    auth_token: Option<String>,
}

impl fmt::Debug for RelayState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // relay auth token 是 transport 凭证，Debug 输出只能显示是否配置，不能泄漏明文。
        formatter
            .debug_struct("RelayState")
            .field("auth_token_configured", &self.auth_token.is_some())
            .field("rooms", &self.room_count())
            .finish()
    }
}

impl Default for RelayState {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RelayState {
    pub fn new(auth_token: Option<String>) -> Self {
        if let Some(token) = auth_token.as_deref()
            && !relay_auth_token_has_safe_length(token)
        {
            warn!(
                min_bytes = RELAY_AUTH_TOKEN_MIN_BYTES,
                "relay auth token is too short; authenticated relay requests will be rejected"
            );
        }
        Self {
            inner: Arc::new(RelayRegistry::default()),
            auth_token,
        }
    }

    pub fn authorizes(&self, token: Option<&str>) -> bool {
        match self.auth_token.as_deref() {
            Some(expected) => token.is_some_and(|provided| {
                relay_auth_token_has_safe_length(expected)
                    && relay_auth_token_has_safe_length(provided)
                    && relay_auth_token_constant_time_eq(expected, provided)
            }),
            None => true,
        }
    }

    pub fn room_count(&self) -> usize {
        self.inner.room_count()
    }

    fn start_pending_client_pair_deadline(&self, registration: &ConnectionRegistration) {
        if registration.role != ConnectionRole::Client {
            return;
        }

        let state = self.clone();
        let registration = registration.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_CLIENT_PAIR_DEADLINE).await;
            if state.close_pending_client_if_unpaired(&registration) {
                warn!(
                    server_id = %registration.server_id.0,
                    client_connection_id = registration.id,
                    timeout_ms = PENDING_CLIENT_PAIR_DEADLINE.as_millis(),
                    "relay pending client data pair deadline exceeded"
                );
            }
        });
    }

    fn register_route(
        &self,
        prelude: &RoutePrelude,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        self.inner.register(prelude, sender)
    }

    #[cfg(test)]
    fn register_with_generation(
        &self,
        server_id: ServerId,
        role: ConnectionRole,
        route_generation: Option<Nonce>,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let prelude = RoutePrelude {
            server_id,
            route_role: match role {
                ConnectionRole::DaemonControl => RouteRole::DaemonControl,
                ConnectionRole::DaemonData => RouteRole::DaemonData,
                ConnectionRole::Client => RouteRole::Client,
            },
            connection_role: role,
            route_generation,
            client_id: None,
            data_token: None,
        };
        self.register_route(&prelude, sender)
    }

    #[cfg(test)]
    fn register(
        &self,
        server_id: ServerId,
        role: ConnectionRole,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let route_generation = match role {
            ConnectionRole::DaemonControl | ConnectionRole::DaemonData => {
                Some(test_route_generation(server_id))
            }
            ConnectionRole::Client => None,
        };
        self.register_with_generation(server_id, role, route_generation, sender)
    }

    fn unregister(&self, registration: &ConnectionRegistration) {
        self.inner.unregister(registration);
    }

    fn has_client(&self, server_id: ServerId, client_id: RelayClientId) -> bool {
        self.inner.has_client(server_id, client_id)
    }

    #[cfg(test)]
    fn client_has_data_pair(&self, registration: &ConnectionRegistration) -> bool {
        self.inner.client_has_data_pair(registration)
    }

    async fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        self.inner.forward_from(registration, frame).await
    }

    async fn forward_http_request_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        self.inner
            .forward_client_to_daemon_data_backpressured(registration, frame)
            .await
    }

    fn close_pending_client_if_unpaired(&self, registration: &ConnectionRegistration) -> bool {
        self.inner.close_pending_client_if_unpaired(registration)
    }

    async fn wait_client_data_pair(&self, registration: &ConnectionRegistration) -> bool {
        self.inner.wait_client_data_pair(registration).await
    }

    async fn flush_pre_pair_client_frames(&self, registration: &ConnectionRegistration) {
        self.inner.flush_pre_pair_client_frames(registration).await;
    }

    fn queue_pong_for_registration(
        &self,
        registration: &ConnectionRegistration,
        payload: Vec<u8>,
    ) -> RelayForwardOutcome {
        self.inner
            .queue_pong_for_registration(registration, payload)
    }
}

#[derive(Debug)]
struct DaemonDataIngress {
    sender: mpsc::Sender<OpaqueFrame>,
    byte_budget: Arc<DataQueueByteBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonDataIngressError {
    Backpressured,
    Closed,
}

impl DaemonDataIngress {
    fn new(frame_capacity: usize, byte_budget: usize) -> (Self, mpsc::Receiver<OpaqueFrame>) {
        let (sender, receiver) = mpsc::channel(frame_capacity);
        (
            Self {
                sender,
                byte_budget: Arc::new(DataQueueByteBudget::new(byte_budget)),
            },
            receiver,
        )
    }

    fn with_limits(
        frame_capacity: usize,
        byte_budget: usize,
    ) -> (Self, mpsc::Receiver<OpaqueFrame>) {
        Self::new(frame_capacity, byte_budget)
    }

    fn try_enqueue(&self, frame: OpaqueFrame) -> Result<(), DaemonDataIngressError> {
        let queued_bytes = frame.len();
        if self.byte_budget.exceeds_limit(queued_bytes)
            || !self.byte_budget.try_reserve(queued_bytes)
        {
            return Err(DaemonDataIngressError::Backpressured);
        }
        match self.sender.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(frame)) => {
                self.byte_budget.release(frame.len());
                Err(DaemonDataIngressError::Backpressured)
            }
            Err(mpsc::error::TrySendError::Closed(frame)) => {
                self.byte_budget.release(frame.len());
                Err(DaemonDataIngressError::Closed)
            }
        }
    }
}

#[derive(Debug)]
struct DaemonDataForwardTask {
    ingress: DaemonDataIngress,
    join_handle: tokio::task::JoinHandle<()>,
    stats: Arc<DaemonDataForwardStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonDataForwardDrainOutcome {
    Drained,
    TimedOut,
}

#[derive(Debug, Default)]
struct DaemonDataForwardStats {
    attempted: AtomicUsize,
    delivered: AtomicUsize,
    dropped: AtomicUsize,
}

impl DaemonDataForwardStats {
    fn record(&self, report: ForwardReport) {
        self.attempted
            .fetch_add(report.attempted, Ordering::Relaxed);
        self.delivered
            .fetch_add(report.delivered, Ordering::Relaxed);
        self.dropped.fetch_add(report.dropped, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ForwardReport {
        ForwardReport {
            attempted: self.attempted.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

impl DaemonDataForwardTask {
    fn spawn(state: RelayState, registration: ConnectionRegistration) -> Self {
        Self::spawn_with_limits(
            state,
            registration,
            DAEMON_DATA_INGRESS_FRAME_CAPACITY,
            DAEMON_DATA_INGRESS_BYTE_BUDGET,
        )
    }

    fn spawn_with_limits(
        state: RelayState,
        registration: ConnectionRegistration,
        frame_capacity: usize,
        byte_budget: usize,
    ) -> Self {
        let stats = Arc::new(DaemonDataForwardStats::default());
        let (ingress, receiver) = DaemonDataIngress::with_limits(frame_capacity, byte_budget);
        let join_handle = tokio::spawn(run_daemon_data_forwarder(
            state,
            registration,
            receiver,
            ingress.byte_budget.clone(),
            stats.clone(),
        ));
        Self {
            ingress,
            join_handle,
            stats,
        }
    }

    fn ingress(&self) -> &DaemonDataIngress {
        &self.ingress
    }

    async fn shutdown(self) -> (DaemonDataForwardDrainOutcome, ForwardReport) {
        let DaemonDataForwardTask {
            ingress,
            mut join_handle,
            stats,
        } = self;
        // 中文注释：退出时先关闭 ingress sender，让 forward task 在有限时间内把 relay
        // 已经收下的尾帧继续推进到 client outbound queue，避免源 socket 一关就截断尾部输出。
        drop(ingress);
        let drain_deadline = tokio::time::sleep(DAEMON_DATA_INGRESS_DRAIN_TIMEOUT);
        tokio::pin!(drain_deadline);
        let outcome = tokio::select! {
            result = &mut join_handle => {
                if let Err(error) = result {
                    warn!(?error, "relay daemon data forward task exited with join error");
                }
                DaemonDataForwardDrainOutcome::Drained
            }
            _ = &mut drain_deadline => {
                join_handle.abort();
                let _ = join_handle.await;
                DaemonDataForwardDrainOutcome::TimedOut
            }
        };
        (outcome, stats.snapshot())
    }
}

async fn run_daemon_data_forwarder(
    state: RelayState,
    registration: ConnectionRegistration,
    mut receiver: mpsc::Receiver<OpaqueFrame>,
    byte_budget: Arc<DataQueueByteBudget>,
    stats: Arc<DaemonDataForwardStats>,
) {
    while let Some(frame) = receiver.recv().await {
        byte_budget.release(frame.len());
        let report = state.forward_from(&registration, frame).await;
        stats.record(report);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutePrelude {
    server_id: ServerId,
    route_role: RouteRole,
    connection_role: ConnectionRole,
    route_generation: Option<Nonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<Nonce>,
}

#[derive(Debug, Error)]
enum RoutePreludeError {
    #[error("relay websocket closed before route_hello")]
    Closed,
    #[error("relay websocket receive failed during route prelude: {0}")]
    Receive(#[source] axum::Error),
    #[error("relay websocket send failed during route prelude: {0}")]
    Send(#[source] axum::Error),
    #[error("relay websocket send timed out during route prelude")]
    SendTimeout,
    #[error("relay route prelude pong timed out")]
    PongTimeout,
    #[error("route prelude frame exceeded transport limit: {0} bytes")]
    TooLarge(usize),
    #[error("route prelude JSON is invalid: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("expected route_hello as first envelope, got {0:?}")]
    UnexpectedType(MessageType),
    #[cfg_attr(test, allow(dead_code))]
    #[error("legacy daemon mux route is no longer accepted")]
    UnsupportedLegacyDaemonMux,
}

#[cfg(test)]
fn test_route_generation(server_id: ServerId) -> Nonce {
    Nonce(format!("test-route-generation-{}", server_id.0))
}

fn relay_control_frame(envelope: RelayControlEnvelope) -> OpaqueFrame {
    // 中文注释：control 线只承载 relay transport 生命周期消息，不进入 E2EE 业务协议。
    OpaqueFrame::Text(
        serde_json::to_string(&envelope)
            .expect("relay control envelope should encode as JSON text"),
    )
}

#[cfg(test)]
fn relay_control_from_frame(frame: &OpaqueFrame) -> Option<RelayControlEnvelope> {
    let OpaqueFrame::Text(raw) = frame else {
        return None;
    };
    serde_json::from_str(raw).ok()
}

pub async fn handle_socket(mut socket: WebSocket, state: RelayState) {
    let pipe_pump = PipePump::new(DATA_CHANNEL_CAPACITY);
    let tx = pipe_pump.sender();
    let mut endpoint_close_rx = tx.subscribe_close();
    let Some(bound_route) = bind_socket_route(&mut socket, &state, tx).await else {
        return;
    };
    let server_id = bound_route.server_id;
    let role = bound_route.role;
    let registration = bound_route.registration;

    let (sender, mut receiver) = socket.split();
    let mut receive_debug = WebSocketReceiveDebug::new(Instant::now());
    let mut traffic = RelayConnectionTraffic::default();
    // 中文注释：relay 必须是 dumb pipe，但 transport 读写不能互相拖住。
    // 每条 WebSocket 的写侧单独跑，主循环只负责持续读取输入并转发到目标队列；
    // writer 一旦写失败，会直接关闭 endpoint signal，主循环只认这个 signal 退出，
    // 不再依赖另一条 outcome 队列，避免持续入站时把 writer 失败饿住。
    let writer_task = pipe_pump.spawn_writer(sender, server_id, role, registration.id);
    let daemon_data_forwarder = (role == ConnectionRole::DaemonData)
        .then(|| DaemonDataForwardTask::spawn(state.clone(), registration.clone()));

    if role == ConnectionRole::DaemonData {
        // 中文注释：预配对帧 flush 会写入当前 daemon data 的 outbound data 队列。
        // 必须先启动 writer 消费该队列，再执行 flush；否则队列/字节预算被填满时，
        // flush 会等待一个尚未存在的消费者，造成连接读写启动前的饥饿等待。
        state.flush_pre_pair_client_frames(&registration).await;
    }

    loop {
        // 写侧由 writer task 消费；这里持续读入站帧，避免慢写把反方向输入也卡住。
        // 中文注释：writer outcome 只携带关闭/失败生命周期信号；成功写入不回报，
        // 避免大输出期间把“已发送统计”变成另一条缓存队列。
        tokio::select! {
            biased;

            _ = endpoint_close_rx.closed() => {
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    "relay websocket endpoint close signal received"
                );
                break;
            }
            inbound = receiver.next() => {
                let Some(inbound) = inbound else {
                    break;
                };
                let inbound = match inbound {
                    Ok(message) => message,
                    Err(error) => {
                        log_websocket_receive_failed(
                            server_id,
                            role,
                            registration.id,
                            &error,
                            &receive_debug,
                        );
                        break;
                    }
                };
                traffic.record_inbound(&inbound);
                receive_debug.record(
                    websocket_message_kind(&inbound),
                    websocket_message_bytes(&inbound),
                );
                // 中文注释：入站帧只作为当前连接的统计信号。
                // relay 不能因为 control pong 排在大量 stdout 后面就主动判 daemon 离线；
                // daemon 是否在线只由 WebSocket close/read/write error 暴露。
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    message_kind = websocket_message_kind(&inbound),
                    message_bytes = websocket_message_bytes(&inbound),
                    "relay websocket inbound frame received"
                );

                let forward_report = handle_inbound_message(
                    &state,
                    &registration,
                    inbound,
                    daemon_data_forwarder.as_ref(),
                ).await;
                traffic.record_forward(forward_report.report);
                if !forward_report.should_continue {
                    break;
                }
            }
        }
    }

    if let Some(forwarder) = daemon_data_forwarder {
        let (drain_outcome, forward_stats) = forwarder.shutdown().await;
        traffic.record_forward(forward_stats);
        if drain_outcome == DaemonDataForwardDrainOutcome::TimedOut {
            warn!(
                server_id = %server_id.0,
                connection_id = registration.id,
                timeout_ms = DAEMON_DATA_INGRESS_DRAIN_TIMEOUT.as_millis(),
                "relay daemon data forward task drain timed out during socket shutdown"
            );
        }
    }
    writer_task.abort();
    state.unregister(&registration);
    if traffic.has_activity() {
        trace!(
            server_id = %server_id.0,
            ?role,
            connection_id = registration.id,
            ?traffic,
            "relay websocket traffic counters"
        );
    }
    debug!(
        server_id = %server_id.0,
        ?role,
        connection_id = registration.id,
        "relay websocket unregistered"
    );
}

async fn handle_inbound_message(
    state: &RelayState,
    registration: &ConnectionRegistration,
    message: Message,
    daemon_data_forwarder: Option<&DaemonDataForwardTask>,
) -> RelayForwardOutcome {
    match message {
        Message::Text(text) => {
            if let Err(len) = reject_oversized_frame(text.len()) {
                warn!(
                    server_id = %registration.server_id.0,
                    ?registration.role,
                    connection_id = registration.id,
                    frame_len = len,
                    "dropping oversized relay text frame"
                );
                return RelayForwardOutcome::close_with(ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                });
            }
            if registration.role == ConnectionRole::DaemonData {
                queue_daemon_data_ingress_frame(
                    registration,
                    daemon_data_forwarder,
                    OpaqueFrame::Text(text),
                )
            } else {
                forward_opaque(state, registration, OpaqueFrame::Text(text)).await
            }
        }
        Message::Binary(bytes) => {
            if let Err(len) = reject_oversized_frame(bytes.len()) {
                warn!(
                    server_id = %registration.server_id.0,
                    ?registration.role,
                    connection_id = registration.id,
                    frame_len = len,
                    "dropping oversized relay binary frame"
                );
                return RelayForwardOutcome::close_with(ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                });
            }
            if registration.role == ConnectionRole::DaemonData {
                queue_daemon_data_ingress_frame(
                    registration,
                    daemon_data_forwarder,
                    OpaqueFrame::Binary(bytes),
                )
            } else {
                forward_opaque(state, registration, OpaqueFrame::Binary(bytes)).await
            }
        }
        Message::Ping(payload) => {
            // 中文注释：daemon data 线在一对一模式下不再承载 relay control payload。
            // Ping 只按 WebSocket 保活处理，不能再把旧 DataReady 解释成 idle pipe 入池。
            queue_relay_pong_for_inbound_ping(state, registration, payload).await
        }
        Message::Pong(_) => RelayForwardOutcome::continue_with(ForwardReport {
            attempted: 0,
            delivered: 0,
            dropped: 0,
        }),
        Message::Close(_) => RelayForwardOutcome::close_with(ForwardReport {
            attempted: 0,
            delivered: 0,
            dropped: 0,
        }),
    }
}

fn queue_daemon_data_ingress_frame(
    registration: &ConnectionRegistration,
    daemon_data_forwarder: Option<&DaemonDataForwardTask>,
    frame: OpaqueFrame,
) -> RelayForwardOutcome {
    let Some(forwarder) = daemon_data_forwarder else {
        warn!(
            server_id = %registration.server_id.0,
            connection_id = registration.id,
            "relay daemon data ingress task missing"
        );
        return RelayForwardOutcome::close_with(ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        });
    };
    match forwarder.ingress().try_enqueue(frame) {
        Ok(()) => RelayForwardOutcome::continue_with(ForwardReport {
            attempted: 0,
            delivered: 0,
            dropped: 0,
        }),
        Err(DaemonDataIngressError::Backpressured) => {
            warn!(
                server_id = %registration.server_id.0,
                connection_id = registration.id,
                frame_capacity = DAEMON_DATA_INGRESS_FRAME_CAPACITY,
                byte_budget = DAEMON_DATA_INGRESS_BYTE_BUDGET,
                "relay daemon data ingress queue exhausted"
            );
            RelayForwardOutcome::close_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            })
        }
        Err(DaemonDataIngressError::Closed) => RelayForwardOutcome::close_with(ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        }),
    }
}

async fn queue_relay_pong_for_inbound_ping(
    state: &RelayState,
    registration: &ConnectionRegistration,
    payload: Vec<u8>,
) -> RelayForwardOutcome {
    state.queue_pong_for_registration(registration, payload)
}

async fn forward_opaque(
    state: &RelayState,
    registration: &ConnectionRegistration,
    frame: OpaqueFrame,
) -> RelayForwardOutcome {
    let report = state.forward_from(registration, frame).await;
    let should_continue = !(registration.role == ConnectionRole::Client
        && report.dropped > 0
        && !state.has_client(registration.server_id, RelayClientId(registration.id)));
    // 中文注释：转发是 relay 的最高频路径，不能逐帧写日志；连接关闭时会输出聚合计数。
    RelayForwardOutcome {
        report,
        should_continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::router;
    use axum::body::{Body, Bytes};
    use axum::http::StatusCode;
    use futures_util::{SinkExt, StreamExt};
    use termd_proto::{
        Envelope, ErrorPayload, Nonce, PROTOCOL_PACKET_VERSION, ProtocolVersion,
        RelayHttpTunnelFrame, RouteHelloPayload, RouteReadyPayload, UnixTimestampMillis,
        decode_relay_http_tunnel_frame, encode_relay_http_tunnel_request_body,
    };
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    fn server_id(value: u128) -> ServerId {
        ServerId(uuid::Uuid::from_u128(value))
    }

    struct TestReceiver {
        control: mpsc::Receiver<RelayOutbound>,
        data: PumpDataReceiver,
    }

    impl TestReceiver {
        fn try_recv(&mut self) -> Result<RelayOutbound, TryRecvError> {
            match self.control.try_recv() {
                Ok(outbound) => Ok(outbound),
                Err(TryRecvError::Empty) => self.data.try_recv(),
                Err(error) => Err(error),
            }
        }
    }

    fn channel() -> (FrameSender, TestReceiver) {
        channel_with_data_capacity(DATA_CHANNEL_CAPACITY)
    }

    fn channel_with_control_capacity(control_capacity: usize) -> (FrameSender, TestReceiver) {
        let pipe_pump = PipePump::with_capacities(control_capacity, DATA_CHANNEL_CAPACITY);
        let (sender, control_rx, data_rx) = pipe_pump.into_test_parts();
        (
            sender,
            TestReceiver {
                control: control_rx,
                data: data_rx,
            },
        )
    }

    fn channel_with_data_capacity(data_capacity: usize) -> (FrameSender, TestReceiver) {
        let pipe_pump = PipePump::with_capacities(CONTROL_CHANNEL_CAPACITY, data_capacity);
        let (sender, control, data) = pipe_pump.into_test_parts();
        (sender, TestReceiver { control, data })
    }

    async fn recv_data_text(receiver: &mut TestReceiver) -> String {
        let outbound = receiver
            .data
            .recv()
            .await
            .expect("daemon data receiver should stay open");
        let RelayOutbound::Frame(OpaqueFrame::Text(text)) = outbound else {
            panic!("expected text data frame, got {outbound:?}");
        };
        text
    }

    async fn recv_data_frame(receiver: &mut TestReceiver) -> OpaqueFrame {
        let outbound = receiver
            .data
            .recv()
            .await
            .expect("daemon data receiver should stay open");
        let RelayOutbound::Frame(frame) = outbound else {
            panic!("expected data frame, got {outbound:?}");
        };
        frame
    }

    fn route_hello_with_generation(
        server_id: ServerId,
        role: RouteRole,
        route_generation: Option<Nonce>,
        client_id: Option<RelayClientId>,
        data_token: Option<Nonce>,
    ) -> Envelope<RouteHelloPayload> {
        Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role,
                protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                nonce: Nonce("test-route".to_owned()),
                route_generation,
                client_id,
                data_token,
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        )
    }

    fn route_hello(
        server_id: ServerId,
        role: RouteRole,
        client_id: Option<RelayClientId>,
        data_token: Option<Nonce>,
    ) -> Envelope<RouteHelloPayload> {
        let route_generation = match role {
            RouteRole::DaemonControl | RouteRole::DaemonData => {
                Some(test_route_generation(server_id))
            }
            RouteRole::Client | RouteRole::DaemonMux => None,
        };
        route_hello_with_generation(server_id, role, route_generation, client_id, data_token)
    }

    async fn register_test_route(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server_id: ServerId,
        role: RouteRole,
    ) {
        send_route_hello_with_data(socket, server_id, role, None, None).await;
        expect_route_ready(socket, server_id, role).await;
    }

    async fn send_route_hello_with_data(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server_id: ServerId,
        role: RouteRole,
        client_id: Option<RelayClientId>,
        data_token: Option<Nonce>,
    ) {
        send_route_hello_with_generation(socket, server_id, role, None, client_id, data_token)
            .await;
    }

    async fn send_route_hello_with_generation(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server_id: ServerId,
        role: RouteRole,
        route_generation: Option<Nonce>,
        client_id: Option<RelayClientId>,
        data_token: Option<Nonce>,
    ) {
        let route_generation = route_generation.or_else(|| match role {
            RouteRole::DaemonControl | RouteRole::DaemonData => {
                Some(test_route_generation(server_id))
            }
            RouteRole::Client | RouteRole::DaemonMux => None,
        });
        socket
            .send(ClientMessage::Text(
                serde_json::to_string(&route_hello_with_generation(
                    server_id,
                    role,
                    route_generation,
                    client_id,
                    data_token,
                ))
                .unwrap(),
            ))
            .await
            .unwrap();
    }

    async fn send_client_route_hello_only(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server_id: ServerId,
    ) {
        send_route_hello_with_data(socket, server_id, RouteRole::Client, None, None).await;
    }

    async fn expect_open_data(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> (RelayClientId, Nonce) {
        loop {
            let next = timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("daemon control should receive open_data")
                .expect("daemon control websocket should stay open")
                .expect("daemon control frame should be valid");
            if matches!(next, ClientMessage::Ping(_) | ClientMessage::Pong(_)) {
                continue;
            }
            let ClientMessage::Text(raw) = next else {
                panic!("expected relay control text frame, got {next:?}");
            };
            match serde_json::from_str::<RelayControlEnvelope>(&raw).unwrap() {
                RelayControlEnvelope::OpenData {
                    client_id,
                    data_token,
                } => return (client_id, data_token),
                other => panic!("expected open_data control envelope, got {other:?}"),
            }
        }
    }

    async fn expect_client_disconnected(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        wait: Duration,
    ) -> RelayClientId {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "daemon control should receive client_disconnected before timeout"
            );
            let next = timeout(remaining, socket.next())
                .await
                .expect("daemon control should quickly receive client_disconnected")
                .expect("daemon control websocket should stay open")
                .expect("daemon control frame should be valid");
            if matches!(next, ClientMessage::Ping(_) | ClientMessage::Pong(_)) {
                continue;
            }
            let ClientMessage::Text(raw) = next else {
                panic!("expected relay control text frame, got {next:?}");
            };
            match serde_json::from_str::<RelayControlEnvelope>(&raw).unwrap() {
                RelayControlEnvelope::ClientDisconnected { client_id } => return client_id,
                other => panic!("expected client_disconnected control envelope, got {other:?}"),
            }
        }
    }

    async fn expect_route_ready(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server_id: ServerId,
        role: RouteRole,
    ) {
        loop {
            let next = timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("relay should answer route_ready")
                .expect("relay websocket should stay open")
                .expect("route_ready frame should be valid");
            if matches!(next, ClientMessage::Ping(_) | ClientMessage::Pong(_)) {
                continue;
            }
            let ClientMessage::Text(raw) = next else {
                panic!("expected route_ready text frame, got {next:?}");
            };
            let ready: Envelope<RouteReadyPayload> = serde_json::from_str(&raw).unwrap();
            assert_eq!(ready.kind, MessageType::RouteReady);
            assert_eq!(ready.payload.server_id, server_id);
            assert_eq!(ready.payload.role, role);
            return;
        }
    }

    async fn next_data_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Option<ClientMessage> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for relay data frame"
            );
            let next = timeout(remaining, socket.next())
                .await
                .expect("timed out waiting for relay data frame")?;
            match next.unwrap() {
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                frame => return Some(frame),
            }
        }
    }

    async fn pair_client_with_daemon_data(
        url: &str,
        server_id: ServerId,
        daemon_control: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        RelayClientId,
    ) {
        let (mut client, _client_response) = connect_async(url).await.unwrap();
        send_client_route_hello_only(&mut client, server_id).await;
        let (client_id, data_token) = expect_open_data(daemon_control).await;

        let (mut daemon_data, _data_response) = connect_async(url).await.unwrap();
        send_route_hello_with_data(
            &mut daemon_data,
            server_id,
            RouteRole::DaemonData,
            Some(client_id),
            Some(data_token),
        )
        .await;
        expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData).await;
        expect_route_ready(&mut client, server_id, RouteRole::Client).await;

        (client, daemon_data, client_id)
    }

    fn decode_control(outbound: RelayOutbound) -> RelayControlEnvelope {
        match outbound {
            RelayOutbound::Frame(OpaqueFrame::Text(raw)) => serde_json::from_str(&raw).unwrap(),
            other => panic!("expected relay control frame, got {other:?}"),
        }
    }

    fn register_pending_client(
        state: &RelayState,
        server_id: ServerId,
        sender: FrameSender,
        control_rx: &mut TestReceiver,
    ) -> (ConnectionRegistration, RelayClientId, Nonce) {
        let client = state
            .register(server_id, ConnectionRole::Client, sender)
            .unwrap();
        let RelayControlEnvelope::OpenData {
            client_id,
            data_token,
        } = decode_control(control_rx.try_recv().unwrap())
        else {
            panic!("expected open_data after client registration");
        };
        assert_eq!(client_id, RelayClientId(client.id));
        (client, client_id, data_token)
    }

    #[tokio::test]
    async fn client_route_ready_does_not_wait_for_daemon_data_and_early_frames_are_piped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(95);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;

        let (mut client, _client_response) = connect_async(url.clone()).await.unwrap();
        send_client_route_hello_only(&mut client, server_id).await;

        let (client_id, data_token) = expect_open_data(&mut daemon_control).await;
        expect_route_ready(&mut client, server_id, RouteRole::Client).await;
        client
            .send(ClientMessage::Text("early-client-to-daemon".to_owned()))
            .await
            .unwrap();

        let (mut daemon_data, _data_response) = connect_async(url).await.unwrap();
        send_route_hello_with_data(
            &mut daemon_data,
            server_id,
            RouteRole::DaemonData,
            Some(client_id),
            Some(data_token),
        )
        .await;
        expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData).await;

        assert_eq!(
            next_data_frame(&mut daemon_data).await.unwrap(),
            ClientMessage::Text("early-client-to-daemon".to_owned())
        );

        client
            .send(ClientMessage::Text("client-to-daemon".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            next_data_frame(&mut daemon_data).await.unwrap(),
            ClientMessage::Text("client-to-daemon".to_owned())
        );

        daemon_data
            .send(ClientMessage::Binary(vec![1, 2, 3, 4]))
            .await
            .unwrap();
        assert_eq!(
            next_data_frame(&mut client).await.unwrap(),
            ClientMessage::Binary(vec![1, 2, 3, 4])
        );

        daemon_control.close(None).await.unwrap();
        daemon_data.close(None).await.unwrap();
        client.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn client_disconnect_while_waiting_for_data_pair_notifies_daemon_immediately() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(94);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;

        let (mut client, _client_response) = connect_async(url).await.unwrap();
        send_client_route_hello_only(&mut client, server_id).await;
        let (client_id, _data_token) = expect_open_data(&mut daemon_control).await;

        // 中文注释：浏览器快速切会话时，旧 client 会在 daemon data 线接入前关闭。
        // relay 必须立刻通知 daemon 取消这次数据线，而不是等 5 秒配对超时。
        client.close(None).await.unwrap();
        let disconnected =
            expect_client_disconnected(&mut daemon_control, Duration::from_millis(500)).await;
        assert_eq!(disconnected, client_id);

        daemon_control.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn pending_client_pair_deadline_closes_unpaired_client_and_notifies_daemon() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(97);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;

        let (mut client, _client_response) = connect_async(url).await.unwrap();
        send_client_route_hello_only(&mut client, server_id).await;
        let (client_id, _data_token) = expect_open_data(&mut daemon_control).await;
        expect_route_ready(&mut client, server_id, RouteRole::Client).await;

        // 中文注释：daemon data 一直不反连时，relay 必须自己回收 pending client，
        // 不能让公网连接和预配对缓冲无限占用 room 资源。
        let disconnected =
            expect_client_disconnected(&mut daemon_control, Duration::from_millis(500)).await;
        assert_eq!(disconnected, client_id);
        timeout(Duration::from_millis(500), async {
            loop {
                match client.next().await {
                    None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_))) => break,
                    Some(Ok(ClientMessage::Ping(payload))) => {
                        let _ = client.send(ClientMessage::Pong(payload)).await;
                    }
                    Some(Ok(ClientMessage::Pong(_))) => {}
                    Some(Ok(other)) => {
                        panic!("expected pending client websocket close, got {other:?}")
                    }
                }
            }
        })
        .await
        .expect("pending client should close after pair deadline");

        daemon_control.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn legacy_daemon_mux_route_is_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(96);
        let (mut socket, _response) = connect_async(format!("ws://{addr}/ws")).await.unwrap();

        send_route_hello_with_data(&mut socket, server_id, RouteRole::DaemonMux, None, None).await;
        let raw = match next_data_frame(&mut socket).await.unwrap() {
            ClientMessage::Text(raw) => raw,
            other => panic!("expected relay error text, got {other:?}"),
        };
        let error: Envelope<ErrorPayload> = serde_json::from_str(&raw).unwrap();
        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error.payload.code, "relay_legacy_route_rejected");

        server.abort();
    }

    #[tokio::test]
    async fn relay_client_socket_receives_transport_idle_ping() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(91);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;
        let (mut client, mut daemon_data, _) =
            pair_client_with_daemon_data(&url, server_id, &mut daemon_control).await;

        match timeout(Duration::from_secs(1), client.next()).await {
            Ok(Some(Ok(ClientMessage::Ping(payload)))) => {
                assert_eq!(payload.len(), std::mem::size_of::<u64>());
            }
            other => panic!("expected relay transport idle ping, got {other:?}"),
        }

        daemon_control.close(None).await.unwrap();
        daemon_data.close(None).await.unwrap();
        client.close(None).await.unwrap();
        server.abort();
    }

    #[test]
    fn client_reset_without_close_is_debug_noise_only_for_browser_clients() {
        let reset = "WebSocket protocol error: Connection reset without closing handshake";

        assert!(websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::Client,
            reset
        ));
        assert!(!websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::DaemonControl,
            reset
        ));
        assert!(!websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::DaemonData,
            reset
        ));
        assert!(!websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::Client,
            "WebSocket protocol error: protocol violation"
        ));
    }

    #[test]
    fn route_prelude_disconnects_are_debug_noise() {
        assert!(route_prelude_error_is_noisy_client_disconnect(
            &RoutePreludeError::Closed
        ));
        assert!(!route_prelude_error_is_noisy_client_disconnect(
            &RoutePreludeError::UnexpectedType(MessageType::Hello)
        ));
    }

    #[tokio::test]
    async fn relay_route_prelude_times_out_before_registration() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });

        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let next = timeout(
            ROUTE_PRELUDE_TIMEOUT + Duration::from_secs(2),
            socket.next(),
        )
        .await
        .expect("relay should close a socket that never sends route_hello");
        match next {
            None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_))) => {}
            other => panic!("expected relay prelude timeout close, got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn client_receives_retryable_error_when_daemon_control_is_offline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let raw = serde_json::to_string(&route_hello(server_id(1), RouteRole::Client, None, None))
            .unwrap();

        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        socket.send(ClientMessage::Text(raw)).await.unwrap();
        let next = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay should answer offline route errors")
            .expect("relay should send an error frame")
            .expect("relay error frame should be valid websocket data");
        let ClientMessage::Text(raw) = next else {
            panic!("expected relay route error text, got {next:?}");
        };
        let envelope: Envelope<ErrorPayload> = serde_json::from_str(&raw).unwrap();

        assert_eq!(envelope.kind, MessageType::Error);
        assert_eq!(envelope.payload.code, "relay_daemon_offline");
        server.abort();
    }

    #[test]
    fn relay_size_guard_rejects_oversized_frames() {
        assert_eq!(WEBSOCKET_MAX_FRAME_SIZE, 16 * 1024 * 1024);
        assert_eq!(WEBSOCKET_MAX_MESSAGE_SIZE, 16 * 1024 * 1024);
        assert!(reject_oversized_frame(WEBSOCKET_MAX_FRAME_SIZE).is_ok());
        assert_eq!(
            reject_oversized_frame(WEBSOCKET_MAX_FRAME_SIZE + 1),
            Err(WEBSOCKET_MAX_FRAME_SIZE + 1)
        );
    }

    #[test]
    fn relay_data_channel_capacity_does_not_limit_small_terminal_frames_before_byte_budget() {
        // 中文注释：浏览器离线或慢消费时，小 terminal frame 应主要受字节预算保护。
        // 100ms 千兆链路的 BDP 约为 12.5MB；预算低于这个量级会人为填不满管道。
        assert!(DATA_CHANNEL_BYTE_BUDGET >= 12 * 1024 * 1024);
        assert!(DATA_CHANNEL_CAPACITY >= DATA_CHANNEL_BYTE_BUDGET / 512);
    }

    #[test]
    fn relay_route_prelude_uses_browser_friendly_timeout() {
        assert_eq!(ROUTE_PRELUDE_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn relay_http_tunnel_deadline_is_not_file_api_path_specific() {
        assert_eq!(
            relay_http_tunnel_request_body_deadline("POST"),
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("post"),
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("PUT"),
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("PATCH"),
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("GET"),
            RelayHttpTunnelRequestBodyDeadline::None
        );
    }

    #[tokio::test]
    async fn relay_http_upload_first_chunk_deadline_times_out() {
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>())
                .into_data_stream();
        let registration = ConnectionRegistration {
            server_id: server_id(93),
            role: ConnectionRole::Client,
            id: 1,
            route_generation: None,
            paired_client_id: None,
        };

        let result = relay_http_tunnel_forward_request_body(
            RelayState::default(),
            registration,
            body,
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT),
        )
        .await;

        assert_eq!(result, Err(StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn relay_established_transport_uses_only_websocket_idle_ping() {
        // 中文注释：完成 route prelude 后，relay 仍只作为 dumb pipe 转发业务。
        // idle ping 是 WebSocket 控制帧，只保活代理/NAT，不进入 E2EE 业务协议。
        assert_eq!(WEBSOCKET_SEND_DEADLINE, Duration::from_secs(10));
        assert_eq!(WEBSOCKET_PONG_DEADLINE, Duration::from_secs(10));
        assert_eq!(WEBSOCKET_IDLE_PING_INTERVAL, Duration::from_millis(50));
        let now = Instant::now();
        assert!(!websocket_idle_ping_due(
            now + WEBSOCKET_IDLE_PING_INTERVAL - Duration::from_millis(1),
            now
        ));
        assert!(websocket_idle_ping_due(
            now + WEBSOCKET_IDLE_PING_INTERVAL,
            now
        ));
    }

    #[test]
    fn websocket_outbound_frame_pressure_distinguishes_slow_from_fast_large_frames() {
        // 中文注释：快速大帧是 terminal 流量的正常形态，不应刷 info 日志；慢发送才需要显眼诊断。
        assert_eq!(
            websocket_outbound_frame_pressure_level(256 * 1024, Duration::ZERO),
            OutboundFramePressureLevel::Debug
        );
        assert_eq!(
            websocket_outbound_frame_pressure_level(8 * 1024, Duration::from_millis(49)),
            OutboundFramePressureLevel::None
        );
        assert_eq!(
            websocket_outbound_frame_pressure_level(8 * 1024, Duration::from_millis(50)),
            OutboundFramePressureLevel::Info
        );
    }

    #[test]
    fn room_registers_control_client_and_data_pair() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, _data_rx) = channel();

        let control = state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        assert_eq!(control.role, ConnectionRole::DaemonControl);
        assert!(!state.client_has_data_pair(&client));

        let data_prelude = RoutePrelude {
            server_id,
            route_role: RouteRole::DaemonData,
            connection_role: ConnectionRole::DaemonData,
            route_generation: Some(test_route_generation(server_id)),
            client_id: Some(client_id),
            data_token: Some(data_token),
        };
        let data = state.register_route(&data_prelude, data_tx).unwrap();

        assert_eq!(data.role, ConnectionRole::DaemonData);
        assert_eq!(data.paired_client_id, Some(client.id));
        assert!(state.client_has_data_pair(&client));
        assert!(state.has_client(server_id, RelayClientId(client.id)));
    }

    #[test]
    fn room_rejects_pending_clients_over_room_limit() {
        let state = RelayState::default();
        let server_id = server_id(81);
        let (control_tx, mut control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        // 中文注释：这里故意不接 daemon data，让 client 都停在 pending 状态。
        for _ in 0..64 {
            let (client_tx, _client_rx) = channel();
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        }

        let (overflow_tx, _overflow_rx) = channel();
        assert!(
            state
                .register(server_id, ConnectionRole::Client, overflow_tx)
                .is_err(),
            "第 65 个未配对 client 应被 room 级 pending 数量上限拒绝"
        );
    }

    #[tokio::test]
    async fn room_pre_pair_buffer_uses_room_byte_budget() {
        let state = RelayState::default();
        let server_id = server_id(82);
        let (control_tx, mut control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let mut clients = Vec::new();
        for _ in 0..3 {
            let (client_tx, _client_rx) = channel();
            let (client, _client_id, _data_token) =
                register_pending_client(&state, server_id, client_tx, &mut control_rx);
            clients.push(client);
        }

        // 中文注释：每个 frame 都低于单 client 上限；第三个只应因 room 总预算被拒绝。
        let frame = vec![7; PRE_PAIR_CLIENT_BUFFER_MAX_BYTES * 3 / 4];
        assert_eq!(
            state
                .forward_from(&clients[0], OpaqueFrame::Binary(frame.clone()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        assert_eq!(
            state
                .forward_from(&clients[1], OpaqueFrame::Binary(frame.clone()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        assert_eq!(
            state
                .forward_from(&clients[2], OpaqueFrame::Binary(frame))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            }
        );
        assert!(
            !state.has_client(server_id, RelayClientId(clients[2].id)),
            "超出 room 预配对字节预算的 client 应被清理"
        );
    }

    #[tokio::test]
    async fn paired_client_frames_wait_behind_pre_pair_flush() {
        let state = RelayState::default();
        let server_id = server_id(84);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, mut data_rx) = channel_with_data_capacity(1);
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);

        assert_eq!(
            state
                .forward_from(&client, OpaqueFrame::Text("before-1".to_owned()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        assert_eq!(
            state
                .forward_from(&client, OpaqueFrame::Text("before-2".to_owned()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );

        data_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text(
                "occupy-capacity".to_owned(),
            )))
            .unwrap();
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();

        let flush_state = state.clone();
        let flush_registration = data_registration.clone();
        let mut flush_task = tokio::spawn(async move {
            flush_state
                .flush_pre_pair_client_frames(&flush_registration)
                .await;
        });
        assert!(
            timeout(Duration::from_millis(20), &mut flush_task)
                .await
                .is_err(),
            "预缓冲 flush 应先被占满的 daemon data 队列卡住"
        );

        // 中文注释：配对已经完成但旧预缓冲尚未冲刷完；新帧必须继续排到旧帧后面，
        // 不能因为 paired_daemon_data_id 已设置就绕过预缓冲直发。
        assert_eq!(
            state
                .forward_from(&client, OpaqueFrame::Text("during-flush".to_owned()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );

        assert_eq!(recv_data_text(&mut data_rx).await, "occupy-capacity");
        assert_eq!(recv_data_text(&mut data_rx).await, "before-1");
        assert_eq!(recv_data_text(&mut data_rx).await, "before-2");
        assert_eq!(recv_data_text(&mut data_rx).await, "during-flush");
        timeout(Duration::from_secs(1), flush_task)
            .await
            .expect("预缓冲 flush 应在队列被读取后完成")
            .unwrap();
        assert!(state.has_client(server_id, RelayClientId(client.id)));
    }

    #[tokio::test]
    async fn empty_pre_pair_frame_keeps_flush_fifo_gate_open() {
        let state = RelayState::default();
        let server_id = server_id(85);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);

        assert_eq!(
            state
                .forward_from(&client, OpaqueFrame::Text(String::new()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();

        // 中文注释：空 text frame 没有字节但仍是有效的 opaque 业务 frame。
        // data 已配对但 flush 尚未显式执行时，新 frame 必须继续进入预缓冲，
        // 否则会排到这个空 frame 前面。
        assert_eq!(
            state
                .forward_from(&client, OpaqueFrame::Text("after-empty".to_owned()))
                .await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );

        state.flush_pre_pair_client_frames(&data_registration).await;

        assert_eq!(
            recv_data_frame(&mut data_rx).await,
            OpaqueFrame::Text(String::new())
        );
        assert_eq!(
            recv_data_frame(&mut data_rx).await,
            OpaqueFrame::Text("after-empty".to_owned())
        );
    }

    #[tokio::test]
    async fn daemon_data_without_client_assignment_is_rejected() {
        let state = RelayState::default();
        let server_id = server_id(83);
        let (control_tx, _control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let (data_tx, _data_rx) = channel();
        let data = state.register_route(
            &RoutePrelude {
                server_id,
                route_role: RouteRole::DaemonData,
                connection_role: ConnectionRole::DaemonData,
                route_generation: Some(test_route_generation(server_id)),
                client_id: None,
                data_token: None,
            },
            data_tx,
        );

        // 中文注释：termd 到 relay 的 data pipe 必须由某个 client 的
        // OpenData 明确触发，不能再注册成可被后续 client 复用的 idle pool。
        assert_eq!(data, Err(RelayError::DaemonDataRouteInvalid));
    }

    #[test]
    fn daemon_routes_require_route_generation() {
        let state = RelayState::default();
        let server_id = server_id(84);

        let (control_tx, _control_rx) = channel();
        let control = state.register_route(
            &RoutePrelude {
                server_id,
                route_role: RouteRole::DaemonControl,
                connection_role: ConnectionRole::DaemonControl,
                route_generation: None,
                client_id: None,
                data_token: None,
            },
            control_tx,
        );
        assert_eq!(control, Err(RelayError::DaemonRouteGenerationRequired));

        let (data_tx, _data_rx) = channel();
        let data = state.register_route(
            &RoutePrelude {
                server_id,
                route_role: RouteRole::DaemonData,
                connection_role: ConnectionRole::DaemonData,
                route_generation: None,
                client_id: None,
                data_token: None,
            },
            data_tx,
        );
        assert_eq!(data, Err(RelayError::DaemonRouteGenerationRequired));
    }

    #[test]
    fn daemon_data_from_previous_route_generation_is_rejected() {
        let state = RelayState::default();
        let server_id = server_id(86);
        let generation_a = Nonce("route-generation-a".to_owned());
        let generation_b = Nonce("route-generation-b".to_owned());
        let (control_a_tx, _control_a_rx) = channel();
        let (control_b_tx, _control_b_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (stale_data_tx, _stale_data_rx) = channel();
        let (fresh_data_tx, _fresh_data_rx) = channel();
        let mut control_b_rx = _control_b_rx;

        state
            .register_with_generation(
                server_id,
                ConnectionRole::DaemonControl,
                Some(generation_a.clone()),
                control_a_tx,
            )
            .unwrap();
        state
            .register_with_generation(
                server_id,
                ConnectionRole::DaemonControl,
                Some(generation_b.clone()),
                control_b_tx,
            )
            .unwrap();
        let (_client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_b_rx);

        let stale = state.register_route(
            &RoutePrelude {
                server_id,
                route_role: RouteRole::DaemonData,
                connection_role: ConnectionRole::DaemonData,
                route_generation: Some(generation_a),
                client_id: Some(client_id),
                data_token: Some(data_token.clone()),
            },
            stale_data_tx,
        );
        assert_eq!(stale, Err(RelayError::DaemonDataRouteRejected));

        let fresh = state.register_route(
            &RoutePrelude {
                server_id,
                route_role: RouteRole::DaemonData,
                connection_role: ConnectionRole::DaemonData,
                route_generation: Some(generation_b),
                client_id: Some(client_id),
                data_token: Some(data_token),
            },
            fresh_data_tx,
        );
        assert!(
            fresh.is_ok(),
            "current generation paired data pipe should register"
        );
    }

    #[tokio::test]
    async fn stale_daemon_data_socket_from_previous_route_generation_is_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = server_id(87);
        let url = format!("ws://{addr}/ws");
        let generation_a = Nonce("socket-route-generation-a".to_owned());
        let generation_b = Nonce("socket-route-generation-b".to_owned());

        let (mut control_a, _response) = connect_async(url.clone()).await.unwrap();
        send_route_hello_with_generation(
            &mut control_a,
            server_id,
            RouteRole::DaemonControl,
            Some(generation_a.clone()),
            None,
            None,
        )
        .await;
        expect_route_ready(&mut control_a, server_id, RouteRole::DaemonControl).await;

        let (mut control_b, _response) = connect_async(url.clone()).await.unwrap();
        send_route_hello_with_generation(
            &mut control_b,
            server_id,
            RouteRole::DaemonControl,
            Some(generation_b.clone()),
            None,
            None,
        )
        .await;
        expect_route_ready(&mut control_b, server_id, RouteRole::DaemonControl).await;

        let (mut client, _client_response) = connect_async(url.clone()).await.unwrap();
        send_client_route_hello_only(&mut client, server_id).await;
        let (client_id, data_token) = expect_open_data(&mut control_b).await;

        let (mut stale_data, _response) = connect_async(url.clone()).await.unwrap();
        send_route_hello_with_generation(
            &mut stale_data,
            server_id,
            RouteRole::DaemonData,
            Some(generation_a),
            Some(client_id),
            Some(data_token.clone()),
        )
        .await;
        let ClientMessage::Text(raw) = next_data_frame(&mut stale_data).await.unwrap() else {
            panic!("expected relay error text for stale daemon data");
        };
        let error: Envelope<ErrorPayload> = serde_json::from_str(&raw).unwrap();
        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error.payload.code, "relay_data_route_rejected");

        let (mut fresh_data, _response) = connect_async(url.clone()).await.unwrap();
        send_route_hello_with_generation(
            &mut fresh_data,
            server_id,
            RouteRole::DaemonData,
            Some(generation_b),
            Some(client_id),
            Some(data_token),
        )
        .await;
        expect_route_ready(&mut fresh_data, server_id, RouteRole::DaemonData).await;
        expect_route_ready(&mut client, server_id, RouteRole::Client).await;

        control_a.close(None).await.unwrap();
        control_b.close(None).await.unwrap();
        client.close(None).await.unwrap();
        stale_data.close(None).await.unwrap();
        fresh_data.close(None).await.unwrap();
        server.abort();
    }

    #[test]
    fn relay_auth_rejects_unsafe_short_configured_token() {
        let short_state = RelayState::new(Some("short".to_owned()));
        assert!(
            !short_state.authorizes(Some("short")),
            "公网 relay token 明显过短时不能被接受"
        );

        let long_state = RelayState::new(Some("relay-secret-1".to_owned()));
        assert!(long_state.authorizes(Some("relay-secret-1")));
        assert!(!long_state.authorizes(Some("relay-secret-2")));
    }

    #[tokio::test]
    async fn http_tunnel_uses_daemon_data_pipe_without_parsing_body() {
        let state = RelayState::default();
        let server_id = server_id(91);
        let (control_tx, mut control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let tunnel_state = state.clone();
        let tunnel = tokio::spawn(async move {
            tunnel_state
                .http_tunnel(
                    server_id,
                    "POST".to_owned(),
                    "/api/files/upload/init".to_owned(),
                    vec![("x-termd-server-id".to_owned(), server_id.0.to_string())],
                    Body::from("opaque-e2ee-body").into_data_stream(),
                )
                .await
                .unwrap()
        });

        let RelayOutbound::Frame(open_frame) = control_rx.control.recv().await.unwrap() else {
            panic!("daemon control should receive open_data");
        };
        let RelayControlEnvelope::OpenData {
            client_id,
            data_token,
        } = relay_control_from_frame(&open_frame).unwrap()
        else {
            panic!("expected open_data");
        };
        let (data_tx, mut data_rx) = channel();
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        state.flush_pre_pair_client_frames(&data_registration).await;
        let RelayOutbound::Frame(OpaqueFrame::Binary(request_wire)) =
            data_rx.data.recv().await.unwrap()
        else {
            panic!("daemon data should receive tunnel request");
        };
        let termd_proto::RelayHttpTunnelFrame::RequestHead {
            method,
            path,
            headers,
        } = termd_proto::decode_relay_http_tunnel_frame(&request_wire).unwrap()
        else {
            panic!("expected HTTP tunnel request head");
        };
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/files/upload/init");
        assert_eq!(
            headers,
            vec![("x-termd-server-id".to_owned(), server_id.0.to_string())]
        );

        let RelayOutbound::Frame(OpaqueFrame::Binary(request_wire)) =
            data_rx.data.recv().await.unwrap()
        else {
            panic!("daemon data should receive tunnel body");
        };
        let termd_proto::RelayHttpTunnelFrame::RequestBody { body } =
            termd_proto::decode_relay_http_tunnel_frame(&request_wire).unwrap()
        else {
            panic!("expected HTTP tunnel request body");
        };
        assert_eq!(body, b"opaque-e2ee-body");

        let RelayOutbound::Frame(OpaqueFrame::Binary(request_wire)) =
            data_rx.data.recv().await.unwrap()
        else {
            panic!("daemon data should receive tunnel end");
        };
        assert_eq!(
            termd_proto::decode_relay_http_tunnel_frame(&request_wire),
            Some(termd_proto::RelayHttpTunnelFrame::RequestEnd)
        );

        state
            .forward_from(
                &data_registration,
                OpaqueFrame::Binary(termd_proto::encode_relay_http_tunnel_response_head(201)),
            )
            .await;
        state
            .forward_from(
                &data_registration,
                OpaqueFrame::Binary(termd_proto::encode_relay_http_tunnel_response_body(
                    b"opaque-response".to_vec(),
                )),
            )
            .await;
        state
            .forward_from(
                &data_registration,
                OpaqueFrame::Binary(termd_proto::encode_relay_http_tunnel_response_end()),
            )
            .await;
        let response = tunnel.await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"opaque-response");
    }

    #[tokio::test]
    async fn http_tunnel_request_body_waits_for_daemon_data_backpressure() {
        let state = RelayState::default();
        let server_id = server_id(93);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, mut data_rx) = channel_with_data_capacity(1);
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx.clone(),
            )
            .unwrap();
        state.flush_pre_pair_client_frames(&data_registration).await;
        data_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text(
                "queued-before-http-body".to_owned(),
            )))
            .unwrap();

        let send_state = state.clone();
        let send_client = client.clone();
        let mut send_task = tokio::spawn(async move {
            send_state
                .forward_http_request_from(
                    &send_client,
                    OpaqueFrame::Binary(encode_relay_http_tunnel_request_body(
                        b"large-upload-fragment".to_vec(),
                    )),
                )
                .await
        });

        assert!(
            timeout(Duration::from_millis(20), &mut send_task)
                .await
                .is_err(),
            "HTTP tunnel request body 应等待 daemon data 队列背压，而不是 try_send 失败"
        );
        let _queued = data_rx.data.recv().await.unwrap();
        let report = timeout(Duration::from_secs(1), &mut send_task)
            .await
            .expect("body send should resume after daemon data queue is drained")
            .unwrap();
        assert_eq!(
            report,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        let RelayOutbound::Frame(OpaqueFrame::Binary(request_wire)) =
            data_rx.data.recv().await.unwrap()
        else {
            panic!("daemon data should receive tunnel body after backpressure clears");
        };
        let RelayHttpTunnelFrame::RequestBody { body } =
            decode_relay_http_tunnel_frame(&request_wire).unwrap()
        else {
            panic!("expected HTTP tunnel request body");
        };
        assert_eq!(body, b"large-upload-fragment");
    }

    #[tokio::test]
    async fn http_tunnel_drop_after_response_head_unregisters_client() {
        let state = RelayState::default();
        let server_id = server_id(92);
        let (control_tx, mut control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let tunnel_state = state.clone();
        let tunnel = tokio::spawn(async move {
            tunnel_state
                .http_tunnel(
                    server_id,
                    "POST".to_owned(),
                    "/api/files/download".to_owned(),
                    Vec::new(),
                    Body::empty().into_data_stream(),
                )
                .await
                .unwrap()
        });

        let RelayOutbound::Frame(open_frame) = control_rx.control.recv().await.unwrap() else {
            panic!("daemon control should receive open_data");
        };
        let RelayControlEnvelope::OpenData {
            client_id,
            data_token,
        } = relay_control_from_frame(&open_frame).unwrap()
        else {
            panic!("expected open_data");
        };
        let (data_tx, mut data_rx) = channel();
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        state.flush_pre_pair_client_frames(&data_registration).await;
        let _ = data_rx.data.recv().await.unwrap();
        state
            .forward_from(
                &data_registration,
                OpaqueFrame::Binary(termd_proto::encode_relay_http_tunnel_response_head(200)),
            )
            .await;

        let response = timeout(Duration::from_secs(1), tunnel)
            .await
            .expect("relay should return response head")
            .unwrap();
        drop(response);

        timeout(Duration::from_millis(100), async {
            loop {
                match data_rx.data.recv().await {
                    Some(RelayOutbound::Close) | None => break,
                    Some(_) => {}
                }
            }
        })
        .await
        .expect("dropping response body should close paired data pipe");
        timeout(Duration::from_millis(100), async {
            loop {
                if !state.has_client(server_id, client_id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("dropping response body should unregister synthetic client");
        assert!(!state.has_client(server_id, client_id));
    }

    #[tokio::test]
    async fn daemon_control_disconnect_closes_clients_and_data_pipes() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, _data_rx) = channel();
        let mut control_close_rx = control_tx.subscribe_close();
        let mut client_close_rx = client_tx.subscribe_close();
        let mut data_close_rx = data_tx.subscribe_close();

        let control = state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (_client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        let data_prelude = RoutePrelude {
            server_id,
            route_role: RouteRole::DaemonData,
            connection_role: ConnectionRole::DaemonData,
            route_generation: Some(test_route_generation(server_id)),
            client_id: Some(client_id),
            data_token: Some(data_token),
        };
        state.register_route(&data_prelude, data_tx).unwrap();

        state.unregister(&control);

        timeout(Duration::from_millis(50), control_close_rx.closed())
            .await
            .expect("control close signal should fire");
        timeout(Duration::from_millis(50), client_close_rx.closed())
            .await
            .expect("client close signal should fire");
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("daemon data close signal should fire");
        assert_eq!(state.room_count(), 0);
    }

    #[tokio::test]
    async fn client_disconnect_closes_paired_data_pipe() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        let mut data_close_rx = data_tx.subscribe_close();
        let mut control_close_rx = control_tx.subscribe_close();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        let data_prelude = RoutePrelude {
            server_id,
            route_role: RouteRole::DaemonData,
            connection_role: ConnectionRole::DaemonData,
            route_generation: Some(test_route_generation(server_id)),
            client_id: Some(client_id),
            data_token: Some(data_token),
        };
        state.register_route(&data_prelude, data_tx).unwrap();

        state.unregister(&client);

        assert_eq!(data_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert!(matches!(
            data_rx.try_recv().unwrap_err(),
            TryRecvError::Empty | TryRecvError::Disconnected
        ));
        assert_eq!(control_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("client disconnect should close paired daemon data pipe");
        assert!(!state.has_client(server_id, client_id));
        assert!(
            timeout(Duration::from_millis(30), control_close_rx.closed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn client_disconnect_requires_new_control_open_data() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        let mut data_close_rx = data_tx.subscribe_close();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let (client_a_tx, _client_a_rx) = channel();
        let (client_a, client_a_id, token_a) =
            register_pending_client(&state, server_id, client_a_tx, &mut control_rx);
        let daemon_data = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_a_id),
                    data_token: Some(token_a),
                },
                data_tx,
            )
            .unwrap();

        state.unregister(&client_a);
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("client disconnect should close the old daemon data pipe");
        assert_eq!(data_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(
            handle_inbound_message(
                &state,
                &daemon_data,
                Message::Ping(b"legacy-data-ready".to_vec()),
                None,
            )
            .await
            .report,
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            }
        );
        assert!(matches!(
            data_rx.try_recv().unwrap_err(),
            TryRecvError::Empty | TryRecvError::Disconnected
        ));

        let (client_b_tx, _client_b_rx) = channel();
        let client_b = state
            .register(server_id, ConnectionRole::Client, client_b_tx)
            .unwrap();
        let RelayControlEnvelope::OpenData {
            client_id: client_b_id,
            ..
        } = decode_control(control_rx.try_recv().unwrap())
        else {
            panic!("expected cold data assignment on daemon control for second client");
        };
        assert_eq!(client_b_id, RelayClientId(client_b.id));
    }

    #[tokio::test]
    async fn client_pre_pair_frame_flushes_after_matching_daemon_data_connects() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let (client_tx, _client_rx) = channel();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        let first_frame = OpaqueFrame::Binary(b"first-upload-request-head".to_vec());
        assert_eq!(
            state.forward_from(&client, first_frame.clone()).await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );

        assert_eq!(data_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        let data_registration = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        state.flush_pre_pair_client_frames(&data_registration).await;

        // 中文注释：client 早到帧只能在匹配的 daemon data 反连完成后进入 data 线；
        // relay 不再把 OpenData 写进预热 data pipe。
        assert_eq!(
            data_rx.data.recv().await.unwrap(),
            RelayOutbound::Frame(first_frame)
        );
        assert_eq!(data_rx.control.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[tokio::test]
    async fn data_line_control_shaped_text_stays_opaque() {
        let state = RelayState::default();
        let server_id = server_id(94);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, mut client_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx, &mut control_rx);
        let data = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();

        let control_shaped = r#"{"type":"data_ready","payload":{}}"#.to_owned();
        let report = state
            .forward_from(&client, OpaqueFrame::Text(control_shaped.clone()))
            .await;
        assert_eq!(report.delivered, 1);
        assert_eq!(
            data_rx.data.recv().await.unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text(control_shaped.clone()))
        );

        let report = state
            .forward_from(&data, OpaqueFrame::Text(control_shaped.clone()))
            .await;
        assert_eq!(report.delivered, 1);
        assert_eq!(
            client_rx.data.recv().await.unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text(control_shaped))
        );
    }

    #[tokio::test]
    async fn daemon_data_disconnect_closes_only_paired_client() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();

        let (client_a_tx, _client_a_rx) = channel();
        let (data_a_tx, _data_a_rx) = channel();
        let mut client_a_close_rx = client_a_tx.subscribe_close();
        let (client_a, client_a_id, token_a) =
            register_pending_client(&state, server_id, client_a_tx, &mut control_rx);
        let data_a = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_a_id),
                    data_token: Some(token_a),
                },
                data_a_tx,
            )
            .unwrap();

        let (client_b_tx, _client_b_rx) = channel();
        let (data_b_tx, _data_b_rx) = channel();
        let mut client_b_close_rx = client_b_tx.subscribe_close();
        let (client_b, client_b_id, token_b) =
            register_pending_client(&state, server_id, client_b_tx, &mut control_rx);
        state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_b_id),
                    data_token: Some(token_b),
                },
                data_b_tx,
            )
            .unwrap();

        state.unregister(&data_a);

        timeout(Duration::from_millis(50), client_a_close_rx.closed())
            .await
            .expect("paired client should close when daemon data disconnects");
        assert!(!state.has_client(server_id, RelayClientId(client_a.id)));
        assert!(state.has_client(server_id, RelayClientId(client_b.id)));
        assert!(
            timeout(Duration::from_millis(30), client_b_close_rx.closed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slow_client_data_queue_backpressures_daemon_data_without_immediate_disconnect() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, mut client_rx) = channel_with_data_capacity(1);
        let (data_tx, mut data_rx) = channel();
        let mut client_close_rx = client_tx.subscribe_close();
        let mut data_close_rx = data_tx.subscribe_close();
        let mut control_close_rx = control_tx.subscribe_close();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx.clone(), &mut control_rx);
        let daemon_data = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();

        client_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let forward_task = tokio::spawn({
            let state = state.clone();
            let daemon_data = daemon_data.clone();
            async move {
                state
                    .forward_from(&daemon_data, OpaqueFrame::Text("overflow".to_owned()))
                    .await
            }
        });

        // 中文注释：第一次 frame 占满 client data 队列时，下一个下行 frame 不应立刻触发
        // client/data pipe 被关闭；它应该先在 relay 内等待浏览器 writer 消费。
        let client_closed = client_close_rx.closed();
        let data_closed = data_close_rx.closed();
        let control_closed = control_close_rx.closed();
        tokio::pin!(client_closed);
        tokio::pin!(data_closed);
        tokio::pin!(control_closed);
        assert!(
            timeout(Duration::from_millis(30), &mut client_closed)
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(30), &mut data_closed)
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(30), &mut control_closed)
                .await
                .is_err()
        );

        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned()))
        );
        let report = timeout(Duration::from_millis(50), forward_task)
            .await
            .expect("forward task should finish after queue drains")
            .expect("forward task should join cleanly");
        assert_eq!(
            report,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text("overflow".to_owned()))
        );
        assert_eq!(control_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(data_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        assert!(state.has_client(server_id, RelayClientId(client.id)));
    }

    #[tokio::test]
    async fn daemon_data_ingress_keeps_ping_path_live_while_client_queue_is_blocked() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, mut client_rx) = channel_with_data_capacity(1);
        let (data_tx, mut data_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (_client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx.clone(), &mut control_rx);
        let daemon_data = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        let forwarder = DaemonDataForwardTask::spawn_with_limits(
            state.clone(),
            daemon_data.clone(),
            4,
            WEBSOCKET_MAX_FRAME_SIZE,
        );

        client_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let data_report = handle_inbound_message(
            &state,
            &daemon_data,
            Message::Text("overflow".to_owned()),
            Some(&forwarder),
        )
        .await;
        assert_eq!(
            data_report.report,
            ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            }
        );

        let ping_report = handle_inbound_message(
            &state,
            &daemon_data,
            Message::Ping(b"keepalive".to_vec()),
            Some(&forwarder),
        )
        .await;
        assert!(ping_report.should_continue);
        assert_eq!(
            ping_report.report,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        assert_eq!(
            data_rx.try_recv().unwrap(),
            RelayOutbound::Pong(b"keepalive".to_vec())
        );
        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned()))
        );

        let drained = timeout(Duration::from_millis(50), client_rx.data.recv())
            .await
            .expect("queued daemon data frame should flush after client queue drains")
            .expect("client data channel should stay open");
        assert_eq!(
            drained,
            RelayOutbound::Frame(OpaqueFrame::Text("overflow".to_owned()))
        );

        let (drain_outcome, forward_stats) = forwarder.shutdown().await;
        assert_eq!(drain_outcome, DaemonDataForwardDrainOutcome::Drained);
        assert_eq!(
            forward_stats,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
    }

    #[tokio::test]
    async fn daemon_data_forwarder_drains_queued_tail_frames_before_shutdown() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, mut client_rx) = channel_with_data_capacity(1);
        let (data_tx, _data_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let (_client, client_id, data_token) =
            register_pending_client(&state, server_id, client_tx.clone(), &mut control_rx);
        let daemon_data = state
            .register_route(
                &RoutePrelude {
                    server_id,
                    route_role: RouteRole::DaemonData,
                    connection_role: ConnectionRole::DaemonData,
                    route_generation: Some(test_route_generation(server_id)),
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        let forwarder = DaemonDataForwardTask::spawn_with_limits(
            state.clone(),
            daemon_data.clone(),
            4,
            WEBSOCKET_MAX_FRAME_SIZE,
        );

        client_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let enqueue_report = handle_inbound_message(
            &state,
            &daemon_data,
            Message::Text("tail".to_owned()),
            Some(&forwarder),
        )
        .await;
        assert_eq!(
            enqueue_report.report,
            ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            }
        );

        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned()))
        );
        let (drain_outcome, forward_stats) = forwarder.shutdown().await;
        assert_eq!(drain_outcome, DaemonDataForwardDrainOutcome::Drained);
        assert_eq!(
            forward_stats,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );
        let drained = timeout(Duration::from_millis(50), client_rx.data.recv())
            .await
            .expect("queued tail frame should flush during shutdown")
            .expect("client data channel should stay open");
        assert_eq!(
            drained,
            RelayOutbound::Frame(OpaqueFrame::Text("tail".to_owned()))
        );
    }

    #[tokio::test]
    async fn endpoint_close_signal_terminates_even_when_control_queue_is_full() {
        let (sender, mut receiver) = channel_with_control_capacity(1);
        let mut close_rx = sender.subscribe_close();

        sender
            .try_send_control(RelayOutbound::Pong(Vec::new()))
            .unwrap();
        assert!(matches!(
            sender.try_send_control(RelayOutbound::Close),
            Err(mpsc::error::TrySendError::Full(RelayOutbound::Close))
        ));

        sender.close_endpoint();

        timeout(Duration::from_millis(50), close_rx.closed())
            .await
            .expect("endpoint close signal should not wait for queue capacity");
        assert_eq!(
            receiver.try_recv().unwrap(),
            RelayOutbound::Pong(Vec::new())
        );
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn data_queue_rejects_large_frames_by_byte_budget_before_frame_capacity() {
        let (sender, mut receiver) = channel_with_data_capacity(DATA_CHANNEL_CAPACITY);
        let frame_size = 16 * 1024;
        let frame = RelayOutbound::Frame(OpaqueFrame::Binary(vec![7; frame_size]));
        let accepted_before_budget_full = DATA_CHANNEL_BYTE_BUDGET / frame_size;

        for _ in 0..accepted_before_budget_full {
            sender.try_send(frame.clone()).unwrap();
        }

        assert!(matches!(
            sender.try_send(frame.clone()),
            Err(RelayDataSendError::BudgetFull)
        ));
        assert_eq!(receiver.try_recv().unwrap(), frame);
        sender.try_send(frame).unwrap();
    }

    #[test]
    fn relay_traffic_counters_aggregate_forwarded_frames() {
        let mut traffic = RelayConnectionTraffic::default();

        traffic.record_inbound(&Message::Binary(vec![1, 2, 3]));
        traffic.record_forward(ForwardReport {
            attempted: 2,
            delivered: 1,
            dropped: 1,
        });

        assert!(traffic.has_activity());
        assert_eq!(traffic.in_binary.calls, 1);
        assert_eq!(traffic.in_binary.bytes, 3);
        assert_eq!(traffic.forwarded_attempted, 2);
        assert_eq!(traffic.forwarded_delivered, 1);
        assert_eq!(traffic.forwarded_dropped, 1);
    }

    #[test]
    fn frame_metadata_does_not_include_payload_content() {
        let text = OpaqueFrame::Text("pair_request terminal plaintext".to_owned());
        let binary = OpaqueFrame::Binary(b"pairing_token ciphertext bytes".to_vec());

        assert_eq!(text.kind(), "text");
        assert_eq!(text.len(), "pair_request terminal plaintext".len());
        assert!(!format!("{text:?}").contains("pair_request"));
        assert!(!format!("{text:?}").contains("terminal plaintext"));
        assert!(!format!("{binary:?}").contains("pairing_token"));
        assert!(!format!("{binary:?}").contains("ciphertext"));
    }

    #[test]
    fn relay_state_debug_redacts_auth_token() {
        let state = RelayState::new(Some("relay-secret-1".to_owned()));
        let rendered = format!("{state:?}");

        assert!(rendered.contains("auth_token_configured"));
        assert!(!rendered.contains("relay-secret-1"));
    }
}
