use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, BodyDataStream, Bytes};
use axum::extract::ws::{Message, WebSocket};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, ErrorPayload, MessageType, Nonce, RelayClientId, RelayControlEnvelope,
    RelayHttpTunnelFrame, RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
    decode_relay_data_control, decode_relay_http_tunnel_frame, encode_relay_data_control,
    encode_relay_http_tunnel_request_body, encode_relay_http_tunnel_request_end,
    encode_relay_http_tunnel_request_head,
};
use thiserror::Error;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tracing::{debug, info, trace, warn};

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
// 中文注释：client route_ready 先于 daemon data 反连完成返回时，browser 可能立刻发送
// E2EE hello/auth/attach。relay 只做短暂 opaque 缓冲，避免公网反连慢几百毫秒就让前端超时。
const PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES: usize = 256;
const PRE_PAIR_CLIENT_BUFFER_MAX_BYTES: usize = 4 * 1024 * 1024;
// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_OUTBOUND_FRAME_PRESSURE_INFO_THRESHOLD: Duration = Duration::from_millis(50);
const WEBSOCKET_OUTBOUND_FRAME_PRESSURE_DEBUG_BYTES: usize = 128 * 1024;
#[cfg(not(test))]
const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_millis(50);
// 终端 snapshot 是 E2EE 后的 opaque binary frame，relay 不能拆包或解析。
// 这里的上限必须能容纳 1000 行 scrollback 的完整重绘，同时仍保留传输层内存保护。
pub(crate) const WEBSOCKET_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub(crate) const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
type ConnectionId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayHttpTunnelRequestBodyDeadline {
    None,
    FirstChunk(Duration),
    Whole(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundFramePressureLevel {
    None,
    Debug,
    Info,
}

fn websocket_outbound_frame_pressure_level(
    frame_len: usize,
    elapsed: Duration,
) -> OutboundFramePressureLevel {
    if elapsed >= WEBSOCKET_OUTBOUND_FRAME_PRESSURE_INFO_THRESHOLD {
        return OutboundFramePressureLevel::Info;
    }
    if frame_len >= WEBSOCKET_OUTBOUND_FRAME_PRESSURE_DEBUG_BYTES {
        return OutboundFramePressureLevel::Debug;
    }
    OutboundFramePressureLevel::None
}

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

#[derive(Debug, Clone)]
struct FrameSender {
    control: mpsc::Sender<RelayOutbound>,
    data: mpsc::Sender<RelayOutbound>,
    data_budget: Arc<DataQueueByteBudget>,
    close_signal: EndpointCloseSignal,
}

impl FrameSender {
    fn channel(
        data_capacity: usize,
    ) -> (
        Self,
        mpsc::Receiver<RelayOutbound>,
        mpsc::Receiver<RelayOutbound>,
    ) {
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_CAPACITY);
        let (data_tx, data_rx) = mpsc::channel(data_capacity);
        let data_budget = Arc::new(DataQueueByteBudget::new(DATA_CHANNEL_BYTE_BUDGET));
        let close_signal = EndpointCloseSignal::new();
        (
            Self {
                control: control_tx,
                data: data_tx,
                data_budget,
                close_signal,
            },
            control_rx,
            data_rx,
        )
    }

    fn try_send_data(&self, outbound: RelayOutbound) -> Result<(), RelayDataSendError> {
        let queued_bytes = outbound.queued_data_bytes();
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        if !self.data_budget.try_reserve(queued_bytes) {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data queue byte budget exhausted"
            );
            return Err(RelayDataSendError::BudgetFull);
        }
        match self.data.try_send(outbound) {
            Ok(()) => {
                trace!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue accepted frame"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue full"
                );
                Err(RelayDataSendError::BudgetFull)
            }
            Err(mpsc::error::TrySendError::Closed(_outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue closed"
                );
                Err(RelayDataSendError::Closed)
            }
        }
    }

    async fn send_data(&self, outbound: RelayOutbound) -> Result<(), RelayDataSendError> {
        let queued_bytes = outbound.queued_data_bytes();
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        if self.data_budget.exceeds_limit(queued_bytes) {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data frame exceeds byte budget"
            );
            return Err(RelayDataSendError::BudgetFull);
        }
        let mut close_rx = self.subscribe_close();
        let budget_reserved = tokio::select! {
            biased;

            _ = self.data.closed() => false,
            reserved = self.data_budget.reserve_or_wait(queued_bytes, &mut close_rx) => reserved,
        };
        if !budget_reserved {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data queue closed while waiting for byte budget"
            );
            return Err(RelayDataSendError::Closed);
        }

        let permit = tokio::select! {
            biased;

            _ = close_rx.closed() => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind,
                    queued_bytes,
                    "relay data queue closed while waiting for channel capacity"
                );
                return Err(RelayDataSendError::Closed);
            }
            permit = self.data.reserve() => permit,
        };

        match permit {
            Ok(permit) => {
                permit.send(outbound);
                trace!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue accepted frame"
                );
                Ok(())
            }
            Err(_closed) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue closed"
                );
                Err(RelayDataSendError::Closed)
            }
        }
    }

    #[cfg(test)]
    fn try_send(&self, outbound: RelayOutbound) -> Result<(), RelayDataSendError> {
        self.try_send_data(outbound)
    }

    fn try_send_control(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), mpsc::error::TrySendError<RelayOutbound>> {
        // 生命周期控制消息不能被普通业务队列挤掉；但它也必须有上限。
        // 如果底层 WebSocket 慢写到 control 都堆满，继续无界缓存只会拖垮整个 relay。
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        match self.control.try_send(outbound) {
            Ok(()) => {
                trace!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue accepted frame"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(outbound)) => {
                warn!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue full"
                );
                Err(mpsc::error::TrySendError::Full(outbound))
            }
            Err(mpsc::error::TrySendError::Closed(outbound)) => {
                warn!(
                    outbound = outbound_label,
                    frame_kind, "relay control queue closed"
                );
                Err(mpsc::error::TrySendError::Closed(outbound))
            }
        }
    }

    fn subscribe_close(&self) -> EndpointCloseReceiver {
        self.close_signal.subscribe()
    }

    fn close_endpoint(&self) {
        self.close_signal.close();
    }

    fn request_close(&self) {
        // 中文注释：close 信号是可靠退出路径；队列里的 Close 只是尽力发送 WebSocket
        // close frame。即使 control 队列已满，endpoint 也会通过信号退出。
        self.close_endpoint();
        let _ = self.try_send_control(RelayOutbound::Close);
    }
}

#[derive(Debug)]
enum RelayDataSendError {
    BudgetFull,
    Closed,
}

#[derive(Debug)]
struct DataQueueByteBudget {
    limit: usize,
    queued: AtomicUsize,
    notify: Notify,
}

impl DataQueueByteBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            queued: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }

        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.limit {
                return false;
            }
            match self.queued.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        // 中文注释：release 只在成功入队后的出队/发送失败回滚路径调用。
        // 使用 saturating_sub 兜住测试或未来改动造成的重复释放，不让计数下溢成巨大值。
        let _ = self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(bytes))
            });
        self.notify.notify_waiters();
    }

    fn exceeds_limit(&self, bytes: usize) -> bool {
        bytes > self.limit
    }

    async fn reserve_or_wait(&self, bytes: usize, close_rx: &mut EndpointCloseReceiver) -> bool {
        if bytes == 0 {
            return true;
        }
        debug_assert!(!self.exceeds_limit(bytes));
        loop {
            // 中文注释：必须先注册/enable 通知，再检查容量。
            // 如果先 try_reserve 再 notified().await，release 可能刚好夹在两者之间，
            // notify_waiters 不会为未来 waiter 保留许可，等待方就会永久睡眠。
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_reserve(bytes) {
                return true;
            }
            tokio::select! {
                biased;

                _ = close_rx.closed() => return false,
                _ = &mut notified => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
struct EndpointCloseSignal {
    sender: watch::Sender<bool>,
}

impl EndpointCloseSignal {
    fn new() -> Self {
        let (sender, _receiver) = watch::channel(false);
        Self { sender }
    }

    fn subscribe(&self) -> EndpointCloseReceiver {
        EndpointCloseReceiver {
            receiver: self.sender.subscribe(),
        }
    }

    fn close(&self) {
        let _ = self.sender.send(true);
    }
}

#[derive(Debug)]
struct EndpointCloseReceiver {
    receiver: watch::Receiver<bool>,
}

impl EndpointCloseReceiver {
    async fn closed(&mut self) {
        if *self.receiver.borrow() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayOutbound {
    Frame(OpaqueFrame),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

impl RelayOutbound {
    fn label(&self) -> &'static str {
        match self {
            Self::Frame(_) => "frame",
            Self::Ping(_) => "ping",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn frame_kind(&self) -> &'static str {
        match self {
            Self::Frame(frame) => frame.kind(),
            Self::Ping(_) => "ping",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn queued_data_bytes(&self) -> usize {
        match self {
            Self::Frame(frame) => frame.len(),
            Self::Ping(_) | Self::Pong(_) | Self::Close => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedRelayOutbound {
    Frame(OpaqueFrame),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayWriteResult {
    Sent,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayWriterOutcome {
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy)]
struct WebSocketReceiveDebug {
    last_inbound_at: Instant,
    last_inbound_kind: &'static str,
    inbound_messages: u64,
    inbound_bytes: u64,
}

impl WebSocketReceiveDebug {
    fn new(now: Instant) -> Self {
        Self {
            last_inbound_at: now,
            last_inbound_kind: "none",
            inbound_messages: 0,
            inbound_bytes: 0,
        }
    }

    fn record(&mut self, kind: &'static str, bytes: usize) {
        let now = Instant::now();
        self.last_inbound_at = now;
        self.last_inbound_kind = kind;
        self.inbound_messages = self.inbound_messages.saturating_add(1);
        self.inbound_bytes = self.inbound_bytes.saturating_add(bytes as u64);
    }
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

fn websocket_message_kind(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
    }
}

fn websocket_message_bytes(message: &Message) -> usize {
    match message {
        Message::Text(raw) => raw.len(),
        Message::Binary(raw) => raw.len(),
        Message::Ping(payload) | Message::Pong(payload) => payload.len(),
        Message::Close(_) => 0,
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
        Self {
            inner: Arc::new(RelayRegistry::default()),
            auth_token,
        }
    }

    pub fn authorizes(&self, token: Option<&str>) -> bool {
        match self.auth_token.as_deref() {
            Some(expected) => token == Some(expected),
            None => true,
        }
    }

    pub fn room_count(&self) -> usize {
        self.inner.room_count()
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
        _route_generation: Option<Nonce>,
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
        self.register_with_generation(server_id, role, None, sender)
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

    pub async fn http_tunnel(
        &self,
        server_id: ServerId,
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: BodyDataStream,
    ) -> Result<Response, StatusCode> {
        let request_body_deadline = relay_http_tunnel_request_body_deadline(&method, &path);
        let request_head =
            encode_relay_http_tunnel_request_head(method.clone(), path.clone(), headers)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
        let (sender, _control_rx, mut data_rx) = FrameSender::channel(DATA_CHANNEL_CAPACITY);
        let data_budget = sender.data_budget.clone();
        let prelude = RoutePrelude {
            server_id,
            route_role: RouteRole::Client,
            connection_role: ConnectionRole::Client,
            client_id: None,
            data_token: None,
        };
        let registration = self
            .register_route(&prelude, sender)
            .map_err(|error| match error {
                RelayError::DaemonControlOffline => StatusCode::SERVICE_UNAVAILABLE,
                RelayError::DaemonControlBusy => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::BAD_GATEWAY,
            })?;
        debug!(
            server_id = %server_id.0,
            client_connection_id = registration.id,
            method = %method,
            path = %path,
            "relay HTTP tunnel registered synthetic client"
        );
        let mut registration_guard =
            RelayHttpTunnelRegistrationGuard::new(self.clone(), registration);
        if timeout(
            ROUTE_PRELUDE_TIMEOUT,
            self.inner
                .wait_client_data_pair(registration_guard.registration()),
        )
        .await
        .ok()
            != Some(true)
        {
            warn!(
                server_id = %server_id.0,
                client_connection_id = registration_guard.registration().id,
                method = %method,
                path = %path,
                "relay HTTP tunnel timed out waiting for data pair"
            );
            return Err(StatusCode::GATEWAY_TIMEOUT);
        }
        let report = self
            .forward_from(
                registration_guard.registration(),
                OpaqueFrame::Binary(request_head),
            )
            .await;
        if report.delivered == 0 {
            warn!(
                server_id = %server_id.0,
                client_connection_id = registration_guard.registration().id,
                method = %method,
                path = %path,
                ?report,
                "relay HTTP tunnel failed to forward request head"
            );
            return Err(StatusCode::BAD_GATEWAY);
        }
        debug!(
            server_id = %server_id.0,
            client_connection_id = registration_guard.registration().id,
            method = %method,
            path = %path,
            "relay HTTP tunnel forwarded request head"
        );

        let request_state = self.clone();
        let request_registration = registration_guard.registration().clone();
        let request_method = method.clone();
        let request_path = path.clone();
        let (request_result_tx, mut request_result_rx) =
            tokio::sync::oneshot::channel::<Result<(), StatusCode>>();
        let request_task = tokio::spawn(async move {
            let result = relay_http_tunnel_forward_request_body(
                request_state,
                request_registration,
                body,
                request_body_deadline,
            )
            .await;
            if let Err(status) = result {
                warn!(
                    method = %request_method,
                    path = %request_path,
                    status = status.as_u16(),
                    "relay HTTP tunnel request body forwarding failed"
                );
            }
            let _ = request_result_tx.send(result);
        });
        registration_guard.set_request_task(request_task);

        let mut request_done = false;
        loop {
            tokio::select! {
                biased;

                request_result = &mut request_result_rx, if !request_done => {
                    request_done = true;
                    match request_result {
                        Ok(Ok(())) => {}
                        Ok(Err(status)) => {
                            return Err(status);
                        }
                        Err(_) => {
                            warn!(
                                server_id = %server_id.0,
                                client_connection_id = registration_guard.registration().id,
                                method = %method,
                                path = %path,
                                "relay HTTP tunnel request body task dropped"
                            );
                            return Err(StatusCode::BAD_GATEWAY);
                        }
                    }
                    continue;
                }
                outbound = data_rx.recv() => {
                    let Some(outbound) = outbound else {
                        warn!(
                            server_id = %server_id.0,
                            client_connection_id = registration_guard.registration().id,
                            method = %method,
                            path = %path,
                            "relay HTTP tunnel data pipe closed before response head"
                        );
                        break;
                    };
                    data_budget.release(outbound.queued_data_bytes());
                    if let RelayOutbound::Frame(OpaqueFrame::Binary(raw)) = outbound
                        && let Some(RelayHttpTunnelFrame::ResponseHead { status }) =
                            decode_relay_http_tunnel_frame(&raw)
                    {
                        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                        debug!(
                            server_id = %server_id.0,
                            client_connection_id = registration_guard.registration().id,
                            method = %method,
                            path = %path,
                            status = status.as_u16(),
                            "relay HTTP tunnel received response head"
                        );
                        let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, io::Error>>(HTTP_TUNNEL_BODY_CHANNEL_CAPACITY);
                        let response_state = self.clone();
                        let response_registration = registration_guard.registration().clone();
                        let request_result_rx = (!request_done).then_some(request_result_rx);
                        let request_task = registration_guard.take_request_task();
                        registration_guard.disarm();
                        tokio::spawn(relay_http_tunnel_forward_response_body(
                            response_state,
                            response_registration,
                            data_rx,
                            body_tx,
                            request_result_rx,
                            request_task,
                            data_budget,
                        ));
                        let body_stream = futures_util::stream::unfold(body_rx, |mut body_rx| async move {
                            body_rx.recv().await.map(|item| (item, body_rx))
                        });
                        return Ok((status, Body::from_stream(body_stream)).into_response());
                    }
                }
            }
        }
        Err(StatusCode::BAD_GATEWAY)
    }
}

struct RelayHttpTunnelRegistrationGuard {
    state: RelayState,
    registration: Option<ConnectionRegistration>,
    request_task: Option<JoinHandle<()>>,
}

impl RelayHttpTunnelRegistrationGuard {
    fn new(state: RelayState, registration: ConnectionRegistration) -> Self {
        Self {
            state,
            registration: Some(registration),
            request_task: None,
        }
    }

    fn registration(&self) -> &ConnectionRegistration {
        self.registration
            .as_ref()
            .expect("HTTP tunnel registration guard must be armed")
    }

    fn set_request_task(&mut self, task: JoinHandle<()>) {
        self.request_task = Some(task);
    }

    fn take_request_task(&mut self) -> JoinHandle<()> {
        self.request_task
            .take()
            .expect("HTTP tunnel request task must exist after response head")
    }

    fn disarm(&mut self) {
        self.registration = None;
    }
}

impl Drop for RelayHttpTunnelRegistrationGuard {
    fn drop(&mut self) {
        if let Some(task) = self.request_task.take() {
            task.abort();
        }
        if let Some(registration) = self.registration.take() {
            // 中文注释：HTTP handler future 可能在 response head 前被 axum 取消；
            // Drop guard 覆盖这段窗口，确保 synthetic client 不会留在 relay room 中。
            self.state.unregister(&registration);
        }
    }
}

async fn relay_http_tunnel_forward_request_body(
    state: RelayState,
    registration: ConnectionRegistration,
    body: BodyDataStream,
    deadline: RelayHttpTunnelRequestBodyDeadline,
) -> Result<(), StatusCode> {
    match deadline {
        RelayHttpTunnelRequestBodyDeadline::Whole(deadline) => {
            let forward =
                relay_http_tunnel_forward_request_body_inner(state, registration, body, None);
            timeout(deadline, forward)
                .await
                .unwrap_or(Err(StatusCode::GATEWAY_TIMEOUT))
        }
        RelayHttpTunnelRequestBodyDeadline::FirstChunk(deadline) => {
            relay_http_tunnel_forward_request_body_inner(state, registration, body, Some(deadline))
                .await
        }
        RelayHttpTunnelRequestBodyDeadline::None => {
            relay_http_tunnel_forward_request_body_inner(state, registration, body, None).await
        }
    }
}

async fn relay_http_tunnel_forward_request_body_inner(
    state: RelayState,
    registration: ConnectionRegistration,
    mut body: BodyDataStream,
    first_chunk_deadline: Option<Duration>,
) -> Result<(), StatusCode> {
    let mut first_chunk = true;
    let mut chunk_count = 0_usize;
    let mut forwarded_bytes = 0_usize;
    loop {
        let next = if first_chunk {
            first_chunk = false;
            if let Some(deadline) = first_chunk_deadline {
                timeout(deadline, body.next())
                    .await
                    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            } else {
                body.next().await
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if chunk.is_empty() {
            continue;
        }
        chunk_count = chunk_count.saturating_add(1);
        forwarded_bytes = forwarded_bytes.saturating_add(chunk.len());
        let report = state
            .forward_http_request_from(
                &registration,
                OpaqueFrame::Binary(encode_relay_http_tunnel_request_body(chunk.to_vec())),
            )
            .await;
        if report.delivered == 0 {
            warn!(
                server_id = %registration.server_id.0,
                client_connection_id = registration.id,
                chunk_count,
                forwarded_bytes,
                ?report,
                "relay HTTP tunnel failed to forward request body chunk"
            );
            return Err(StatusCode::BAD_GATEWAY);
        }
    }
    let report = state
        .forward_http_request_from(
            &registration,
            OpaqueFrame::Binary(encode_relay_http_tunnel_request_end()),
        )
        .await;
    if report.delivered == 0 {
        warn!(
            server_id = %registration.server_id.0,
            client_connection_id = registration.id,
            chunk_count,
            forwarded_bytes,
            ?report,
            "relay HTTP tunnel failed to forward request end"
        );
        return Err(StatusCode::BAD_GATEWAY);
    }
    debug!(
        server_id = %registration.server_id.0,
        client_connection_id = registration.id,
        chunk_count,
        forwarded_bytes,
        "relay HTTP tunnel forwarded complete request body"
    );
    Ok(())
}

fn relay_http_tunnel_request_body_deadline(
    method: &str,
    path: &str,
) -> RelayHttpTunnelRequestBodyDeadline {
    // 中文注释：只有短 metadata body 需要 deadline；`/api/files/upload` 是真实文件长流，
    // 因此只限制首个 metadata chunk，不限制后续文件内容的整体耗时。
    if !method.eq_ignore_ascii_case("POST") {
        return RelayHttpTunnelRequestBodyDeadline::None;
    }
    match path {
        "/api/files/upload/init" | "/api/files/upload/abort" | "/api/files/download" => {
            RelayHttpTunnelRequestBodyDeadline::Whole(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        }
        "/api/files/upload" => {
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        }
        _ => RelayHttpTunnelRequestBodyDeadline::None,
    }
}

async fn relay_http_tunnel_forward_response_body(
    state: RelayState,
    registration: ConnectionRegistration,
    mut data_rx: mpsc::Receiver<RelayOutbound>,
    body_tx: mpsc::Sender<Result<Bytes, io::Error>>,
    mut request_result_rx: Option<tokio::sync::oneshot::Receiver<Result<(), StatusCode>>>,
    request_task: JoinHandle<()>,
    data_budget: Arc<DataQueueByteBudget>,
) {
    let mut clean_shutdown = false;
    loop {
        tokio::select! {
            biased;

            request_result = async {
                match request_result_rx.as_mut() {
                    Some(rx) => rx.await.ok(),
                    None => None,
                }
            }, if request_result_rx.is_some() => {
                request_result_rx = None;
                if !matches!(request_result, Some(Ok(()))) {
                    let _ = body_tx.send(Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "relay HTTP request body forwarding failed",
                    ))).await;
                    break;
                }
            }
            _ = body_tx.closed() => {
                // 中文注释：浏览器拿到 ResponseHead 后可能立刻关闭 body；不能等 daemon
                // 再发 ResponseBody/End 才清理 synthetic client 和 data pipe。
                break;
            }
            outbound = data_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                let queued_bytes = outbound.queued_data_bytes();
                let RelayOutbound::Frame(OpaqueFrame::Binary(raw)) = outbound else {
                    data_budget.release(queued_bytes);
                    continue;
                };
                match decode_relay_http_tunnel_frame(&raw) {
                    Some(RelayHttpTunnelFrame::ResponseBody { body }) => {
                        let send_result = body_tx.send(Ok(Bytes::from(body))).await;
                        data_budget.release(queued_bytes);
                        if send_result.is_err() {
                            break;
                        }
                    }
                    Some(RelayHttpTunnelFrame::ResponseEnd) => {
                        data_budget.release(queued_bytes);
                        clean_shutdown = true;
                        break;
                    }
                    _ => {
                        data_budget.release(queued_bytes);
                    }
                }
            }
        }
    }
    if !clean_shutdown {
        let _ = body_tx
            .send(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay HTTP response stream ended early",
            )))
            .await;
    }
    request_task.abort();
    state.unregister(&registration);
}

#[derive(Debug, Default)]
struct RelayRegistry {
    rooms: Mutex<HashMap<ServerId, RelayRoom>>,
    next_connection_id: AtomicU64,
}

#[derive(Debug, Default)]
struct RelayRoom {
    daemon_control: Option<ConnectionEndpoint>,
    daemon_data: HashMap<ConnectionId, ConnectionEndpoint>,
    idle_daemon_data: VecDeque<ConnectionId>,
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ClientCloseOutcome {
    removed: bool,
    notified_data_pipe: bool,
}

#[derive(Debug, Default, Clone)]
struct PrePairBuffer {
    frames: VecDeque<OpaqueFrame>,
    bytes: usize,
}

impl PrePairBuffer {
    fn push(&mut self, frame: OpaqueFrame) -> Result<(), OpaqueFrame> {
        let frame_len = frame.len();
        if self.frames.len() >= PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES
            || self.bytes.saturating_add(frame_len) > PRE_PAIR_CLIENT_BUFFER_MAX_BYTES
        {
            return Err(frame);
        }
        self.bytes = self.bytes.saturating_add(frame_len);
        self.frames.push_back(frame);
        Ok(())
    }

    fn drain(&mut self) -> Vec<OpaqueFrame> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

impl RelayRoom {
    fn is_empty(&self) -> bool {
        self.daemon_control.is_none() && self.daemon_data.is_empty() && self.clients.is_empty()
    }

    fn close_clients(&mut self) {
        for (_, client) in self.clients.drain() {
            // daemon control 不可用时，client 必须尽快收到 close，避免继续等待业务响应直到超时。
            if let Some(data_id) = client.paired_daemon_data_id
                && let Some(daemon_data) = self.daemon_data.remove(&data_id)
            {
                daemon_data.sender.request_close();
            }
            client.pair_signal.notify_waiters();
            client.sender.request_close();
        }
        for (_, daemon_data) in self.daemon_data.drain() {
            daemon_data.sender.request_close();
        }
        self.idle_daemon_data.clear();
    }

    fn clear_daemon_control_and_dependents(&mut self) {
        if let Some(daemon_control) = self.daemon_control.take() {
            // 中文注释：control 线是 daemon 是否在线的唯一裁定来源；它断开时，
            // 所有关联 browser/data transport 都必须关闭，让 browser 重新建链。
            daemon_control.sender.request_close();
        }
        self.close_clients();
    }

    fn close_client_transport(&mut self, client_id: ConnectionId) -> ClientCloseOutcome {
        let Some(client) = self.clients.remove(&client_id) else {
            return ClientCloseOutcome::default();
        };

        let mut outcome = ClientCloseOutcome {
            removed: true,
            notified_data_pipe: false,
        };
        client.pair_signal.notify_waiters();
        if let Some(data_id) = client.paired_daemon_data_id {
            self.idle_daemon_data.retain(|idle_id| *idle_id != data_id);
            if let Some(daemon_data) = self.daemon_data.remove(&data_id) {
                // 中文注释：同一条 daemon data WebSocket 上 control/data 使用不同队列。
                // client 断开时旧 data frame 可能已经排在 ClientDisconnected 后面；如果复用
                // 这条 pipe，新 client 的 HTTP upload/terminal 输入会被旧 frame 污染。
                // 因此 paired client 一断开就关闭对应 data pipe，让 daemon 建一条新的 idle
                // pipe。WebSocket close 本身就是清理该 client 协议上下文的可靠信号。
                outcome.notified_data_pipe = true;
                daemon_data.sender.request_close();
            }
        }

        client.sender.request_close();
        outcome
    }

    fn mark_daemon_data_ready(&mut self, data_id: ConnectionId) -> bool {
        let Some(daemon_data) = self.daemon_data.get(&data_id) else {
            return false;
        };
        if daemon_data.paired_client_id.is_some() {
            warn!(
                daemon_data_connection_id = data_id,
                paired_client_id = daemon_data.paired_client_id,
                "relay ignored data_ready from still-paired daemon data pipe"
            );
            return false;
        }
        if !self
            .idle_daemon_data
            .iter()
            .any(|idle_id| *idle_id == data_id)
        {
            self.idle_daemon_data.push_back(data_id);
        }
        true
    }

    fn assign_idle_daemon_data_to_client(
        &mut self,
        client_id: ConnectionId,
        data_token: Nonce,
    ) -> Option<ConnectionId> {
        while let Some(data_id) = self.idle_daemon_data.pop_front() {
            let Some(daemon_data) = self.daemon_data.get(&data_id) else {
                continue;
            };
            if daemon_data.paired_client_id.is_some() {
                continue;
            }

            if !self.clients.contains_key(&client_id) {
                return None;
            }
            let assign = RelayControlEnvelope::OpenData {
                client_id: RelayClientId(client_id),
                data_token: data_token.clone(),
            };
            // 中文注释：idle data pipe 的 OpenData 必须和随后 client 业务帧在同一条
            // FIFO lane 里排队。writer 可能刚发过 data_ready pong，下一轮会优先 data lane；
            // 如果 OpenData 放在 control lane，HTTP upload 的 request head/body 可能抢先到达
            // daemon，daemon 还在等 assignment 时就会把业务二进制判成无效控制帧并断开。
            let send_result = daemon_data
                .sender
                .try_send_data(relay_data_control_outbound(assign));
            match send_result {
                Ok(()) => {
                    if let Some(daemon_data) = self.daemon_data.get_mut(&data_id) {
                        daemon_data.paired_client_id = Some(client_id);
                    }
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.paired_daemon_data_id = Some(data_id);
                        client.pair_signal.notify_waiters();
                        return Some(data_id);
                    }
                    return None;
                }
                Err(error) => {
                    warn!(
                        client_connection_id = client_id,
                        daemon_data_connection_id = data_id,
                        ?error,
                        "relay dropped unusable idle daemon data pipe"
                    );
                    if let Some(daemon_data) = self.daemon_data.remove(&data_id) {
                        daemon_data.sender.request_close();
                    }
                }
            }
        }

        None
    }

    fn buffer_client_frame(
        &mut self,
        client_id: ConnectionId,
        frame: OpaqueFrame,
    ) -> Result<(), OpaqueFrame> {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return Err(frame);
        };
        client.pre_pair_buffer.push(frame)
    }

    fn drain_pre_pair_client_frames(
        &mut self,
        client_id: ConnectionId,
    ) -> Option<Vec<OpaqueFrame>> {
        self.clients
            .get_mut(&client_id)
            .map(|client| client.pre_pair_buffer.drain())
    }

    fn notify_client_disconnected_to_control(&mut self, client_id: ConnectionId) {
        let Some(daemon_control) = self.daemon_control.as_ref() else {
            return;
        };
        let envelope = RelayControlEnvelope::ClientDisconnected {
            client_id: RelayClientId(client_id),
        };
        if daemon_control
            .sender
            .try_send_control(RelayOutbound::Frame(relay_control_frame(envelope)))
            .is_err()
        {
            // control 线连生命周期消息都写不进时，保留 room 只会留下假在线状态。
            self.clear_daemon_control_and_dependents();
        }
    }
}

#[derive(Debug, Clone)]
struct ConnectionEndpoint {
    id: ConnectionId,
    sender: FrameSender,
    data_token: Option<Nonce>,
    paired_daemon_data_id: Option<ConnectionId>,
    paired_client_id: Option<ConnectionId>,
    pre_pair_buffer: PrePairBuffer,
    pair_signal: Arc<Notify>,
}

impl ConnectionEndpoint {
    fn new(id: ConnectionId, sender: FrameSender) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: None,
            pre_pair_buffer: PrePairBuffer::default(),
            pair_signal: Arc::new(Notify::new()),
        }
    }

    fn new_client(id: ConnectionId, sender: FrameSender, data_token: Nonce) -> Self {
        Self {
            id,
            sender,
            data_token: Some(data_token),
            paired_daemon_data_id: None,
            paired_client_id: None,
            pre_pair_buffer: PrePairBuffer::default(),
            pair_signal: Arc::new(Notify::new()),
        }
    }

    fn new_daemon_data(id: ConnectionId, sender: FrameSender, client_id: ConnectionId) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: Some(client_id),
            pre_pair_buffer: PrePairBuffer::default(),
            pair_signal: Arc::new(Notify::new()),
        }
    }

    fn new_idle_daemon_data(id: ConnectionId, sender: FrameSender) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: None,
            pre_pair_buffer: PrePairBuffer::default(),
            pair_signal: Arc::new(Notify::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionRegistration {
    server_id: ServerId,
    role: ConnectionRole,
    id: ConnectionId,
    paired_client_id: Option<ConnectionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardReport {
    pub attempted: usize,
    pub delivered: usize,
    pub dropped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelayForwardOutcome {
    report: ForwardReport,
    should_continue: bool,
}

impl RelayForwardOutcome {
    fn continue_with(report: ForwardReport) -> Self {
        Self {
            report,
            should_continue: true,
        }
    }

    fn close_with(report: ForwardReport) -> Self {
        Self {
            report,
            should_continue: false,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum RelayError {
    #[error("daemon control is not connected for server_id")]
    DaemonControlOffline,
    #[error("daemon control channel is backpressured")]
    DaemonControlBusy,
    #[error("daemon data route is missing client_id or data_token")]
    DaemonDataRouteInvalid,
    #[error("daemon data route does not match a pending client")]
    DaemonDataRouteRejected,
    #[error("relay state mutex poisoned")]
    Poisoned,
}

impl RelayError {
    fn route_error_code(&self) -> &'static str {
        match self {
            Self::DaemonControlOffline => "relay_daemon_offline",
            Self::DaemonControlBusy => "relay_busy",
            Self::DaemonDataRouteInvalid => "relay_data_route_invalid",
            Self::DaemonDataRouteRejected => "relay_data_route_rejected",
            Self::Poisoned => "relay_state_unavailable",
        }
    }

    fn route_error_message(&self) -> &'static str {
        match self {
            Self::DaemonControlOffline => {
                "relay daemon control is not connected; retry after daemon reconnects"
            }
            Self::DaemonControlBusy => "relay daemon control is busy; retry shortly",
            Self::DaemonDataRouteInvalid => "relay daemon data route is invalid",
            Self::DaemonDataRouteRejected => "relay daemon data route was rejected",
            Self::Poisoned => "relay state is temporarily unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutePrelude {
    server_id: ServerId,
    route_role: RouteRole,
    connection_role: ConnectionRole,
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

impl RelayRegistry {
    fn remove_room_if_empty(rooms: &mut HashMap<ServerId, RelayRoom>, server_id: ServerId) {
        if rooms.get(&server_id).is_some_and(RelayRoom::is_empty) {
            rooms.remove(&server_id);
        }
    }

    fn room_count(&self) -> usize {
        self.rooms
            .lock()
            .expect("relay registry mutex poisoned")
            .len()
    }

    fn register(
        &self,
        prelude: &RoutePrelude,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let server_id = prelude.server_id;
        let role = prelude.connection_role;
        let id = self.next_connection_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut rooms = self.rooms.lock().map_err(|_| RelayError::Poisoned)?;

        match role {
            ConnectionRole::DaemonControl => {
                let room = rooms.entry(server_id).or_default();
                let endpoint = ConnectionEndpoint::new(id, sender);
                if let Some(stale_control) = room.daemon_control.replace(endpoint) {
                    warn!(
                        server_id = %server_id.0,
                        stale_connection_id = stale_control.id,
                        new_connection_id = id,
                        "replacing stale relay daemon control"
                    );
                    stale_control.sender.request_close();
                    room.close_clients();
                }
                debug!(
                    server_id = %server_id.0,
                    connection_id = id,
                    client_count = room.clients.len(),
                    "relay registered daemon control"
                );
            }
            ConnectionRole::DaemonData => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonControlOffline)?;
                match (prelude.client_id, prelude.data_token.as_ref()) {
                    (Some(client_id), Some(data_token)) => {
                        let client_id = client_id.0;
                        let Some(client) = room.clients.get_mut(&client_id) else {
                            return Err(RelayError::DaemonDataRouteRejected);
                        };
                        if client.data_token.as_ref() != Some(data_token)
                            || client.paired_daemon_data_id.is_some()
                        {
                            return Err(RelayError::DaemonDataRouteRejected);
                        }
                        client.paired_daemon_data_id = Some(id);
                        client.pair_signal.notify_waiters();
                        room.daemon_data.insert(
                            id,
                            ConnectionEndpoint::new_daemon_data(id, sender, client_id),
                        );
                        debug!(
                            server_id = %server_id.0,
                            connection_id = id,
                            client_connection_id = client_id,
                            "relay registered daemon data pipe"
                        );
                    }
                    (None, None) => {
                        room.daemon_data
                            .insert(id, ConnectionEndpoint::new_idle_daemon_data(id, sender));
                        room.idle_daemon_data.push_back(id);
                        debug!(
                            server_id = %server_id.0,
                            connection_id = id,
                            idle_data_pipes = room.idle_daemon_data.len(),
                            "relay registered idle daemon data pipe"
                        );
                    }
                    _ => return Err(RelayError::DaemonDataRouteInvalid),
                }
            }
            ConnectionRole::Client => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonControlOffline)?;
                let data_token = Nonce(format!("relay-data-{}-{id}", uuid::Uuid::new_v4()));
                let control_sender = room
                    .daemon_control
                    .as_ref()
                    .ok_or(RelayError::DaemonControlOffline)?
                    .sender
                    .clone();
                let open_data = RelayControlEnvelope::OpenData {
                    client_id: RelayClientId(id),
                    data_token: data_token.clone(),
                };
                // 中文注释：必须先把 pending client 放进 room，再通知 daemon 反连 data。
                // 否则 control writer 足够快时，daemon data 可能早于 client 入表到达并被误拒。
                room.clients
                    .insert(id, ConnectionEndpoint::new_client(id, sender, data_token));
                let client_data_token = room
                    .clients
                    .get(&id)
                    .and_then(|client| client.data_token.clone())
                    .expect("刚插入的 client 必须带 data token");
                if let Some(data_id) = room.assign_idle_daemon_data_to_client(id, client_data_token)
                {
                    debug!(
                        server_id = %server_id.0,
                        connection_id = id,
                        daemon_data_connection_id = data_id,
                        client_count = room.clients.len(),
                        idle_data_pipes = room.idle_daemon_data.len(),
                        "relay paired client with idle daemon data pipe"
                    );
                    return Ok(ConnectionRegistration {
                        server_id,
                        role,
                        id,
                        paired_client_id: None,
                    });
                }
                match control_sender
                    .try_send_control(RelayOutbound::Frame(relay_control_frame(open_data)))
                {
                    Ok(()) => {
                        debug!(
                            server_id = %server_id.0,
                            connection_id = id,
                            client_count = room.clients.len(),
                            "relay registered pending client data pipe"
                        );
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        room.close_client_transport(id);
                        return Err(RelayError::DaemonControlBusy);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        room.close_client_transport(id);
                        room.clear_daemon_control_and_dependents();
                        return Err(RelayError::DaemonControlOffline);
                    }
                }
            }
        }

        Ok(ConnectionRegistration {
            server_id,
            role,
            id,
            paired_client_id: if role == ConnectionRole::DaemonData {
                prelude.client_id.map(|client_id| client_id.0)
            } else {
                None
            },
        })
    }

    fn unregister(&self, registration: &ConnectionRegistration) {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during unregister");
            return;
        };

        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return;
        };

        match registration.role {
            ConnectionRole::DaemonControl => {
                if room
                    .daemon_control
                    .as_ref()
                    .is_some_and(|daemon| daemon.id == registration.id)
                {
                    debug!(
                        server_id = %registration.server_id.0,
                        connection_id = registration.id,
                        client_count = room.clients.len(),
                        "relay unregistering daemon control"
                    );
                    room.clear_daemon_control_and_dependents();
                }
            }
            ConnectionRole::DaemonData => {
                let removed_data = room.daemon_data.remove(&registration.id);
                let removed = removed_data.is_some();
                room.idle_daemon_data
                    .retain(|data_id| *data_id != registration.id);
                let paired_client_id = removed_data
                    .as_ref()
                    .and_then(|data| data.paired_client_id)
                    .or(registration.paired_client_id);
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    paired_client_id,
                    "relay unregistering daemon data"
                );
                if let Some(data) = removed_data {
                    data.sender.request_close();
                }
                if removed
                    && let Some(client_id) = paired_client_id
                    && let Some(client) = room.clients.get(&client_id)
                    && client.paired_daemon_data_id == Some(registration.id)
                {
                    room.close_client_transport(client_id);
                }
            }
            ConnectionRole::Client => {
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    remaining_clients = room.clients.len(),
                    "relay unregistering client"
                );
                let close = room.close_client_transport(registration.id);
                // 中文注释：已配对 data pipe 直接收到 client_disconnected；尚未配对的
                // pending client 只能通过 control 线通知 daemon 取消冷启动 data 任务。
                if close.removed && !close.notified_data_pipe {
                    room.notify_client_disconnected_to_control(registration.id);
                }
            }
        }

        Self::remove_room_if_empty(&mut rooms, registration.server_id);
    }

    fn has_client(&self, server_id: ServerId, client_id: RelayClientId) -> bool {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client presence check");
            return false;
        };
        rooms
            .get(&server_id)
            .is_some_and(|room| room.clients.contains_key(&client_id.0))
    }

    #[cfg(test)]
    fn client_has_data_pair(&self, registration: &ConnectionRegistration) -> bool {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client pair status lookup");
            return false;
        };
        rooms
            .get(&registration.server_id)
            .and_then(|room| room.clients.get(&registration.id))
            .is_some_and(|client| client.paired_daemon_data_id.is_some())
    }

    async fn wait_client_data_pair(&self, registration: &ConnectionRegistration) -> bool {
        if registration.role != ConnectionRole::Client {
            return false;
        }
        loop {
            let pair_signal = {
                let Ok(rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during client pair wait");
                    return false;
                };
                let Some(room) = rooms.get(&registration.server_id) else {
                    return false;
                };
                let Some(client) = room.clients.get(&registration.id) else {
                    return false;
                };
                if client.paired_daemon_data_id.is_some() {
                    return true;
                }
                client.pair_signal.clone()
            };
            tokio::select! {
                _ = pair_signal.notified() => {}
                // 中文注释：Notify 不保存 notify_waiters 的历史事件；配对如果刚好发生在
                // clone signal 和 await 之间，短轮询能保证不会永远睡在冷启动上传路径上。
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            }
        }
    }

    async fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        match registration.role {
            ConnectionRole::DaemonControl => self.drop_control_payload(registration).await,
            ConnectionRole::DaemonData => {
                self.forward_daemon_data_to_client(registration, frame)
                    .await
            }
            ConnectionRole::Client => {
                self.forward_client_to_daemon_data(registration, frame)
                    .await
            }
        }
    }

    async fn drop_control_payload(&self, registration: &ConnectionRegistration) -> ForwardReport {
        debug!(
            server_id = %registration.server_id.0,
            connection_id = registration.id,
            "dropping payload received on relay daemon control line"
        );
        ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        }
    }

    async fn forward_client_to_daemon_data(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (daemon_data_id, daemon_data_sender) = {
            let Ok(rooms) = self.rooms.lock() else {
                warn!("relay registry mutex poisoned during client data forward");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(room) = rooms.get(&registration.server_id) else {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(client) = room.clients.get(&registration.id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            let Some(data_id) = client.paired_daemon_data_id else {
                drop(rooms);
                return self.buffer_unpaired_client_frame(registration, frame);
            };
            let Some(daemon_data) = room.daemon_data.get(&data_id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            (daemon_data.id, daemon_data.sender.clone())
        };

        // 中文注释：这里不能 await daemon data 队列腾挪；当前 client 的读循环会被卡住，
        // Close 帧也就无法被读取和 unregister。目标队列承压时直接关闭当前 client，
        // 让上游重连，而不是把旧 client 悬挂在 relay 内部。
        match daemon_data_sender.try_send_data(RelayOutbound::Frame(frame)) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during daemon data cleanup");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                if let Some(room) = rooms.get_mut(&registration.server_id)
                    && room
                        .daemon_data
                        .get(&daemon_data_id)
                        .is_some_and(|endpoint| endpoint.id == daemon_data_id)
                {
                    room.close_client_transport(registration.id);
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    async fn forward_client_to_daemon_data_backpressured(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (daemon_data_id, daemon_data_sender) =
            match self.client_daemon_data_sender(registration) {
                Some(pair) => pair,
                None => {
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                }
            };

        // 中文注释：HTTP tunnel 的 request body 在独立任务里读取，不会挡住 browser
        // WebSocket 的 Close/cleanup 读循环；文件体应直接承接 daemon data 线的真实背压。
        match daemon_data_sender
            .send_data(RelayOutbound::Frame(frame))
            .await
        {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                self.close_client_after_daemon_data_send_failure(registration, daemon_data_id)
            }
        }
    }

    fn client_daemon_data_sender(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<(u64, FrameSender)> {
        if registration.role != ConnectionRole::Client {
            return None;
        }
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client data sender lookup");
            return None;
        };
        let room = rooms.get(&registration.server_id)?;
        let client = room.clients.get(&registration.id)?;
        let data_id = client.paired_daemon_data_id?;
        let daemon_data = room.daemon_data.get(&data_id)?;
        Some((daemon_data.id, daemon_data.sender.clone()))
    }

    fn close_client_after_daemon_data_send_failure(
        &self,
        registration: &ConnectionRegistration,
        daemon_data_id: u64,
    ) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon data cleanup");
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        if let Some(room) = rooms.get_mut(&registration.server_id)
            && room
                .daemon_data
                .get(&daemon_data_id)
                .is_some_and(|endpoint| endpoint.id == daemon_data_id)
        {
            room.close_client_transport(registration.id);
        }
        Self::remove_room_if_empty(&mut rooms, registration.server_id);
        ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        }
    }

    fn buffer_unpaired_client_frame(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair client buffering");
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        match room.buffer_client_frame(registration.id, frame) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(_frame) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    max_frames = PRE_PAIR_CLIENT_BUFFER_MAX_FRAMES,
                    max_bytes = PRE_PAIR_CLIENT_BUFFER_MAX_BYTES,
                    "relay pre-pair client buffer full"
                );
                let close = room.close_client_transport(registration.id);
                if close.removed && !close.notified_data_pipe {
                    room.notify_client_disconnected_to_control(registration.id);
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    async fn flush_pre_pair_client_frames(&self, registration: &ConnectionRegistration) {
        if registration.role != ConnectionRole::DaemonData {
            return;
        }
        let Some(client_id) = registration.paired_client_id else {
            return;
        };
        let Some((frames, daemon_sender)) =
            self.take_pre_pair_client_frames(registration, client_id)
        else {
            return;
        };
        let frame_count = frames.len();
        if frame_count == 0 {
            return;
        }
        let mut delivered = 0_usize;
        for frame in frames {
            match daemon_sender.send_data(RelayOutbound::Frame(frame)).await {
                Ok(()) => delivered = delivered.saturating_add(1),
                Err(RelayDataSendError::BudgetFull) | Err(RelayDataSendError::Closed) => {
                    self.close_client_after_pre_pair_flush_failure(registration, client_id);
                    break;
                }
            }
        }
        debug!(
            server_id = %registration.server_id.0,
            daemon_data_connection_id = registration.id,
            client_connection_id = client_id,
            frames = frame_count,
            delivered,
            "relay flushed pre-pair client frames"
        );
    }

    fn take_pre_pair_client_frames(
        &self,
        registration: &ConnectionRegistration,
        client_id: ConnectionId,
    ) -> Option<(Vec<OpaqueFrame>, FrameSender)> {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair client flush");
            return None;
        };
        let room = rooms.get_mut(&registration.server_id)?;
        let daemon_sender = room.daemon_data.get(&registration.id)?.sender.clone();
        let frames = room.drain_pre_pair_client_frames(client_id)?;
        Some((frames, daemon_sender))
    }

    fn close_client_after_pre_pair_flush_failure(
        &self,
        registration: &ConnectionRegistration,
        client_id: ConnectionId,
    ) {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during pre-pair flush cleanup");
            return;
        };
        if let Some(room) = rooms.get_mut(&registration.server_id) {
            let close = room.close_client_transport(client_id);
            if close.removed && !close.notified_data_pipe {
                room.notify_client_disconnected_to_control(client_id);
            }
        }
        Self::remove_room_if_empty(&mut rooms, registration.server_id);
    }

    async fn forward_daemon_data_to_client(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let (client_id, client_sender) = {
            let Ok(rooms) = self.rooms.lock() else {
                warn!("relay registry mutex poisoned during daemon data forward");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(room) = rooms.get(&registration.server_id) else {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let Some(daemon_data) = room.daemon_data.get(&registration.id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            let Some(client_id) = daemon_data.paired_client_id else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            let Some(client) = room.clients.get(&client_id) else {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            if client.paired_daemon_data_id != Some(registration.id) {
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            }
            (client_id, client.sender.clone())
        };

        match client_sender.try_send_data(RelayOutbound::Frame(frame)) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(RelayDataSendError::Closed) | Err(RelayDataSendError::BudgetFull) => {
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during slow client data cleanup");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                if let Some(room) = rooms.get_mut(&registration.server_id) {
                    let close = room.close_client_transport(client_id);
                    if close.removed && !close.notified_data_pipe {
                        room.notify_client_disconnected_to_control(client_id);
                    }
                }
                Self::remove_room_if_empty(&mut rooms, registration.server_id);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn handle_daemon_data_control(
        &self,
        registration: &ConnectionRegistration,
        control: RelayControlEnvelope,
    ) -> ForwardReport {
        match control {
            RelayControlEnvelope::DataReady => {
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during daemon data ready");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                let ready = rooms
                    .get_mut(&registration.server_id)
                    .is_some_and(|room| room.mark_daemon_data_ready(registration.id));
                debug!(
                    server_id = %registration.server_id.0,
                    daemon_data_connection_id = registration.id,
                    ready,
                    "relay received daemon data ready"
                );
                ForwardReport {
                    attempted: 1,
                    delivered: if ready { 1 } else { 0 },
                    dropped: if ready { 0 } else { 1 },
                }
            }
            RelayControlEnvelope::OpenData { .. }
            | RelayControlEnvelope::ClientDisconnected { .. } => {
                warn!(
                    server_id = %registration.server_id.0,
                    daemon_data_connection_id = registration.id,
                    ?control,
                    "relay ignored unexpected daemon data control frame"
                );
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn handle_daemon_data_control_ping(
        &self,
        registration: &ConnectionRegistration,
        control: RelayControlEnvelope,
        pong_payload: Vec<u8>,
    ) -> RelayForwardOutcome {
        if !matches!(control, RelayControlEnvelope::DataReady) {
            return RelayForwardOutcome::continue_with(
                self.handle_daemon_data_control(registration, control),
            );
        }

        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon data ready");
            return RelayForwardOutcome::close_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        };
        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return RelayForwardOutcome::continue_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        };
        let ready = room.mark_daemon_data_ready(registration.id);
        debug!(
            server_id = %registration.server_id.0,
            daemon_data_connection_id = registration.id,
            ready,
            "relay received daemon data ready"
        );
        if !ready {
            return RelayForwardOutcome::continue_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        }
        let Some(daemon_data) = room.daemon_data.get(&registration.id) else {
            room.idle_daemon_data
                .retain(|data_id| *data_id != registration.id);
            return RelayForwardOutcome::continue_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            });
        };
        match daemon_data
            .sender
            .try_send_control(RelayOutbound::Pong(pong_payload))
        {
            Ok(()) => RelayForwardOutcome::continue_with(ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }),
            Err(error) => {
                warn!(
                    server_id = %registration.server_id.0,
                    daemon_data_connection_id = registration.id,
                    %error,
                    "relay failed to acknowledge daemon data ready"
                );
                room.idle_daemon_data
                    .retain(|data_id| *data_id != registration.id);
                if let Some(daemon_data) = room.daemon_data.remove(&registration.id) {
                    daemon_data.sender.request_close();
                }
                RelayForwardOutcome::close_with(ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                })
            }
        }
    }
}

fn relay_control_frame(envelope: RelayControlEnvelope) -> OpaqueFrame {
    // 中文注释：control 线只承载 relay transport 生命周期消息，不进入 E2EE 业务协议。
    OpaqueFrame::Text(
        serde_json::to_string(&envelope)
            .expect("relay control envelope should encode as JSON text"),
    )
}

fn relay_data_control_outbound(envelope: RelayControlEnvelope) -> RelayOutbound {
    // 中文注释：data 线只允许业务 text/binary 原样透传；transport 生命周期控制放进
    // WebSocket ping payload，避免和业务 JSON text 的 `type` 字段发生碰撞。
    RelayOutbound::Ping(
        encode_relay_data_control(&envelope)
            .expect("relay data control payload must fit in websocket control frame"),
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
    // Only the first frame is public routing metadata; payload frames after this stay opaque.
    let prelude = match timeout(ROUTE_PRELUDE_TIMEOUT, read_route_prelude(&mut socket)).await {
        Ok(Ok(prelude)) => prelude,
        Ok(Err(error)) => {
            if matches!(error, RoutePreludeError::UnsupportedLegacyDaemonMux) {
                let _ = send_route_error(
                    &mut socket,
                    "relay_legacy_route_rejected",
                    "legacy daemon mux route is no longer accepted; reconnect with daemon control and daemon data routes",
                )
                .await;
            }
            if route_prelude_error_is_noisy_client_disconnect(&error) {
                debug!(%error, "rejecting relay websocket before route registration");
            } else {
                warn!(%error, "rejecting relay websocket before route registration");
            }
            return;
        }
        Err(_) => {
            warn!(
                timeout_ms = ROUTE_PRELUDE_TIMEOUT.as_millis(),
                "relay route prelude timed out"
            );
            return;
        }
    };
    let server_id = prelude.server_id;
    let role = prelude.connection_role;
    let (tx, control_rx, data_rx) = FrameSender::channel(DATA_CHANNEL_CAPACITY);
    let mut endpoint_close_rx = tx.subscribe_close();
    let writer_close_rx = tx.subscribe_close();
    let data_budget = tx.data_budget.clone();
    let registration = match state.register_route(&prelude, tx) {
        Ok(registration) => registration,
        Err(error) => {
            warn!(server_id = %server_id.0, ?role, %error, "rejecting relay websocket");
            let _ = send_route_error(
                &mut socket,
                error.route_error_code(),
                error.route_error_message(),
            )
            .await;
            return;
        }
    };

    if role == ConnectionRole::Client {
        debug!(
            server_id = %server_id.0,
            connection_id = registration.id,
            "relay client route accepted before daemon data pipe paired"
        );
    }

    match timeout(
        WEBSOCKET_SEND_DEADLINE,
        send_route_ready(&mut socket, &prelude),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(server_id = %server_id.0, ?role, %error, "relay websocket route_ready failed");
            state.unregister(&registration);
            return;
        }
        Err(_) => {
            warn!(
                server_id = %server_id.0,
                ?role,
                timeout_ms = WEBSOCKET_SEND_DEADLINE.as_millis(),
                "relay websocket route_ready timed out"
            );
            state.unregister(&registration);
            return;
        }
    }

    debug!(
        server_id = %server_id.0,
        ?role,
        connection_id = registration.id,
        "relay websocket registered"
    );

    if role == ConnectionRole::DaemonData {
        state
            .inner
            .flush_pre_pair_client_frames(&registration)
            .await;
    }

    let (sender, mut receiver) = socket.split();
    let mut receive_debug = WebSocketReceiveDebug::new(Instant::now());
    let mut traffic = RelayConnectionTraffic::default();
    let (writer_outcome_tx, mut writer_outcome_rx) = mpsc::unbounded_channel();
    // 中文注释：relay 必须是 dumb pipe，但 transport 读写不能互相拖住。
    // 每条 WebSocket 的写侧单独跑，主循环只负责持续读取输入并转发到目标队列；
    // 这样慢 daemon/client 写不会阻塞本连接继续读取反方向的控制帧或新 client hello。
    //
    // outcome 只承载 close/failed 这类生命周期信号，不能按每个成功写出的 frame 回报。
    // 大输出时每帧回报会形成无界统计缓存；成功发送的细粒度日志由 writer 侧直接打印。
    let writer_task = tokio::spawn(run_relay_websocket_writer(
        sender,
        server_id,
        role,
        registration.id,
        control_rx,
        data_rx,
        data_budget,
        writer_close_rx,
        writer_outcome_tx,
    ));

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
                ).await;
                traffic.record_forward(forward_report.report);
                if !forward_report.should_continue {
                    break;
                }
            }
            outcome = writer_outcome_rx.recv() => {
                let Some(outcome) = outcome else {
                    break;
                };
                match outcome {
                    RelayWriterOutcome::Closed => {
                        debug!(
                            server_id = %server_id.0,
                            ?role,
                            connection_id = registration.id,
                            "relay websocket writer reported close"
                        );
                        break;
                    }
                    RelayWriterOutcome::Failed => {
                        warn!(
                            server_id = %server_id.0,
                            ?role,
                            connection_id = registration.id,
                            "relay websocket writer reported failure"
                        );
                        break;
                    }
                }
            }
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

fn route_prelude_error_is_noisy_client_disconnect(error: &RoutePreludeError) -> bool {
    match error {
        RoutePreludeError::Closed => true,
        RoutePreludeError::Receive(receive_error) => receive_error
            .to_string()
            .contains("Connection reset without closing handshake"),
        _ => false,
    }
}

fn log_websocket_receive_failed(
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    error: &axum::Error,
    receive_debug: &WebSocketReceiveDebug,
) {
    let error_text = error.to_string();
    if websocket_receive_failed_is_noisy_client_disconnect(role, &error_text) {
        debug!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            ?receive_debug,
            "relay websocket receive failed"
        );
    } else {
        warn!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            ?receive_debug,
            "relay websocket receive failed"
        );
    }
}

fn websocket_receive_failed_is_noisy_client_disconnect(
    role: ConnectionRole,
    error_text: &str,
) -> bool {
    role == ConnectionRole::Client
        && error_text.contains("Connection reset without closing handshake")
}

fn websocket_idle_ping_due(now: Instant, last_write_at: Instant) -> bool {
    now.duration_since(last_write_at) >= WEBSOCKET_IDLE_PING_INTERVAL
}

async fn send_relay_outbound(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
) -> RelayWriteResult {
    match prepare_relay_outbound(outbound) {
        PreparedRelayOutbound::Frame(frame) => {
            send_relay_opaque_frame(sender, server_id, role, connection_id, frame, channel).await
        }
        PreparedRelayOutbound::Ping(payload) => {
            match send_relay_established_message(
                sender,
                Message::Ping(payload),
                "relay websocket ping",
            )
            .await
            {
                Ok(()) => RelayWriteResult::Sent,
                Err(()) => RelayWriteResult::Failed,
            }
        }
        PreparedRelayOutbound::Pong(payload) => {
            match send_relay_established_message(
                sender,
                Message::Pong(payload),
                "relay websocket pong",
            )
            .await
            {
                Ok(()) => RelayWriteResult::Sent,
                Err(()) => RelayWriteResult::Failed,
            }
        }
        PreparedRelayOutbound::Close => {
            let _ = send_message_with_deadline(
                sender,
                Message::Close(None),
                WEBSOCKET_SEND_DEADLINE,
                "relay websocket close",
            )
            .await;
            RelayWriteResult::Closed
        }
    }
}

fn prepare_relay_outbound(outbound: RelayOutbound) -> PreparedRelayOutbound {
    match outbound {
        RelayOutbound::Frame(frame) => PreparedRelayOutbound::Frame(frame),
        RelayOutbound::Ping(payload) => PreparedRelayOutbound::Ping(payload),
        RelayOutbound::Pong(payload) => PreparedRelayOutbound::Pong(payload),
        RelayOutbound::Close => PreparedRelayOutbound::Close,
    }
}

async fn run_relay_websocket_writer(
    mut sender: futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    mut control_rx: mpsc::Receiver<RelayOutbound>,
    mut data_rx: mpsc::Receiver<RelayOutbound>,
    data_budget: Arc<DataQueueByteBudget>,
    mut close_rx: EndpointCloseReceiver,
    outcome_tx: mpsc::UnboundedSender<RelayWriterOutcome>,
) {
    let mut prefer_data_once = false;
    let mut last_write_at = Instant::now();
    let mut idle_ping = tokio::time::interval_at(
        Instant::now() + WEBSOCKET_IDLE_PING_INTERVAL,
        WEBSOCKET_IDLE_PING_INTERVAL,
    );
    idle_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut idle_ping_nonce = 0_u64;
    loop {
        if prefer_data_once {
            tokio::select! {
                biased;

                _ = close_rx.closed() => {
                    write_relay_close_and_report(
                        &mut sender,
                        &outcome_tx,
                    )
                    .await;
                    break;
                }
                outbound = data_rx.recv() => {
                    prefer_data_once = false;
                    let Some(outbound) = outbound else {
                        break;
                    };
                    let outbound_label = outbound.label();
                    let frame_kind = outbound.frame_kind();
                    let queued_bytes = outbound.queued_data_bytes();
                    data_budget.release(queued_bytes);
                    trace!(
                        server_id = %server_id.0,
                        ?role,
                        connection_id,
                        channel = "data",
                        outbound = outbound_label,
                        frame_kind,
                        queued_bytes,
                        "relay websocket writer dequeued frame"
                    );
                    if !write_relay_outbound_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        outbound,
                        "data",
                        &outcome_tx,
                    )
                    .await
                    {
                        break;
                    }
                    last_write_at = Instant::now();
                }
                outbound = control_rx.recv() => {
                    let Some(outbound) = outbound else {
                        break;
                    };
                    trace!(
                        server_id = %server_id.0,
                        ?role,
                        connection_id,
                        channel = "control",
                        outbound = outbound.label(),
                        frame_kind = outbound.frame_kind(),
                        queued_bytes = outbound.queued_data_bytes(),
                        "relay websocket writer dequeued frame"
                    );
                    prefer_data_once = true;
                    if !write_relay_outbound_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        outbound,
                        "control",
                        &outcome_tx,
                    )
                    .await
                    {
                        break;
                    }
                    last_write_at = Instant::now();
                }
                _ = idle_ping.tick() => {
                    if websocket_idle_ping_due(Instant::now(), last_write_at) {
                        if !write_relay_idle_ping_and_report(
                            &mut sender,
                            server_id,
                            role,
                            connection_id,
                            &mut idle_ping_nonce,
                            &outcome_tx,
                        )
                        .await
                        {
                            break;
                        }
                        last_write_at = Instant::now();
                    }
                }
            }
            continue;
        }

        tokio::select! {
            biased;

            _ = close_rx.closed() => {
                write_relay_close_and_report(
                    &mut sender,
                    &outcome_tx,
                )
                .await;
                break;
            }
            outbound = control_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id,
                    channel = "control",
                    outbound = outbound.label(),
                    frame_kind = outbound.frame_kind(),
                    queued_bytes = outbound.queued_data_bytes(),
                    "relay websocket writer dequeued frame"
                );
                prefer_data_once = true;
                if !write_relay_outbound_and_report(
                    &mut sender,
                    server_id,
                    role,
                    connection_id,
                    outbound,
                    "control",
                    &outcome_tx,
                )
                .await
                {
                    break;
                }
                last_write_at = Instant::now();
            }
            outbound = data_rx.recv() => {
                prefer_data_once = false;
                let Some(outbound) = outbound else {
                    break;
                };
                let outbound_label = outbound.label();
                let frame_kind = outbound.frame_kind();
                let queued_bytes = outbound.queued_data_bytes();
                data_budget.release(queued_bytes);
                trace!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id,
                    channel = "data",
                    outbound = outbound_label,
                    frame_kind,
                    queued_bytes,
                    "relay websocket writer dequeued frame"
                );
                if !write_relay_outbound_and_report(
                    &mut sender,
                    server_id,
                    role,
                    connection_id,
                    outbound,
                    "data",
                    &outcome_tx,
                )
                .await
                {
                    break;
                }
                last_write_at = Instant::now();
            }
            _ = idle_ping.tick() => {
                if websocket_idle_ping_due(Instant::now(), last_write_at) {
                    if !write_relay_idle_ping_and_report(
                        &mut sender,
                        server_id,
                        role,
                        connection_id,
                        &mut idle_ping_nonce,
                        &outcome_tx,
                    )
                    .await
                    {
                        break;
                    }
                    last_write_at = Instant::now();
                }
            }
        }
    }
}

async fn write_relay_close_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    outcome_tx: &mpsc::UnboundedSender<RelayWriterOutcome>,
) {
    // 中文注释：这是独立于 mpsc 队列的关闭路径；队列满时也能尽力发送 close frame，
    // 并通知读侧退出。真正的 socket 回收由 handle_socket drop/abort 兜底。
    let _ = send_message_with_deadline(
        sender,
        Message::Close(None),
        WEBSOCKET_SEND_DEADLINE,
        "relay websocket close signal",
    )
    .await;
    let _ = outcome_tx.send(RelayWriterOutcome::Closed);
}

async fn write_relay_idle_ping_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    idle_ping_nonce: &mut u64,
    outcome_tx: &mpsc::UnboundedSender<RelayWriterOutcome>,
) -> bool {
    *idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
    let payload = idle_ping_nonce.to_be_bytes().to_vec();
    // 中文注释：这是 WebSocket 控制帧保活，不进入 E2EE 业务协议。
    // relay 不等待业务 ACK，也不解析终端内容；ping 只用于让代理/NAT 看见连接活动。
    // 是否离线只能由底层 WebSocket read/write close/error 暴露，不能由 relay 自己计数裁定。
    match send_message_with_deadline(
        sender,
        Message::Ping(payload),
        WEBSOCKET_SEND_DEADLINE,
        "relay websocket idle ping",
    )
    .await
    {
        Ok(()) => {
            trace!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                "relay websocket idle ping sent"
            );
            true
        }
        Err(()) => {
            warn!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                "relay websocket idle ping failed"
            );
            let _ = outcome_tx.send(RelayWriterOutcome::Failed);
            false
        }
    }
}

async fn write_relay_outbound_and_report(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
    outcome_tx: &mpsc::UnboundedSender<RelayWriterOutcome>,
) -> bool {
    match send_relay_outbound(sender, server_id, role, connection_id, outbound, channel).await {
        RelayWriteResult::Sent => true,
        RelayWriteResult::Closed => {
            let _ = outcome_tx.send(RelayWriterOutcome::Closed);
            false
        }
        RelayWriteResult::Failed => {
            let _ = outcome_tx.send(RelayWriterOutcome::Failed);
            false
        }
    }
}

async fn send_relay_opaque_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    frame: OpaqueFrame,
    channel: &'static str,
) -> RelayWriteResult {
    let frame_kind = frame.kind();
    let frame_len = frame.len();
    let started_at = Instant::now();
    if send_relay_established_message(sender, frame.into(), "relay websocket outbound frame")
        .await
        .is_err()
    {
        warn!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            frame_kind,
            frame_len,
            channel,
            "relay websocket send failed"
        );
        return RelayWriteResult::Failed;
    }
    let elapsed = started_at.elapsed();
    match websocket_outbound_frame_pressure_level(frame_len, elapsed) {
        OutboundFramePressureLevel::Info => {
            info!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                frame_kind,
                frame_len,
                channel,
                elapsed_ms = elapsed.as_millis(),
                "relay websocket outbound frame pressure"
            );
        }
        OutboundFramePressureLevel::Debug => {
            debug!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                frame_kind,
                frame_len,
                channel,
                elapsed_ms = elapsed.as_millis(),
                "relay websocket outbound large frame"
            );
        }
        OutboundFramePressureLevel::None => {}
    }
    RelayWriteResult::Sent
}

async fn send_relay_established_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
    context: &'static str,
) -> Result<(), ()> {
    // relay 建连之后就是 dumb pipe：慢网络只应该形成 WebSocket/TCP 背压，
    // 不能由 relay 自己用短发送超时把仍然打开的连接判死。
    sender.send(message).await.map_err(|error| {
        warn!(%error, context = context, "relay websocket send failed");
    })
}

async fn read_route_prelude(socket: &mut WebSocket) -> Result<RoutePrelude, RoutePreludeError> {
    loop {
        let Some(message) = socket.next().await else {
            return Err(RoutePreludeError::Closed);
        };
        let message = message.map_err(RoutePreludeError::Receive)?;

        match message {
            Message::Text(raw) => {
                reject_oversized_frame(raw.len()).map_err(RoutePreludeError::TooLarge)?;
                return decode_route_prelude_from_str(&raw);
            }
            Message::Binary(raw) => {
                reject_oversized_frame(raw.len()).map_err(RoutePreludeError::TooLarge)?;
                return decode_route_prelude_from_slice(&raw);
            }
            Message::Ping(payload) => {
                timeout(WEBSOCKET_PONG_DEADLINE, socket.send(Message::Pong(payload)))
                    .await
                    .map_err(|_| RoutePreludeError::PongTimeout)?
                    .map_err(RoutePreludeError::Send)?
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(RoutePreludeError::Closed),
        }
    }
}

fn decode_route_prelude_from_str(raw: &str) -> Result<RoutePrelude, RoutePreludeError> {
    let envelope = serde_json::from_str::<Envelope<RouteHelloPayload>>(raw)?;
    decode_route_prelude(envelope)
}

fn decode_route_prelude_from_slice(raw: &[u8]) -> Result<RoutePrelude, RoutePreludeError> {
    let envelope = serde_json::from_slice::<Envelope<RouteHelloPayload>>(raw)?;
    decode_route_prelude(envelope)
}

fn decode_route_prelude(
    envelope: Envelope<RouteHelloPayload>,
) -> Result<RoutePrelude, RoutePreludeError> {
    if envelope.kind != MessageType::RouteHello {
        return Err(RoutePreludeError::UnexpectedType(envelope.kind));
    }

    // protocol_version, nonce, and timestamp_ms are carried for the protocol edge;
    // relay only uses server_id and role to place this socket into a route room.
    let route_role = envelope.payload.role;
    let connection_role = ConnectionRole::from_route_role(route_role)?;
    Ok(RoutePrelude {
        server_id: envelope.payload.server_id,
        route_role,
        connection_role,
        client_id: envelope.payload.client_id,
        data_token: envelope.payload.data_token,
    })
}

async fn send_route_ready(
    socket: &mut WebSocket,
    prelude: &RoutePrelude,
) -> Result<(), RoutePreludeError> {
    let ready = Envelope::new(
        MessageType::RouteReady,
        RouteReadyPayload {
            server_id: prelude.server_id,
            role: prelude.route_role,
        },
    );
    let raw = serde_json::to_string(&ready)?;
    socket
        .send(Message::Text(raw))
        .await
        .map_err(RoutePreludeError::Send)
}

async fn send_route_error(
    socket: &mut WebSocket,
    code: &'static str,
    message: &'static str,
) -> Result<(), RoutePreludeError> {
    let error = Envelope::new(
        MessageType::Error,
        ErrorPayload {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    );
    let raw = serde_json::to_string(&error)?;
    timeout(WEBSOCKET_SEND_DEADLINE, socket.send(Message::Text(raw)))
        .await
        .map_err(|_| RoutePreludeError::SendTimeout)?
        .map_err(RoutePreludeError::Send)
}

async fn handle_inbound_message(
    state: &RelayState,
    registration: &ConnectionRegistration,
    message: Message,
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
            forward_opaque(state, registration, OpaqueFrame::Text(text)).await
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
            forward_opaque(state, registration, OpaqueFrame::Binary(bytes)).await
        }
        Message::Ping(payload) => {
            if registration.role == ConnectionRole::DaemonData
                && let Some(control) = decode_relay_data_control(&payload)
            {
                // 中文注释：DataReady 的 pong 是 daemon 侧回收 idle data pipe 的确认点。
                // 必须在同一个 room 锁内完成 ready 入池和 pong 入队，避免新 client 抢先
                // 收到 OpenData，导致 daemon 还在等待 pong 时丢失 assignment。
                return state
                    .inner
                    .handle_daemon_data_control_ping(registration, control, payload);
            }
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

async fn queue_relay_pong_for_inbound_ping(
    state: &RelayState,
    registration: &ConnectionRegistration,
    payload: Vec<u8>,
) -> RelayForwardOutcome {
    let Ok(rooms) = state.inner.rooms.lock() else {
        warn!("relay registry mutex poisoned during ping pong enqueue");
        return RelayForwardOutcome::close_with(ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        });
    };
    let sender = rooms
        .get(&registration.server_id)
        .and_then(|room| match registration.role {
            ConnectionRole::DaemonControl => room.daemon_control.as_ref().and_then(|endpoint| {
                (endpoint.id == registration.id).then_some(endpoint.sender.clone())
            }),
            ConnectionRole::DaemonData => room
                .daemon_data
                .get(&registration.id)
                .map(|endpoint| endpoint.sender.clone()),
            ConnectionRole::Client => room
                .clients
                .get(&registration.id)
                .map(|endpoint| endpoint.sender.clone()),
        });
    drop(rooms);
    let Some(sender) = sender else {
        return RelayForwardOutcome::close_with(ForwardReport {
            attempted: 1,
            delivered: 0,
            dropped: 1,
        });
    };
    match sender.try_send_control(RelayOutbound::Pong(payload)) {
        Ok(()) => {
            trace!(
                server_id = %registration.server_id.0,
                ?registration.role,
                connection_id = registration.id,
                "relay queued pong for inbound ping"
            );
            RelayForwardOutcome::continue_with(ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            })
        }
        Err(error) => {
            warn!(
                server_id = %registration.server_id.0,
                ?registration.role,
                connection_id = registration.id,
                %error,
                "relay failed to queue pong for inbound ping"
            );
            RelayForwardOutcome::close_with(ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            })
        }
    }
}

fn reject_oversized_frame(len: usize) -> Result<(), usize> {
    // axum 的升级配置在 router 层；这里在 ws 层再做一次元数据大小闸门，避免继续转发超限 frame。
    let max = WEBSOCKET_MAX_FRAME_SIZE.min(WEBSOCKET_MAX_MESSAGE_SIZE);
    if len > max { Err(len) } else { Ok(()) }
}

async fn send_message_with_deadline(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
    deadline: Duration,
    context: &'static str,
) -> Result<(), ()> {
    match timeout(deadline, sender.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            warn!(%error, context = context, "relay websocket send failed");
            Err(())
        }
        Err(_) => {
            warn!(
                ?deadline,
                context = context,
                "relay websocket send timed out"
            );
            Err(())
        }
    }
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
    use futures_util::{SinkExt, StreamExt};
    use termd_proto::{
        ErrorPayload, Nonce, PROTOCOL_PACKET_VERSION, ProtocolVersion, RouteReadyPayload,
        UnixTimestampMillis,
    };
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    fn server_id(value: u128) -> ServerId {
        ServerId(uuid::Uuid::from_u128(value))
    }

    struct TestReceiver {
        control: mpsc::Receiver<RelayOutbound>,
        data: mpsc::Receiver<RelayOutbound>,
        data_budget: Arc<DataQueueByteBudget>,
    }

    impl TestReceiver {
        fn try_recv(&mut self) -> Result<RelayOutbound, TryRecvError> {
            match self.control.try_recv() {
                Ok(outbound) => Ok(outbound),
                Err(TryRecvError::Empty) => match self.data.try_recv() {
                    Ok(outbound) => {
                        self.data_budget.release(outbound.queued_data_bytes());
                        Ok(outbound)
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            }
        }
    }

    fn channel() -> (FrameSender, TestReceiver) {
        channel_with_data_capacity(DATA_CHANNEL_CAPACITY)
    }

    fn channel_with_control_capacity(control_capacity: usize) -> (FrameSender, TestReceiver) {
        let (control_tx, control_rx) = mpsc::channel(control_capacity);
        let (data_tx, data_rx) = mpsc::channel(DATA_CHANNEL_CAPACITY);
        let data_budget = Arc::new(DataQueueByteBudget::new(DATA_CHANNEL_BYTE_BUDGET));
        (
            FrameSender {
                control: control_tx,
                data: data_tx,
                data_budget: data_budget.clone(),
                close_signal: EndpointCloseSignal::new(),
            },
            TestReceiver {
                control: control_rx,
                data: data_rx,
                data_budget,
            },
        )
    }

    fn channel_with_data_capacity(data_capacity: usize) -> (FrameSender, TestReceiver) {
        let (sender, control, data) = FrameSender::channel(data_capacity);
        let data_budget = sender.data_budget.clone();
        (
            sender,
            TestReceiver {
                control,
                data,
                data_budget,
            },
        )
    }

    fn route_hello(
        server_id: ServerId,
        role: RouteRole,
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
                route_generation: None,
                client_id,
                data_token,
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        )
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
        socket
            .send(ClientMessage::Text(
                serde_json::to_string(&route_hello(server_id, role, client_id, data_token))
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
        loop {
            let next = timeout(wait, socket.next())
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
        loop {
            match socket.next().await?.unwrap() {
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
            RelayOutbound::Ping(payload) => decode_relay_data_control(&payload)
                .expect("expected relay data control ping payload"),
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
            axum::serve(listener, router(RelayState::default(), false))
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
            axum::serve(listener, router(RelayState::default(), false))
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
    async fn legacy_daemon_mux_route_is_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
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
            axum::serve(listener, router(RelayState::default(), false))
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
            axum::serve(listener, router(RelayState::default(), false))
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
            axum::serve(listener, router(RelayState::default(), false))
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
    fn relay_http_tunnel_deadline_applies_only_to_short_request_bodies() {
        assert_eq!(
            relay_http_tunnel_request_body_deadline("POST", "/api/files/upload/init"),
            RelayHttpTunnelRequestBodyDeadline::Whole(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("POST", "/api/files/download"),
            RelayHttpTunnelRequestBodyDeadline::Whole(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("POST", "/api/files/upload/abort"),
            RelayHttpTunnelRequestBodyDeadline::Whole(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
        );
        assert_eq!(
            relay_http_tunnel_request_body_deadline("POST", "/api/files/upload"),
            RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
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
            client_id: Some(client_id),
            data_token: Some(data_token),
        };
        let data = state.register_route(&data_prelude, data_tx).unwrap();

        assert_eq!(data.role, ConnectionRole::DaemonData);
        assert_eq!(data.paired_client_id, Some(client.id));
        assert!(state.client_has_data_pair(&client));
        assert!(state.has_client(server_id, RelayClientId(client.id)));
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
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        state
            .inner
            .flush_pre_pair_client_frames(&data_registration)
            .await;
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
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx.clone(),
            )
            .unwrap();
        state
            .inner
            .flush_pre_pair_client_frames(&data_registration)
            .await;
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
        let queued = data_rx.data.recv().await.unwrap();
        data_rx.data_budget.release(queued.queued_data_bytes());
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
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();
        state
            .inner
            .flush_pre_pair_client_frames(&data_registration)
            .await;
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
    async fn client_disconnect_does_not_requeue_closed_data_pipe() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        let mut data_close_rx = data_tx.subscribe_close();

        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        let daemon_data = state
            .register(server_id, ConnectionRole::DaemonData, data_tx)
            .unwrap();

        let (client_a_tx, _client_a_rx) = channel();
        let client_a = state
            .register(server_id, ConnectionRole::Client, client_a_tx)
            .unwrap();
        let RelayControlEnvelope::OpenData {
            client_id: client_a_id,
            ..
        } = decode_control(data_rx.try_recv().unwrap())
        else {
            panic!("expected idle daemon data assignment for first client");
        };
        assert_eq!(client_a_id, RelayClientId(client_a.id));
        assert_eq!(control_rx.try_recv().unwrap_err(), TryRecvError::Empty);

        state.unregister(&client_a);
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("client disconnect should close the old daemon data pipe");
        assert_eq!(data_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(
            handle_inbound_message(
                &state,
                &daemon_data,
                Message::Ping(encode_relay_data_control(&RelayControlEnvelope::DataReady).unwrap())
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
    async fn idle_data_assignment_is_ordered_before_first_client_frame() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, _control_rx) = channel();
        let (data_tx, mut data_rx) = channel();
        state
            .register(server_id, ConnectionRole::DaemonControl, control_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonData, data_tx)
            .unwrap();

        let (client_tx, _client_rx) = channel();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();
        let first_frame = OpaqueFrame::Binary(b"first-upload-request-head".to_vec());
        assert_eq!(
            state.forward_from(&client, first_frame.clone()).await,
            ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            }
        );

        // 中文注释：这个顺序是 HTTP upload 不再 0 速度/502 的关键不变量。
        // daemon idle data connection 必须先收到 OpenData，再收到 request head/body。
        let first = data_rx.data.recv().await.unwrap();
        let RelayControlEnvelope::OpenData { client_id, .. } = decode_control(first) else {
            panic!("expected open_data before first client frame");
        };
        assert_eq!(client_id, RelayClientId(client.id));
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
    async fn slow_client_data_queue_closes_client_and_data_pipe() {
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
                    client_id: Some(client_id),
                    data_token: Some(data_token),
                },
                data_tx,
            )
            .unwrap();

        client_tx
            .try_send_data(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let report = state
            .forward_from(&daemon_data, OpaqueFrame::Text("overflow".to_owned()))
            .await;

        assert_eq!(
            report,
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            }
        );
        assert_eq!(data_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(control_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        timeout(Duration::from_millis(50), client_close_rx.closed())
            .await
            .expect("slow client should be closed");
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("slow client cleanup should close paired daemon data pipe");
        assert!(!state.has_client(server_id, RelayClientId(client.id)));
        assert!(
            timeout(Duration::from_millis(30), control_close_rx.closed())
                .await
                .is_err()
        );
        assert_eq!(client_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned()))
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
