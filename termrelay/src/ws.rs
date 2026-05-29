use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, ErrorPayload, MessageType, Nonce, RelayClientId, RelayControlEnvelope,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
};
use thiserror::Error;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, timeout};
use tracing::{debug, info, trace, warn};

// 中文注释：relay 是 dumb pipe，不能长期替慢浏览器缓存终端流。
// 预算按 100ms 千兆链路的 BDP 量级设置；健康连接可以填满管道，
// 慢 client 仍会在预算耗尽后关闭并让前端重连拿 snapshot。
const DATA_CHANNEL_CAPACITY: usize = 32 * 1024;
const DATA_CHANNEL_BYTE_BUDGET: usize = 16 * 1024 * 1024;
const CONTROL_CHANNEL_CAPACITY: usize = 256;
// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WEBSOCKET_IDLE_PING_INTERVAL: Duration = Duration::from_millis(50);
pub(crate) const WEBSOCKET_MAX_FRAME_SIZE: usize = 1024 * 1024;
pub(crate) const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
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
    Pong(Vec<u8>),
    Close,
}

impl RelayOutbound {
    fn label(&self) -> &'static str {
        match self {
            Self::Frame(_) => "frame",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn frame_kind(&self) -> &'static str {
        match self {
            Self::Frame(frame) => frame.kind(),
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn queued_data_bytes(&self) -> usize {
        match self {
            Self::Frame(frame) => frame.len(),
            Self::Pong(_) | Self::Close => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedRelayOutbound {
    Frame(OpaqueFrame),
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

    fn client_pair_receiver(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<EndpointCloseReceiver> {
        self.inner.client_pair_receiver(registration)
    }

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
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
}

impl RelayRoom {
    fn is_empty(&self) -> bool {
        self.daemon_control.is_none() && self.daemon_data.is_empty() && self.clients.is_empty()
    }

    fn close_clients(&mut self) {
        for (_, client) in self.clients.drain() {
            // daemon control 不可用时，client 必须尽快收到 close，避免继续等待业务响应直到超时。
            if let Some(pair_signal) = client.pair_signal.as_ref() {
                pair_signal.close();
            }
            if let Some(data_id) = client.paired_daemon_data_id
                && let Some(daemon_data) = self.daemon_data.remove(&data_id)
            {
                daemon_data.sender.request_close();
            }
            client.sender.request_close();
        }
        for (_, daemon_data) in self.daemon_data.drain() {
            daemon_data.sender.request_close();
        }
    }

    fn clear_daemon_control_and_dependents(&mut self) {
        if let Some(daemon_control) = self.daemon_control.take() {
            // 中文注释：control 线是 daemon 是否在线的唯一裁定来源；它断开时，
            // 所有关联 browser/data transport 都必须关闭，让 browser 重新建链。
            daemon_control.sender.request_close();
        }
        self.close_clients();
    }

    fn close_client_transport(&mut self, client_id: ConnectionId) {
        if let Some(client) = self.clients.remove(&client_id) {
            if let Some(pair_signal) = client.pair_signal.as_ref() {
                pair_signal.close();
            }
            if let Some(data_id) = client.paired_daemon_data_id
                && let Some(daemon_data) = self.daemon_data.remove(&data_id)
            {
                daemon_data.sender.request_close();
            }
            client.sender.request_close();
        }
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
    pair_signal: Option<EndpointCloseSignal>,
}

impl ConnectionEndpoint {
    fn new(id: ConnectionId, sender: FrameSender) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: None,
            pair_signal: None,
        }
    }

    fn new_client(id: ConnectionId, sender: FrameSender, data_token: Nonce) -> Self {
        Self {
            id,
            sender,
            data_token: Some(data_token),
            paired_daemon_data_id: None,
            paired_client_id: None,
            pair_signal: Some(EndpointCloseSignal::new()),
        }
    }

    fn new_daemon_data(id: ConnectionId, sender: FrameSender, client_id: ConnectionId) -> Self {
        Self {
            id,
            sender,
            data_token: None,
            paired_daemon_data_id: None,
            paired_client_id: Some(client_id),
            pair_signal: None,
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
                let client_id = prelude
                    .client_id
                    .ok_or(RelayError::DaemonDataRouteInvalid)?
                    .0;
                let data_token = prelude
                    .data_token
                    .as_ref()
                    .ok_or(RelayError::DaemonDataRouteInvalid)?;
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonControlOffline)?;
                let Some(client) = room.clients.get_mut(&client_id) else {
                    return Err(RelayError::DaemonDataRouteRejected);
                };
                if client.data_token.as_ref() != Some(data_token)
                    || client.paired_daemon_data_id.is_some()
                {
                    return Err(RelayError::DaemonDataRouteRejected);
                }
                client.paired_daemon_data_id = Some(id);
                if let Some(pair_signal) = client.pair_signal.as_ref() {
                    pair_signal.close();
                }
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
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    paired_client_id = registration.paired_client_id,
                    "relay unregistering daemon data"
                );
                if let Some(data) = removed_data {
                    data.sender.request_close();
                }
                if removed
                    && let Some(client_id) = registration.paired_client_id
                    && let Some(client) = room.clients.get(&client_id)
                    && client.paired_daemon_data_id == Some(registration.id)
                {
                    room.close_client_transport(client_id);
                }
            }
            ConnectionRole::Client => {
                let removed = room.clients.contains_key(&registration.id);
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    remaining_clients = room.clients.len(),
                    "relay unregistering client"
                );
                room.close_client_transport(registration.id);
                // 中文注释：client WebSocket 断开就是这条 data pipe 的生命周期结束。
                // 通过 control 线通知 daemon，让它取消对应 data 连接上下文。
                if removed {
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

    fn client_pair_receiver(
        &self,
        registration: &ConnectionRegistration,
    ) -> Option<EndpointCloseReceiver> {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client pair receiver lookup");
            return None;
        };
        let room = rooms.get(&registration.server_id)?;
        let client = room.clients.get(&registration.id)?;
        client
            .pair_signal
            .as_ref()
            .map(EndpointCloseSignal::subscribe)
    }

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
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
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
                    room.close_client_transport(client_id);
                    room.notify_client_disconnected_to_control(client_id);
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
}

fn relay_control_frame(envelope: RelayControlEnvelope) -> OpaqueFrame {
    // 中文注释：control 线只承载 relay transport 生命周期消息，不进入 E2EE 业务协议。
    OpaqueFrame::Text(
        serde_json::to_string(&envelope)
            .expect("relay control envelope should encode as JSON text"),
    )
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
            warn!(%error, "rejecting relay websocket before route registration");
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
        let Some(mut pair_rx) = state.client_pair_receiver(&registration) else {
            state.unregister(&registration);
            let _ = send_route_error(
                &mut socket,
                "relay_data_route_invalid",
                "relay client data route state is missing",
            )
            .await;
            return;
        };
        if timeout(ROUTE_PRELUDE_TIMEOUT, pair_rx.closed())
            .await
            .is_err()
            || !state.client_has_data_pair(&registration)
        {
            warn!(
                server_id = %server_id.0,
                connection_id = registration.id,
                timeout_ms = ROUTE_PRELUDE_TIMEOUT.as_millis(),
                "relay client data route pairing timed out"
            );
            state.unregister(&registration);
            let _ = send_route_error(
                &mut socket,
                "relay_data_route_timeout",
                "relay daemon data route did not connect in time",
            )
            .await;
            return;
        }
        debug!(
            server_id = %server_id.0,
            connection_id = registration.id,
            "relay client route accepted after daemon data pipe paired"
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
    if elapsed >= Duration::from_millis(50) || frame_len >= 128 * 1024 {
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
            let Ok(rooms) = state.inner.rooms.lock() else {
                warn!("relay registry mutex poisoned during ping pong enqueue");
                return RelayForwardOutcome::close_with(ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                });
            };
            let sender =
                rooms
                    .get(&registration.server_id)
                    .and_then(|room| match registration.role {
                        ConnectionRole::DaemonControl => {
                            room.daemon_control.as_ref().and_then(|endpoint| {
                                (endpoint.id == registration.id).then_some(endpoint.sender.clone())
                            })
                        }
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
        let RelayOutbound::Frame(OpaqueFrame::Text(raw)) = outbound else {
            panic!("expected relay control text frame, got {outbound:?}");
        };
        serde_json::from_str(&raw).unwrap()
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
    async fn client_route_waits_for_daemon_data_and_then_raw_frames_are_piped() {
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
        assert!(
            timeout(Duration::from_millis(60), client.next())
                .await
                .is_err(),
            "client route_ready 必须等 daemon data 线完成配对后再发送"
        );

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
        assert_eq!(WEBSOCKET_MAX_FRAME_SIZE, 1024 * 1024);
        assert_eq!(WEBSOCKET_MAX_MESSAGE_SIZE, 4 * 1024 * 1024);
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
    async fn client_disconnect_notifies_control_and_closes_paired_data() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, _client_rx) = channel();
        let (data_tx, _data_rx) = channel();
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

        assert_eq!(
            decode_control(control_rx.try_recv().unwrap()),
            RelayControlEnvelope::ClientDisconnected { client_id }
        );
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("paired data pipe should close after client disconnect");
        assert!(!state.has_client(server_id, client_id));
        assert!(
            timeout(Duration::from_millis(30), control_close_rx.closed())
                .await
                .is_err()
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
    async fn slow_client_data_queue_closes_only_that_client_and_data_pipe() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (control_tx, mut control_rx) = channel();
        let (client_tx, mut client_rx) = channel_with_data_capacity(1);
        let (data_tx, _data_rx) = channel();
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
        assert_eq!(
            decode_control(control_rx.try_recv().unwrap()),
            RelayControlEnvelope::ClientDisconnected { client_id }
        );
        timeout(Duration::from_millis(50), client_close_rx.closed())
            .await
            .expect("slow client should be closed");
        timeout(Duration::from_millis(50), data_close_rx.closed())
            .await
            .expect("paired daemon data should be closed");
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
