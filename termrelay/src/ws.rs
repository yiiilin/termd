use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, ErrorPayload, MessageType, Nonce, RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId, decode_binary_relay_mux_envelope,
    encode_binary_relay_mux_envelope,
};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};

const DATA_CHANNEL_CAPACITY: usize = 1024;
const DATA_CHANNEL_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const CONTROL_CHANNEL_CAPACITY: usize = 256;
// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
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
    out_text: RelayTrafficBucket,
    out_binary: RelayTrafficBucket,
    out_ping: RelayTrafficBucket,
    out_pong: RelayTrafficBucket,
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

    fn record_outbound(&mut self, frame_kind: &'static str, frame_len: usize) {
        match frame_kind {
            "text" => self.out_text.record(frame_len),
            "binary" => self.out_binary.record(frame_len),
            "ping" => self.out_ping.record(frame_len),
            "pong" => self.out_pong.record(frame_len),
            _ => {}
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
            || !self.out_text.is_empty()
            || !self.out_binary.is_empty()
            || !self.out_ping.is_empty()
            || !self.out_pong.is_empty()
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

    fn try_send(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), mpsc::error::TrySendError<RelayOutbound>> {
        let queued_bytes = outbound.queued_data_bytes();
        let outbound_label = outbound.label();
        let frame_kind = outbound.frame_kind();
        if !self.data_budget.try_reserve(queued_bytes) {
            warn!(
                outbound = outbound_label,
                frame_kind, queued_bytes, "relay data queue byte budget exhausted"
            );
            return Err(mpsc::error::TrySendError::Full(outbound));
        }
        match self.data.try_send(outbound) {
            Ok(()) => {
                debug!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue accepted frame"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue full"
                );
                Err(mpsc::error::TrySendError::Full(outbound))
            }
            Err(mpsc::error::TrySendError::Closed(outbound)) => {
                self.data_budget.release(queued_bytes);
                warn!(
                    outbound = outbound_label,
                    frame_kind, queued_bytes, "relay data queue closed"
                );
                Err(mpsc::error::TrySendError::Closed(outbound))
            }
        }
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
                debug!(
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
struct DataQueueByteBudget {
    limit: usize,
    queued: AtomicUsize,
}

impl DataQueueByteBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            queued: AtomicUsize::new(0),
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
    DaemonMux,
    Client,
}

impl ConnectionRole {
    fn from_route_role(role: RouteRole) -> Self {
        match role {
            RouteRole::Client => Self::Client,
            RouteRole::DaemonMux => Self::DaemonMux,
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
    MuxClientFrame {
        client_id: RelayClientId,
        frame: OpaqueFrame,
    },
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

impl RelayOutbound {
    fn label(&self) -> &'static str {
        match self {
            Self::Frame(_) => "frame",
            Self::MuxClientFrame { .. } => "mux_client_frame",
            Self::Ping(_) => "ping",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn frame_kind(&self) -> &'static str {
        match self {
            Self::Frame(frame) | Self::MuxClientFrame { frame, .. } => frame.kind(),
            Self::Ping(_) => "ping",
            Self::Pong(_) => "pong",
            Self::Close => "close",
        }
    }

    fn queued_data_bytes(&self) -> usize {
        match self {
            Self::Frame(frame) => frame.len(),
            Self::MuxClientFrame { frame, .. } => frame.len(),
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
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SentRelayOutbound {
    frame_kind: &'static str,
    frame_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayWriteResult {
    Sent(SentRelayOutbound),
    Dropped,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayWriterOutcome {
    Sent(SentRelayOutbound),
    Closed,
    Failed,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct WebSocketHeartbeatPendingSnapshot {
    pending_for_ms: u64,
    since_last_inbound_ms: u64,
    since_last_outbound_ms: u64,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
    inbound_messages_since_ping: u64,
    inbound_bytes_since_ping: u64,
    outbound_messages_since_ping: u64,
    outbound_bytes_since_ping: u64,
}

#[derive(Debug, Clone, Copy)]
struct WebSocketHeartbeatDebug {
    last_inbound_at: Instant,
    last_outbound_at: Instant,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
    pending_ping_sent_at: Option<Instant>,
    inbound_messages_since_ping: u64,
    inbound_bytes_since_ping: u64,
    outbound_messages_since_ping: u64,
    outbound_bytes_since_ping: u64,
}

impl WebSocketHeartbeatDebug {
    fn new(now: Instant) -> Self {
        Self {
            last_inbound_at: now,
            last_outbound_at: now,
            last_inbound_kind: "none",
            last_outbound_kind: "none",
            pending_ping_sent_at: None,
            inbound_messages_since_ping: 0,
            inbound_bytes_since_ping: 0,
            outbound_messages_since_ping: 0,
            outbound_bytes_since_ping: 0,
        }
    }

    fn record_inbound(&mut self, kind: &'static str, bytes: usize) {
        let now = Instant::now();
        self.last_inbound_at = now;
        self.last_inbound_kind = kind;
        if self.pending_ping_sent_at.is_some() {
            self.inbound_messages_since_ping = self.inbound_messages_since_ping.saturating_add(1);
            self.inbound_bytes_since_ping =
                self.inbound_bytes_since_ping.saturating_add(bytes as u64);
        }
    }

    fn record_outbound(&mut self, kind: &'static str, bytes: usize) {
        let now = Instant::now();
        self.last_outbound_at = now;
        self.last_outbound_kind = kind;
        if self.pending_ping_sent_at.is_some() {
            self.outbound_messages_since_ping = self.outbound_messages_since_ping.saturating_add(1);
            self.outbound_bytes_since_ping =
                self.outbound_bytes_since_ping.saturating_add(bytes as u64);
        }
    }

    fn note_ping_sent(&mut self) {
        let now = Instant::now();
        self.last_outbound_at = now;
        self.last_outbound_kind = "ping";
        self.pending_ping_sent_at = Some(now);
        self.inbound_messages_since_ping = 0;
        self.inbound_bytes_since_ping = 0;
        self.outbound_messages_since_ping = 0;
        self.outbound_bytes_since_ping = 0;
    }

    fn note_pong_received(&mut self) {
        self.pending_ping_sent_at = None;
        self.inbound_messages_since_ping = 0;
        self.inbound_bytes_since_ping = 0;
        self.outbound_messages_since_ping = 0;
        self.outbound_bytes_since_ping = 0;
    }

    fn pending_snapshot(&self) -> Option<WebSocketHeartbeatPendingSnapshot> {
        let ping_sent_at = self.pending_ping_sent_at?;
        Some(WebSocketHeartbeatPendingSnapshot {
            pending_for_ms: ping_sent_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            since_last_inbound_ms: self
                .last_inbound_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            since_last_outbound_ms: self
                .last_outbound_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            last_inbound_kind: self.last_inbound_kind,
            last_outbound_kind: self.last_outbound_kind,
            inbound_messages_since_ping: self.inbound_messages_since_ping,
            inbound_bytes_since_ping: self.inbound_bytes_since_ping,
            outbound_messages_since_ping: self.outbound_messages_since_ping,
            outbound_bytes_since_ping: self.outbound_bytes_since_ping,
        })
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

fn websocket_heartbeat_enabled(role: ConnectionRole) -> bool {
    // 中文注释：relay 是 dumb pipe，不能用自己的心跳策略裁决 browser/daemon 是否“在线”。
    // daemon mux 的长连接由 daemon 主动发标准 WebSocket Ping 保活；browser client 则以
    // TCP/WebSocket 实际 close、读写失败或队列背压作为清理信号。后台标签页、手机浏览器和
    // 公网代理都可能延迟 Pong，如果 relay 主动要求 Pong，会把仍可恢复的连接误杀成超时。
    let _ = role;
    false
}

fn websocket_idle_timeout_enabled(role: ConnectionRole) -> bool {
    // 中文注释：同上，relay 只按真实 transport 事件清理连接，不按“多久没业务帧”清理。
    // 终端会话可以长时间静默；browser 从后台回来后应该复用或重新建立连接，而不是被 relay
    // 的固定 idle timer 提前关闭并造成前端看到操作超时。
    let _ = role;
    false
}

impl From<OpaqueFrame> for RelayOpaqueFrame {
    fn from(frame: OpaqueFrame) -> Self {
        match frame {
            OpaqueFrame::Text(data) => Self::Text { data },
            OpaqueFrame::Binary(data) => Self::Binary {
                data_base64: general_purpose::STANDARD.encode(data),
            },
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

fn opaque_frame_from_mux(frame: RelayOpaqueFrame) -> Result<OpaqueFrame, RelayMuxFrameError> {
    match frame {
        RelayOpaqueFrame::Text { data } => Ok(OpaqueFrame::Text(data)),
        RelayOpaqueFrame::Binary { data_base64 } => general_purpose::STANDARD
            .decode(data_base64)
            .map(OpaqueFrame::Binary)
            .map_err(RelayMuxFrameError::InvalidBase64),
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
        route_generation: Option<Nonce>,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let prelude = RoutePrelude {
            server_id,
            route_role: match role {
                ConnectionRole::DaemonMux => RouteRole::DaemonMux,
                ConnectionRole::Client => RouteRole::Client,
            },
            connection_role: role,
            route_generation,
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
        let generation = if matches!(role, ConnectionRole::DaemonMux) {
            Some(Nonce("test-route-generation".to_owned()))
        } else {
            None
        };
        self.register_with_generation(server_id, role, generation, sender)
    }

    fn unregister(&self, registration: &ConnectionRegistration) {
        self.inner.unregister(registration);
    }

    fn has_client(&self, server_id: ServerId, client_id: RelayClientId) -> bool {
        self.inner.has_client(server_id, client_id)
    }

    fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        self.inner.forward_from(registration, frame)
    }
}

#[derive(Debug, Default)]
struct RelayRegistry {
    rooms: Mutex<HashMap<ServerId, RelayRoom>>,
    next_connection_id: AtomicU64,
}

#[derive(Debug, Default)]
struct RelayRoom {
    daemon_mux: Option<ConnectionEndpoint>,
    daemon_mux_generation: Option<Nonce>,
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
    disconnect_notify_pending: HashSet<ConnectionId>,
}

impl RelayRoom {
    fn close_clients(&mut self) {
        for (_, client) in self.clients.drain() {
            // daemon mux 已不可用时，client 必须尽快收到 close，避免继续等待业务响应直到超时。
            client.sender.request_close();
        }
    }

    fn clear_daemon_mux_and_dependents(&mut self) {
        if let Some(daemon_mux) = self.daemon_mux.take() {
            // 中文注释：daemon mux 从 room 移除时必须同时终止它自己的 endpoint。
            // 只把 room.daemon_mux 置空会留下假活 WebSocket，control 队列满时尤其明显。
            daemon_mux.sender.request_close();
        }
        self.daemon_mux_generation = None;
        self.disconnect_notify_pending.clear();
        self.close_clients();
    }

    fn notify_client_disconnected_or_remember(&mut self, client_id: ConnectionId) {
        let Some(daemon_mux) = self.daemon_mux.as_ref() else {
            return;
        };
        if notify_daemon_mux_client_disconnected(daemon_mux, client_id).is_ok() {
            self.disconnect_notify_pending.remove(&client_id);
        } else {
            // 中文注释：relay 不能因为旧 client 的断开通知发送失败而关闭 daemon mux，
            // 但也不能永久忘掉这件事。记录 tombstone 后，若 daemon 继续向该 client
            // 输出，relay 会在控制队列恢复后重试 ClientDisconnected。
            self.disconnect_notify_pending.insert(client_id);
        }
    }

    fn retry_pending_client_disconnect(&mut self, client_id: ConnectionId) {
        if !self.disconnect_notify_pending.contains(&client_id) {
            return;
        }
        self.notify_client_disconnected_or_remember(client_id);
    }
}

#[derive(Debug, Clone)]
struct ConnectionEndpoint {
    id: ConnectionId,
    sender: FrameSender,
    route_generation: Option<Nonce>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionRegistration {
    server_id: ServerId,
    role: ConnectionRole,
    id: ConnectionId,
    route_generation: Option<Nonce>,
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
    #[error("daemon mux is not connected for server_id")]
    DaemonMuxOffline,
    #[error("daemon mux control channel is backpressured")]
    DaemonMuxBusy,
    #[error("relay state mutex poisoned")]
    Poisoned,
}

impl RelayError {
    fn route_error_code(&self) -> &'static str {
        match self {
            Self::DaemonMuxOffline => "relay_daemon_offline",
            Self::DaemonMuxBusy => "relay_busy",
            Self::Poisoned => "relay_state_unavailable",
        }
    }

    fn route_error_message(&self) -> &'static str {
        match self {
            Self::DaemonMuxOffline => {
                "relay daemon mux is not connected; retry after daemon reconnects"
            }
            Self::DaemonMuxBusy => "relay daemon mux is busy; retry shortly",
            Self::Poisoned => "relay state is temporarily unavailable",
        }
    }
}

#[derive(Debug, Error)]
enum RelayMuxFrameError {
    #[error("relay mux envelope is invalid")]
    InvalidEnvelope,
    #[error("relay mux frame binary payload is not valid base64")]
    InvalidBase64(#[source] base64::DecodeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutePrelude {
    server_id: ServerId,
    route_role: RouteRole,
    connection_role: ConnectionRole,
    route_generation: Option<Nonce>,
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
}

impl RelayRegistry {
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
        let route_generation = prelude.route_generation.clone();
        let id = self.next_connection_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut rooms = self.rooms.lock().map_err(|_| RelayError::Poisoned)?;

        match role {
            ConnectionRole::DaemonMux => {
                let room = rooms.entry(server_id).or_default();
                let endpoint = ConnectionEndpoint {
                    id,
                    sender,
                    route_generation: route_generation.clone(),
                };
                if let Some(stale_mux) = room.daemon_mux.replace(endpoint) {
                    warn!(
                        server_id = %server_id.0,
                        stale_connection_id = stale_mux.id,
                        new_connection_id = id,
                        "replacing stale relay daemon mux"
                    );
                    // 新 mux 已经到达时，旧 mux 多半是半断连接；关闭旧 mux 和旧 clients，
                    // 让浏览器按统一重连路径重新完成 E2EE 握手，避免复用旧 client_id。
                    stale_mux.sender.request_close();
                    room.close_clients();
                }
                room.daemon_mux_generation = route_generation.clone();
                debug!(
                    server_id = %server_id.0,
                    connection_id = id,
                    client_count = room.clients.len(),
                    "relay registered daemon mux"
                );
            }
            ConnectionRole::Client => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonMuxOffline)?;
                if room.daemon_mux.is_none() {
                    return Err(RelayError::DaemonMuxOffline);
                }
                room.clients.insert(
                    id,
                    ConnectionEndpoint {
                        id,
                        sender,
                        route_generation: None,
                    },
                );
                debug!(
                    server_id = %server_id.0,
                    connection_id = id,
                    client_count = room.clients.len(),
                    "relay registered client"
                );
            }
        }

        Ok(ConnectionRegistration {
            server_id,
            role,
            id,
            route_generation,
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
            ConnectionRole::DaemonMux => {
                if room
                    .daemon_mux
                    .as_ref()
                    .is_some_and(|daemon| daemon.id == registration.id)
                {
                    debug!(
                        server_id = %registration.server_id.0,
                        connection_id = registration.id,
                        client_count = room.clients.len(),
                        "relay unregistering daemon mux"
                    );
                    room.clear_daemon_mux_and_dependents();
                }
            }
            ConnectionRole::Client => {
                room.clients.remove(&registration.id);
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    remaining_clients = room.clients.len(),
                    "relay unregistering client"
                );
                // 中文注释：旧 client 的断开通知不能反向摧毁整个 room。
                // control 队列满说明 daemon mux 正在承压；relay 作为 dumb pipe 只移除
                // 当前 client，并记录待通知 tombstone，避免 stale client 继续占用主干。
                room.notify_client_disconnected_or_remember(registration.id);
            }
        }

        if room.daemon_mux.is_none() && room.clients.is_empty() {
            rooms.remove(&registration.server_id);
        }
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

    fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        match registration.role {
            ConnectionRole::DaemonMux => self.forward_mux_to_client(registration, frame),
            ConnectionRole::Client => self.forward_client_to_mux_daemon(registration, frame),
        }
    }

    fn forward_client_to_mux_daemon(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during client mux forward");
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        let Some(room) = rooms.get_mut(&registration.server_id) else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        if !room.clients.contains_key(&registration.id) {
            debug!(
                server_id = %registration.server_id.0,
                connection_id = registration.id,
                "dropping frame from relay client that is no longer registered"
            );
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        }

        let Some(daemon_mux) = room.daemon_mux.as_ref() else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        let frame_kind = frame.kind();
        let frame_len = frame.len();
        match daemon_mux.sender.try_send(RelayOutbound::MuxClientFrame {
            client_id: RelayClientId(registration.id),
            frame,
        }) {
            Ok(()) => {
                debug!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    daemon_connection_id = daemon_mux.id,
                    frame_kind,
                    frame_len,
                    "relay forwarded client frame to daemon mux queue"
                );
                ForwardReport {
                    attempted: 1,
                    delivered: 1,
                    dropped: 0,
                }
            }
            Err(mpsc::error::TrySendError::Full(RelayOutbound::MuxClientFrame {
                client_id,
                ..
            })) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = registration.id,
                    daemon_connection_id = daemon_mux.id,
                    "dropping relay client because daemon mux control queue is full"
                );
                if let Some(client) = room.clients.remove(&registration.id) {
                    client.sender.request_close();
                }
                // data 队列满只是 daemon 暂时消费不过来，不能把主干判为离线；
                // 只通知 daemon 清理当前 client，其他 client 等 mux 恢复消费后继续工作。
                room.notify_client_disconnected_or_remember(client_id.0);
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            },
            Err(mpsc::error::TrySendError::Closed(_outbound)) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = daemon_mux.id,
                    "dropping offline relay daemon mux"
                );
                room.clear_daemon_mux_and_dependents();
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn forward_mux_to_client(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        // 中文注释：mux envelope 和内部 binary payload 可能很大，必须在 registry 全局锁外解析。
        // relay 锁只用于查当前 room/endpoint 和 clone sender，避免大输出阻塞其它 client 输入或注册。
        let envelope = match mux_envelope_from_opaque_frame(frame) {
            Ok(envelope) => envelope,
            Err(error) => {
                warn!(server_id = %registration.server_id.0, %error, "rejecting invalid relay mux envelope");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 1,
                };
            }
        };
        let (client_id, frame) = match envelope {
            RelayMuxEnvelope::Keepalive { .. } | RelayMuxEnvelope::KeepaliveAck { .. } => {
                // relay 不维护 daemon 心跳状态；标准 WebSocket Ping/Pong 由传输层处理。
                // 旧 mux keepalive 帧到达时只丢弃，避免 relay 重新承担保活协议角色。
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 0,
                };
            }
            RelayMuxEnvelope::DaemonFrame { client_id, frame } => (client_id, frame),
            RelayMuxEnvelope::ClientConnected { .. }
            | RelayMuxEnvelope::ClientDisconnected { .. }
            | RelayMuxEnvelope::ClientFrame { .. } => {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 1,
                };
            }
        };
        let target_sender = {
            let Ok(mut rooms) = self.rooms.lock() else {
                warn!("relay registry mutex poisoned during daemon mux forward");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };

            let Some(room) = rooms.get_mut(&registration.server_id) else {
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 0,
                };
            };
            let active = match registration.role {
                ConnectionRole::DaemonMux => room.daemon_mux.as_ref().is_some_and(|endpoint| {
                    endpoint.id == registration.id
                        && endpoint.route_generation.as_ref()
                            == registration.route_generation.as_ref()
                }),
                ConnectionRole::Client => false,
            };
            if !active {
                debug!(
                    server_id = %registration.server_id.0,
                    ?registration.role,
                    connection_id = registration.id,
                    "dropping frame from stale relay daemon mux connection"
                );
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            }
            let Some(client) = room.clients.get(&client_id.0) else {
                room.retry_pending_client_disconnect(client_id.0);
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            };
            client.sender.clone()
        };

        let frame = match opaque_frame_from_mux(frame) {
            Ok(frame) => frame,
            Err(error) => {
                warn!(server_id = %registration.server_id.0, %error, "rejecting invalid relay mux frame");
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            }
        };

        match target_sender.try_send(RelayOutbound::Frame(frame)) {
            Ok(()) => {
                debug!(
                    server_id = %registration.server_id.0,
                    daemon_connection_id = registration.id,
                    client_connection_id = client_id.0,
                    "relay forwarded daemon mux frame to client queue"
                );
                ForwardReport {
                    attempted: 1,
                    delivered: 1,
                    dropped: 0,
                }
            }
            Err(error) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = client_id.0,
                    %error,
                    "dropping slow relay mux client"
                );
                let Ok(mut rooms) = self.rooms.lock() else {
                    warn!("relay registry mutex poisoned during slow client cleanup");
                    return ForwardReport {
                        attempted: 1,
                        delivered: 0,
                        dropped: 1,
                    };
                };
                if let Some(room) = rooms.get_mut(&registration.server_id) {
                    if let Some(client) = room.clients.remove(&client_id.0) {
                        // 中文注释：慢 client 从 room 移除后必须主动关闭自己的 socket。
                        // 否则它还能继续把输入帧塞向 daemon mux，形成“已下线 client 占用主干”的假活连接。
                        client.sender.request_close();
                    }
                    room.notify_client_disconnected_or_remember(client_id.0);
                }
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }
}

fn notify_daemon_mux_client_disconnected(
    daemon_mux: &ConnectionEndpoint,
    client_id: ConnectionId,
) -> Result<(), ()> {
    let envelope = RelayMuxEnvelope::ClientDisconnected {
        client_id: RelayClientId(client_id),
    };
    daemon_mux
        .sender
        .try_send_control(RelayOutbound::Frame(mux_envelope_frame(envelope)))
        .map(|()| {
            debug!(
                daemon_connection_id = daemon_mux.id,
                client_connection_id = client_id,
                "relay queued client disconnect notification to daemon mux"
            );
        })
        .map_err(|error| {
            warn!(
                daemon_connection_id = daemon_mux.id,
                client_connection_id = client_id,
                %error,
                "failed to notify daemon mux about relay client disconnect"
            );
        })
}

pub async fn handle_socket(mut socket: WebSocket, state: RelayState) {
    // Only the first frame is public routing metadata; payload frames after this stay opaque.
    let prelude = match timeout(ROUTE_PRELUDE_TIMEOUT, read_route_prelude(&mut socket)).await {
        Ok(Ok(prelude)) => prelude,
        Ok(Err(error)) => {
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
    let self_sender = tx.clone();
    let mut endpoint_close_rx = self_sender.subscribe_close();
    let writer_close_rx = self_sender.subscribe_close();
    let data_budget = self_sender.data_budget.clone();
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
        if let Err(error) = notify_mux_client_connected(&state, &registration) {
            warn!(
                server_id = %server_id.0,
                ?role,
                connection_id = registration.id,
                %error,
                "rejecting relay websocket after daemon mux notify failed"
            );
            state.unregister(&registration);
            let _ = send_route_error(
                &mut socket,
                error.route_error_code(),
                error.route_error_message(),
            )
            .await;
            return;
        }
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
    let mut idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
    let mut heartbeat = tokio::time::interval_at(
        Instant::now() + WEBSOCKET_HEARTBEAT_INTERVAL,
        WEBSOCKET_HEARTBEAT_INTERVAL,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let heartbeat_enabled = websocket_heartbeat_enabled(role);
    let idle_timeout_enabled = websocket_idle_timeout_enabled(role);
    let mut pending_pong_deadline: Option<Instant> = None;
    let mut ping_enqueued = false;
    let mut heartbeat_debug = WebSocketHeartbeatDebug::new(Instant::now());
    let mut traffic = RelayConnectionTraffic::default();
    let (writer_outcome_tx, mut writer_outcome_rx) = mpsc::unbounded_channel();
    // 中文注释：relay 必须是 dumb pipe，但 transport 读写不能互相拖住。
    // 每条 WebSocket 的写侧单独跑，主循环只负责持续读取输入并转发到目标队列；
    // 这样慢 daemon/client 写不会阻塞本连接继续读取反方向的控制帧或新 client hello。
    let writer_task = tokio::spawn(run_relay_websocket_writer(
        state.clone(),
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
        let pending_pong_deadline_snapshot = pending_pong_deadline;
        // 写侧由 writer task 消费；这里持续读入站帧，避免慢写把反方向输入也卡住。
        // 中文注释：writer outcome 在大输出期间可能持续就绪。它只能更新统计和心跳状态，
        // 不能排在 inbound 前面，否则 relay client 的输入/close 会被输出完成通知饿住。
        tokio::select! {
            biased;

            _ = tokio::time::sleep_until(idle_deadline), if idle_timeout_enabled => {
                warn!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    "relay websocket idle timeout"
                );
                break;
            }
            _ = async move {
                if let Some(deadline) = pending_pong_deadline_snapshot {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if heartbeat_enabled => {
                warn!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    heartbeat_debug = ?heartbeat_debug.pending_snapshot(),
                    "relay websocket pong timed out"
                );
                break;
            }
            _ = endpoint_close_rx.closed() => {
                debug!(
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
                            &heartbeat_debug,
                        );
                        break;
                    }
                };
                idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
                traffic.record_inbound(&inbound);
                heartbeat_debug.record_inbound(
                    websocket_message_kind(&inbound),
                    websocket_message_bytes(&inbound),
                );
                debug!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    message_kind = websocket_message_kind(&inbound),
                    message_bytes = websocket_message_bytes(&inbound),
                    "relay websocket inbound frame received"
                );
                let is_pong = matches!(inbound, Message::Pong(_));

                let forward_report = handle_inbound_message(
                    &state,
                    &registration,
                    inbound,
                );
                traffic.record_forward(forward_report.report);
                if !forward_report.should_continue {
                    break;
                }
                if is_pong {
                    pending_pong_deadline = None;
                    heartbeat_debug.note_pong_received();
                }
            }
            outcome = writer_outcome_rx.recv() => {
                let Some(outcome) = outcome else {
                    break;
                };
                match outcome {
                    RelayWriterOutcome::Sent(sent) => {
                        debug!(
                            server_id = %server_id.0,
                            ?role,
                            connection_id = registration.id,
                            frame_kind = sent.frame_kind,
                            frame_len = sent.frame_len,
                            "relay websocket writer reported sent frame"
                        );
                        traffic.record_outbound(sent.frame_kind, sent.frame_len);
                        heartbeat_debug.record_outbound(sent.frame_kind, sent.frame_len);
                        if sent.frame_kind == "ping" {
                            ping_enqueued = false;
                            heartbeat_debug.note_ping_sent();
                            pending_pong_deadline = Some(Instant::now() + WEBSOCKET_PONG_DEADLINE);
                        }
                        idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
                    }
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
            _ = heartbeat.tick(), if heartbeat_enabled => {
                if pending_pong_deadline.is_none() && !ping_enqueued {
                    if self_sender
                        .try_send_control(RelayOutbound::Ping(Vec::new()))
                        .is_err()
                    {
                        break;
                    }
                    ping_enqueued = true;
                }
            }
        }
    }

    writer_task.abort();
    state.unregister(&registration);
    if traffic.has_activity() {
        debug!(
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
    heartbeat_debug: &WebSocketHeartbeatDebug,
) {
    let error_text = error.to_string();
    if websocket_receive_failed_is_noisy_client_disconnect(role, &error_text) {
        debug!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            heartbeat_debug = ?heartbeat_debug.pending_snapshot(),
            "relay websocket receive failed"
        );
    } else {
        warn!(
            server_id = %server_id.0,
            ?role,
            connection_id,
            %error,
            heartbeat_debug = ?heartbeat_debug.pending_snapshot(),
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

async fn send_relay_outbound(
    state: &RelayState,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
) -> RelayWriteResult {
    match prepare_relay_outbound(state, server_id, role, connection_id, outbound) {
        PreparedRelayOutbound::Frame(frame) => {
            send_relay_opaque_frame(sender, server_id, role, connection_id, frame, channel).await
        }
        PreparedRelayOutbound::Ping(payload) => {
            let frame_len = payload.len();
            match send_message_with_deadline(
                sender,
                Message::Ping(payload),
                WEBSOCKET_SEND_DEADLINE,
                "relay websocket ping",
            )
            .await
            {
                Ok(()) => RelayWriteResult::Sent(SentRelayOutbound {
                    frame_kind: "ping",
                    frame_len,
                }),
                Err(()) => RelayWriteResult::Failed,
            }
        }
        PreparedRelayOutbound::Pong(payload) => {
            let frame_len = payload.len();
            match send_message_with_deadline(
                sender,
                Message::Pong(payload),
                WEBSOCKET_PONG_DEADLINE,
                "relay websocket pong",
            )
            .await
            {
                Ok(()) => RelayWriteResult::Sent(SentRelayOutbound {
                    frame_kind: "pong",
                    frame_len,
                }),
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
        PreparedRelayOutbound::Drop => RelayWriteResult::Dropped,
    }
}

fn prepare_relay_outbound(
    state: &RelayState,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
) -> PreparedRelayOutbound {
    match outbound {
        RelayOutbound::Frame(frame) => PreparedRelayOutbound::Frame(frame),
        RelayOutbound::MuxClientFrame { client_id, frame } => {
            if role != ConnectionRole::DaemonMux {
                warn!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id,
                    client_id = client_id.0,
                    "dropping mux client frame queued for non-daemon connection"
                );
                return PreparedRelayOutbound::Drop;
            }
            if !state.has_client(server_id, client_id) {
                debug!(
                    server_id = %server_id.0,
                    connection_id,
                    client_id = client_id.0,
                    "dropping queued relay client frame after client disconnect"
                );
                return PreparedRelayOutbound::Drop;
            }
            let frame = mux_envelope_frame(RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: frame.into(),
            });
            PreparedRelayOutbound::Frame(frame)
        }
        RelayOutbound::Ping(payload) => PreparedRelayOutbound::Ping(payload),
        RelayOutbound::Pong(payload) => PreparedRelayOutbound::Pong(payload),
        RelayOutbound::Close => PreparedRelayOutbound::Close,
    }
}

async fn run_relay_websocket_writer(
    state: RelayState,
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
                    debug!(
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
                        &state,
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
                }
                outbound = control_rx.recv() => {
                    let Some(outbound) = outbound else {
                        break;
                    };
                    debug!(
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
                        &state,
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
                debug!(
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
                    &state,
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
                debug!(
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
                    &state,
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

async fn write_relay_outbound_and_report(
    state: &RelayState,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    channel: &'static str,
    outcome_tx: &mpsc::UnboundedSender<RelayWriterOutcome>,
) -> bool {
    let outbound_label = outbound.label();
    let frame_kind = outbound.frame_kind();
    let queued_bytes = outbound.queued_data_bytes();
    match send_relay_outbound(
        state,
        sender,
        server_id,
        role,
        connection_id,
        outbound,
        channel,
    )
    .await
    {
        RelayWriteResult::Sent(sent) => {
            let _ = outcome_tx.send(RelayWriterOutcome::Sent(sent));
            true
        }
        RelayWriteResult::Dropped => {
            debug!(
                server_id = %server_id.0,
                ?role,
                connection_id,
                channel,
                outbound = outbound_label,
                frame_kind,
                queued_bytes,
                "relay websocket writer dropped prepared frame"
            );
            true
        }
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
    if send_message_with_deadline(
        sender,
        frame.into(),
        WEBSOCKET_SEND_DEADLINE,
        "relay websocket outbound frame",
    )
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
    RelayWriteResult::Sent(SentRelayOutbound {
        frame_kind,
        frame_len,
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
    Ok(RoutePrelude {
        server_id: envelope.payload.server_id,
        route_role,
        connection_role: ConnectionRole::from_route_role(route_role),
        route_generation: envelope.payload.route_generation,
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

fn handle_inbound_message(
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
            forward_opaque(state, registration, OpaqueFrame::Text(text))
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
            forward_opaque(state, registration, OpaqueFrame::Binary(bytes))
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
                        ConnectionRole::DaemonMux => {
                            room.daemon_mux.as_ref().and_then(|endpoint| {
                                (endpoint.id == registration.id).then_some(endpoint.sender.clone())
                            })
                        }
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
                    debug!(
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

fn forward_opaque(
    state: &RelayState,
    registration: &ConnectionRegistration,
    frame: OpaqueFrame,
) -> RelayForwardOutcome {
    let report = state.forward_from(registration, frame);
    let should_continue = !(registration.role == ConnectionRole::Client
        && report.dropped > 0
        && !state.has_client(registration.server_id, RelayClientId(registration.id)));
    // 中文注释：转发是 relay 的最高频路径，不能逐帧写日志；连接关闭时会输出聚合计数。
    RelayForwardOutcome {
        report,
        should_continue,
    }
}

fn notify_mux_client_connected(
    state: &RelayState,
    registration: &ConnectionRegistration,
) -> Result<(), RelayError> {
    let Ok(mut rooms) = state.inner.rooms.lock() else {
        warn!("relay registry mutex poisoned during mux connect notify");
        return Err(RelayError::Poisoned);
    };
    let Some(room) = rooms.get_mut(&registration.server_id) else {
        return Err(RelayError::DaemonMuxOffline);
    };
    let Some(daemon_mux) = room.daemon_mux.as_ref() else {
        return Err(RelayError::DaemonMuxOffline);
    };
    let envelope = RelayMuxEnvelope::ClientConnected {
        client_id: RelayClientId(registration.id),
    };
    let notify_result = daemon_mux
        .sender
        .try_send_control(RelayOutbound::Frame(mux_envelope_frame(envelope)));
    match notify_result {
        Ok(()) => {
            debug!(
                server_id = %registration.server_id.0,
                daemon_connection_id = daemon_mux.id,
                client_connection_id = registration.id,
                "relay queued client connected notification to daemon mux"
            );
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!(
                server_id = %registration.server_id.0,
                daemon_connection_id = daemon_mux.id,
                client_connection_id = registration.id,
                "rejecting relay client because daemon mux control queue is full"
            );
            Err(RelayError::DaemonMuxBusy)
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!(
                server_id = %registration.server_id.0,
                stale_connection_id = daemon_mux.id,
                "dropping offline relay daemon mux during client connect notify"
            );
            room.clear_daemon_mux_and_dependents();
            Err(RelayError::DaemonMuxOffline)
        }
    }
}

fn mux_envelope_frame(envelope: RelayMuxEnvelope) -> OpaqueFrame {
    OpaqueFrame::Binary(
        encode_binary_relay_mux_envelope(&envelope)
            .expect("relay mux envelope should encode as binary"),
    )
}

fn mux_envelope_from_opaque_frame(
    frame: OpaqueFrame,
) -> Result<RelayMuxEnvelope, RelayMuxFrameError> {
    match frame {
        OpaqueFrame::Text(raw) => serde_json::from_str::<RelayMuxEnvelope>(&raw)
            .map_err(|_| RelayMuxFrameError::InvalidEnvelope),
        OpaqueFrame::Binary(raw) => {
            decode_binary_relay_mux_envelope(&raw).map_err(|_| RelayMuxFrameError::InvalidEnvelope)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::router;
    use termd_proto::{
        Nonce, PROTOCOL_PACKET_VERSION, ProtocolVersion, RouteReadyPayload, UnixTimestampMillis,
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

    fn client_route_hello(server_id: ServerId) -> Envelope<RouteHelloPayload> {
        Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role: RouteRole::Client,
                protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                nonce: Nonce("test-route".to_owned()),
                route_generation: None,
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
        let hello = Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role,
                protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                nonce: Nonce("test-route".to_owned()),
                route_generation: match role {
                    RouteRole::DaemonMux => Some(Nonce("test-route-generation".to_owned())),
                    RouteRole::Client => None,
                },
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        );
        socket
            .send(ClientMessage::Text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();

        let next = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay should answer route_ready")
            .expect("relay websocket should stay open")
            .expect("route_ready frame should be valid");
        let ClientMessage::Text(raw) = next else {
            panic!("expected route_ready text frame, got {next:?}");
        };
        let ready: Envelope<RouteReadyPayload> = serde_json::from_str(&raw).unwrap();
        assert_eq!(ready.kind, MessageType::RouteReady);
        assert_eq!(ready.payload.server_id, server_id);
        assert_eq!(ready.payload.role, role);
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
    fn websocket_heartbeat_requires_pong_even_when_data_flows() {
        let mut heartbeat = WebSocketHeartbeatDebug::new(Instant::now());
        heartbeat.note_ping_sent();

        heartbeat.record_outbound("text", 128);

        assert!(heartbeat.pending_snapshot().is_some());
    }

    #[test]
    fn websocket_heartbeat_pong_clears_pending_ping() {
        let mut heartbeat = WebSocketHeartbeatDebug::new(Instant::now());
        heartbeat.note_ping_sent();

        heartbeat.note_pong_received();

        assert!(heartbeat.pending_snapshot().is_none());
    }

    #[test]
    fn relay_does_not_heartbeat_or_idle_timeout_transport_roles() {
        assert!(!websocket_heartbeat_enabled(ConnectionRole::DaemonMux));
        assert!(!websocket_heartbeat_enabled(ConnectionRole::Client));
        assert!(!websocket_idle_timeout_enabled(ConnectionRole::DaemonMux));
        assert!(!websocket_idle_timeout_enabled(ConnectionRole::Client));
    }

    #[test]
    fn relay_route_prelude_uses_browser_friendly_timeout() {
        assert_eq!(ROUTE_PRELUDE_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn client_reset_without_close_is_debug_noise_not_daemon_mux_warning() {
        let reset = "WebSocket protocol error: Connection reset without closing handshake";

        assert!(websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::Client,
            reset
        ));
        assert!(!websocket_receive_failed_is_noisy_client_disconnect(
            ConnectionRole::DaemonMux,
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
    async fn client_receives_retryable_error_when_daemon_mux_is_offline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
                .await
                .unwrap();
        });
        let raw = serde_json::to_string(&client_route_hello(server_id(1))).unwrap();

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

    #[tokio::test]
    async fn relay_client_socket_does_not_receive_relay_initiated_ping() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
                .await
                .unwrap();
        });
        let server_id = server_id(91);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_mux, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_mux, server_id, RouteRole::DaemonMux).await;
        let (mut client, _client_response) = connect_async(url).await.unwrap();
        register_test_route(&mut client, server_id, RouteRole::Client).await;

        match timeout(
            WEBSOCKET_HEARTBEAT_INTERVAL + Duration::from_millis(200),
            client.next(),
        )
        .await
        {
            Err(_) => {}
            Ok(Some(Ok(ClientMessage::Ping(_)))) => {
                panic!("relay must not ping browser clients; background tabs can delay pong")
            }
            Ok(other) => panic!("expected no relay-initiated frame, got {other:?}"),
        }

        daemon_mux.close(None).await.unwrap();
        client.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_mux_inbound_is_read_while_outbound_to_daemon_is_backpressured() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
                .await
                .unwrap();
        });
        let server_id = server_id(9001);
        let url = format!("ws://{addr}/ws");
        let (mut daemon_mux, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut daemon_mux, server_id, RouteRole::DaemonMux).await;
        let (mut daemon_tx, _daemon_rx) = daemon_mux.split();

        let (mut noisy_client, _noisy_response) = connect_async(url.clone()).await.unwrap();
        register_test_route(&mut noisy_client, server_id, RouteRole::Client).await;
        let (mut fresh_client, _fresh_response) = connect_async(url).await.unwrap();
        register_test_route(&mut fresh_client, server_id, RouteRole::Client).await;

        let noisy_payload = vec![b'x'; WEBSOCKET_MAX_FRAME_SIZE / 2];
        for _ in 0..384 {
            if noisy_client
                .send(ClientMessage::Binary(noisy_payload.clone()))
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 连接 id 在这个独立 RelayState 内按注册顺序递增：
        // daemon_mux=1、noisy_client=2、fresh_client=3。
        let fresh_frame = RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(3),
            frame: RelayOpaqueFrame::Text {
                data: "fresh-client-ready".to_owned(),
            },
        };
        daemon_tx
            .send(ClientMessage::Binary(
                encode_binary_relay_mux_envelope(&fresh_frame).unwrap(),
            ))
            .await
            .unwrap();

        let received = timeout(Duration::from_millis(500), fresh_client.next())
            .await
            .expect("daemon->client frame must not wait behind client->daemon backpressure")
            .expect("fresh client websocket should stay open")
            .expect("fresh client frame should be valid");
        assert_eq!(
            received,
            ClientMessage::Text("fresh-client-ready".to_owned())
        );
        server.abort();
    }

    #[test]
    fn room_registers_one_daemon_mux_and_many_clients() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel();
        let (client_a_tx, _client_a_rx) = channel();
        let (client_b_tx, _client_b_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_a_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_b_tx)
            .unwrap();

        assert_eq!(state.room_count(), 1);
    }

    #[test]
    fn room_replaces_duplicate_daemon_mux_and_closes_stale_clients() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (first_tx, mut first_rx) = channel();
        let (second_tx, _second_rx) = channel();
        let (client_tx, mut client_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonMux, first_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();
        let second = state
            .register(server_id, ConnectionRole::DaemonMux, second_tx)
            .unwrap();

        assert_eq!(state.room_count(), 1);
        assert_eq!(second.role, ConnectionRole::DaemonMux);
        assert_eq!(first_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(client_rx.try_recv().unwrap(), RelayOutbound::Close);
    }

    #[test]
    fn daemon_mux_disconnect_closes_clients() {
        let state = RelayState::default();
        let server = server_id(1);
        let (mux_tx, _mux_rx) = channel();
        let (client_tx, mut client_rx) = channel();

        let daemon_mux = state
            .register(server, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        state
            .register(server, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&daemon_mux);

        assert_eq!(client_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(state.room_count(), 0);
    }

    #[test]
    fn replacing_control_mux_closes_stale_mux_and_stale_mux_cannot_forward() {
        let state = RelayState::default();
        let server = server_id(1);
        let (first_mux_tx, mut first_mux_rx) = channel();
        let (second_mux_tx, _second_mux_rx) = channel();
        let (client_tx, mut client_rx) = channel();

        state
            .register(server, ConnectionRole::DaemonMux, first_mux_tx)
            .unwrap();
        let (stale_mux_tx, _stale_mux_rx) = channel();
        let stale_mux = state
            .register(server, ConnectionRole::DaemonMux, stale_mux_tx)
            .unwrap();
        state
            .register(server, ConnectionRole::DaemonMux, second_mux_tx)
            .unwrap();
        let client = state
            .register(server, ConnectionRole::Client, client_tx)
            .unwrap();

        let stale_frame = mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(client.id),
            frame: RelayOpaqueFrame::Text {
                data: "stale-output".to_owned(),
            },
        });
        let report = state.forward_from(&stale_mux, stale_frame);

        assert_eq!(first_mux_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(report.dropped, 1);
        assert_eq!(client_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn room_rejects_client_when_daemon_mux_is_offline() {
        let state = RelayState::default();
        let (client_tx, _client_rx) = channel();

        let error = state
            .register(server_id(1), ConnectionRole::Client, client_tx)
            .unwrap_err();

        assert_eq!(error, RelayError::DaemonMuxOffline);
    }

    #[test]
    fn client_frames_are_wrapped_for_daemon_mux() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel();
        let (client_tx, _client_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        let text_report = state.forward_from(&client, OpaqueFrame::Text("{not-json".to_owned()));
        let binary_report = state.forward_from(&client, OpaqueFrame::Binary(vec![1, 2, 3]));

        assert_eq!(text_report.delivered, 1);
        assert_eq!(binary_report.delivered, 1);
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientFrame {
                client_id: RelayClientId(client.id),
                frame: RelayOpaqueFrame::Text {
                    data: "{not-json".to_owned(),
                },
            }
        );
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientFrame {
                client_id: RelayClientId(client.id),
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "AQID".to_owned(),
                },
            }
        );
    }

    #[test]
    fn client_frame_goes_only_to_matching_daemon_mux() {
        let state = RelayState::default();
        let server_a = server_id(1);
        let server_b = server_id(2);
        let (mux_a_tx, mut mux_a_rx) = channel();
        let (mux_b_tx, mut mux_b_rx) = channel();
        let (client_a_tx, _client_a_rx) = channel();

        state
            .register(server_a, ConnectionRole::DaemonMux, mux_a_tx)
            .unwrap();
        state
            .register(server_b, ConnectionRole::DaemonMux, mux_b_tx)
            .unwrap();
        let client_a = state
            .register(server_a, ConnectionRole::Client, client_a_tx)
            .unwrap();

        let report = state.forward_from(&client_a, OpaqueFrame::Text("opaque".to_owned()));

        assert_eq!(report.delivered, 1);
        assert_eq!(
            decode_mux(mux_a_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientFrame {
                client_id: RelayClientId(client_a.id),
                frame: RelayOpaqueFrame::Text {
                    data: "opaque".to_owned(),
                },
            }
        );
        assert_eq!(mux_b_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn mux_daemon_receives_client_events_and_frames_with_client_id() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel();
        let (client_tx, _client_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();
        notify_mux_client_connected(&state, &client).unwrap();

        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientConnected {
                client_id: RelayClientId(client.id)
            }
        );

        let report = state.forward_from(
            &client,
            OpaqueFrame::Text("{\"type\":\"pair_request\",\"payload\":\"opaque\"}".to_owned()),
        );

        assert_eq!(report.delivered, 1);
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientFrame {
                client_id: RelayClientId(client.id),
                frame: RelayOpaqueFrame::Text {
                    data: "{\"type\":\"pair_request\",\"payload\":\"opaque\"}".to_owned(),
                },
            }
        );

        state.unregister(&client);
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id)
            }
        );
    }

    #[test]
    fn mux_daemon_frame_goes_only_to_target_client() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel();
        let (client_a_tx, mut client_a_rx) = channel();
        let (client_b_tx, mut client_b_rx) = channel();

        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client_a = state
            .register(server_id, ConnectionRole::Client, client_a_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_b_tx)
            .unwrap();

        let response = mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(client_a.id),
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AQIDBA==".to_owned(),
            },
        });
        let report = state.forward_from(&mux, response);

        assert_eq!(report.delivered, 1);
        assert_eq!(
            client_a_rx.try_recv().unwrap(),
            RelayOutbound::Frame(OpaqueFrame::Binary(vec![1, 2, 3, 4]))
        );
        assert_eq!(client_b_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn mux_daemon_keepalive_is_ignored_without_touching_clients() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel();
        let (client_tx, mut client_rx) = channel();

        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        let report = state.forward_from(
            &mux,
            mux_envelope_frame(RelayMuxEnvelope::Keepalive { nonce: 42 }),
        );

        assert_eq!(report.delivered, 0);
        assert_eq!(report.dropped, 0);
        assert_eq!(mux_rx.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(client_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn relay_mux_uses_binary_frame_for_opaque_binary_payload() {
        let envelope = RelayMuxEnvelope::ClientFrame {
            client_id: RelayClientId(7),
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AQIDBA==".to_owned(),
            },
        };
        let frame = mux_envelope_frame(envelope.clone());
        let OpaqueFrame::Binary(raw) = &frame else {
            panic!("expected binary mux frame");
        };

        assert!(!String::from_utf8_lossy(raw).contains("data_base64"));
        assert!(raw.ends_with(&[1, 2, 3, 4]));
        assert_eq!(mux_envelope_from_opaque_frame(frame).unwrap(), envelope);
    }

    #[test]
    fn slow_client_drop_notifies_daemon_mux() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel();
        let (client_tx, _client_rx) = channel_with_data_capacity(1);

        client_tx
            .try_send(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        let response = mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(client.id),
            frame: RelayOpaqueFrame::Text {
                data: "response".to_owned(),
            },
        });
        let report = state.forward_from(&mux, response);

        assert_eq!(report.dropped, 1);
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id)
            }
        );
    }

    #[test]
    fn slow_client_disconnect_keeps_daemon_mux_when_lifecycle_queue_is_full() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_control_capacity(1);
        let (client_tx, _client_rx) = channel_with_data_capacity(1);

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        client_tx
            .try_send(RelayOutbound::Frame(OpaqueFrame::Text("queued".to_owned())))
            .unwrap();
        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        let response = mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(client.id),
            frame: RelayOpaqueFrame::Text {
                data: "response".to_owned(),
            },
        });
        let report = state.forward_from(&mux, response);

        assert_eq!(report.dropped, 1);
        assert!(!state.has_client(server_id, RelayClientId(client.id)));
        assert_eq!(
            state.forward_from(
                &mux,
                mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
                    client_id: RelayClientId(client.id),
                    frame: RelayOpaqueFrame::Text {
                        data: "late".to_owned(),
                    },
                }),
            ),
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1
            }
        );
        assert_eq!(mux_rx.try_recv().unwrap(), RelayOutbound::Ping(Vec::new()));
    }

    #[test]
    fn client_connect_reports_busy_when_daemon_mux_lifecycle_queue_is_full() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel_with_control_capacity(1);
        let (client_tx, _client_rx) = channel();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        let error = notify_mux_client_connected(&state, &client).unwrap_err();

        // 中文注释：daemon mux 在线但 lifecycle/control 队列满是 backpressure，
        // 不能伪装成 offline，否则 Web 会进入错误的重连/离线提示路径。
        assert_eq!(error, RelayError::DaemonMuxBusy);
    }

    #[tokio::test]
    async fn old_client_disconnect_does_not_close_fresh_client_when_daemon_mux_control_is_full() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel_with_control_capacity(1);
        let (old_client_tx, _old_client_rx) = channel();
        let (fresh_client_tx, _fresh_client_rx) = channel();
        let mut mux_close_rx = mux_tx.subscribe_close();
        let mut fresh_close_rx = fresh_client_tx.subscribe_close();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let old_client = state
            .register(server_id, ConnectionRole::Client, old_client_tx)
            .unwrap();
        let fresh_client = state
            .register(server_id, ConnectionRole::Client, fresh_client_tx)
            .unwrap();

        state.unregister(&old_client);

        // 旧 client 的断开通知可以丢失，但不能把同 room 的新 client 和 daemon mux 级联关闭。
        assert!(state.has_client(server_id, RelayClientId(fresh_client.id)));
        assert!(
            timeout(Duration::from_millis(30), mux_close_rx.closed())
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(30), fresh_close_rx.closed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn busy_new_client_unregister_does_not_close_existing_clients_or_daemon_mux() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel_with_control_capacity(1);
        let (existing_client_tx, _existing_client_rx) = channel();
        let (fresh_client_tx, _fresh_client_rx) = channel();
        let mut mux_close_rx = mux_tx.subscribe_close();
        let mut existing_close_rx = existing_client_tx.subscribe_close();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let existing_client = state
            .register(server_id, ConnectionRole::Client, existing_client_tx)
            .unwrap();
        let fresh_client = state
            .register(server_id, ConnectionRole::Client, fresh_client_tx)
            .unwrap();

        let error = notify_mux_client_connected(&state, &fresh_client).unwrap_err();
        assert_eq!(error, RelayError::DaemonMuxBusy);
        state.unregister(&fresh_client);

        // 新 client 因 mux 承压被拒后，清理它自己即可；不能把已存在的 client 一起踢掉。
        assert!(state.has_client(server_id, RelayClientId(existing_client.id)));
        assert!(
            timeout(Duration::from_millis(30), mux_close_rx.closed())
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(30), existing_close_rx.closed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn endpoint_close_signal_terminates_even_when_control_queue_is_full() {
        let (sender, mut receiver) = channel_with_control_capacity(1);
        let mut close_rx = sender.subscribe_close();

        sender
            .try_send_control(RelayOutbound::Ping(Vec::new()))
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
            RelayOutbound::Ping(Vec::new())
        );
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[tokio::test]
    async fn daemon_mux_cleanup_signals_dependents_even_when_close_queues_are_full() {
        let state = RelayState::default();
        let server = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_control_capacity(1);
        let (client_tx, mut client_rx) = channel_with_control_capacity(1);
        let mut mux_close_rx = mux_tx.subscribe_close();
        let mut client_close_rx = client_tx.subscribe_close();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        client_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        let daemon_mux = state
            .register(server, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        state
            .register(server, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&daemon_mux);

        timeout(Duration::from_millis(50), mux_close_rx.closed())
            .await
            .expect("daemon mux close signal should bypass its full control queue");
        timeout(Duration::from_millis(50), client_close_rx.closed())
            .await
            .expect("client close signal should bypass its full control queue");
        assert_eq!(mux_rx.try_recv().unwrap(), RelayOutbound::Ping(Vec::new()));
        assert_eq!(
            client_rx.try_recv().unwrap(),
            RelayOutbound::Ping(Vec::new())
        );
        assert_no_queued_close(&mut mux_rx);
        assert_no_queued_close(&mut client_rx);
        assert_eq!(state.room_count(), 0);
    }

    #[test]
    fn data_queue_rejects_large_frames_by_byte_budget_before_frame_capacity() {
        let (sender, mut receiver) = channel_with_data_capacity(DATA_CHANNEL_CAPACITY);
        let frame = RelayOutbound::Frame(OpaqueFrame::Binary(vec![7; WEBSOCKET_MAX_FRAME_SIZE]));

        for _ in 0..8 {
            sender.try_send(frame.clone()).unwrap();
        }

        assert!(matches!(
            sender.try_send(frame.clone()),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert_eq!(receiver.try_recv().unwrap(), frame);
        sender.try_send(frame).unwrap();
    }

    #[test]
    fn relay_traffic_counters_aggregate_forwarded_frames() {
        let mut traffic = RelayConnectionTraffic::default();

        traffic.record_inbound(&Message::Binary(vec![1, 2, 3]));
        traffic.record_outbound("text", 5);
        traffic.record_forward(ForwardReport {
            attempted: 2,
            delivered: 1,
            dropped: 1,
        });

        assert!(traffic.has_activity());
        assert_eq!(traffic.in_binary.calls, 1);
        assert_eq!(traffic.in_binary.bytes, 3);
        assert_eq!(traffic.out_text.calls, 1);
        assert_eq!(traffic.out_text.bytes, 5);
        assert_eq!(traffic.forwarded_attempted, 2);
        assert_eq!(traffic.forwarded_delivered, 1);
        assert_eq!(traffic.forwarded_dropped, 1);
    }

    #[test]
    fn client_disconnect_bypasses_full_daemon_mux_queue() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_data_capacity(1);
        let (client_tx, _client_rx) = channel();

        mux_tx
            .try_send(RelayOutbound::MuxClientFrame {
                client_id: RelayClientId(999),
                frame: OpaqueFrame::Text("queued-data".to_owned()),
            })
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&client);

        // 断开事件必须走控制队列并抢在普通业务帧前面，否则 daemon 会继续保留 stale watcher。
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id)
            }
        );
        assert_eq!(
            mux_rx.try_recv().unwrap(),
            RelayOutbound::MuxClientFrame {
                client_id: RelayClientId(999),
                frame: OpaqueFrame::Text("queued-data".to_owned())
            }
        );
    }

    #[test]
    fn full_daemon_mux_queue_drops_only_current_client() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_data_capacity(1);
        let (client_tx, mut client_rx) = channel();
        let (other_client_tx, mut other_client_rx) = channel();

        mux_tx
            .try_send(RelayOutbound::MuxClientFrame {
                client_id: RelayClientId(999),
                frame: OpaqueFrame::Text("queued-data".to_owned()),
            })
            .unwrap();
        state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();
        let other_client = state
            .register(server_id, ConnectionRole::Client, other_client_tx)
            .unwrap();

        let report = state.forward_from(&client, OpaqueFrame::Text("overflow".to_owned()));

        assert_eq!(report.attempted, 1);
        assert_eq!(report.delivered, 0);
        assert_eq!(report.dropped, 1);
        assert_eq!(client_rx.try_recv().unwrap(), RelayOutbound::Close);
        assert_eq!(other_client_rx.try_recv().unwrap_err(), TryRecvError::Empty);

        // daemon mux data 队列满只是背压，不是 daemon 离线；其他 client 仍应能留在 room 中。
        assert!(state.has_client(server_id, RelayClientId(other_client.id)));
        assert!(!state.has_client(server_id, RelayClientId(client.id)));
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id)
            }
        );
        assert_eq!(
            mux_rx.try_recv().unwrap(),
            RelayOutbound::MuxClientFrame {
                client_id: RelayClientId(999),
                frame: OpaqueFrame::Text("queued-data".to_owned())
            }
        );
    }

    #[test]
    fn stale_daemon_output_for_removed_client_retries_disconnect_notify() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_control_capacity(1);
        let (client_tx, _client_rx) = channel();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&client);

        // 中文注释：第一次 disconnect notify 因 control 队列满而丢失；真实 writer
        // 消费掉旧控制帧后，如果 daemon 仍给旧 client 推输出，relay 必须再次提醒 daemon 清理。
        assert_eq!(mux_rx.try_recv().unwrap(), RelayOutbound::Ping(Vec::new()));
        let report = state.forward_from(
            &mux,
            mux_envelope_frame(RelayMuxEnvelope::DaemonFrame {
                client_id: RelayClientId(client.id),
                frame: RelayOpaqueFrame::Text {
                    data: "stale-output".to_owned(),
                },
            }),
        );

        assert_eq!(
            report,
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            }
        );
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id),
            }
        );
    }

    #[test]
    fn stale_daemon_output_for_removed_client_retries_disconnect_before_payload_decode() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, mut mux_rx) = channel_with_control_capacity(1);
        let (client_tx, _client_rx) = channel();

        mux_tx
            .try_send_control(RelayOutbound::Ping(Vec::new()))
            .unwrap();
        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&client);
        assert_eq!(mux_rx.try_recv().unwrap(), RelayOutbound::Ping(Vec::new()));

        let report = state.forward_from(
            &mux,
            OpaqueFrame::Text(
                serde_json::to_string(&RelayMuxEnvelope::DaemonFrame {
                    client_id: RelayClientId(client.id),
                    frame: RelayOpaqueFrame::Binary {
                        data_base64: "not base64".to_owned(),
                    },
                })
                .unwrap(),
            ),
        );

        // 中文注释：目标 client 已经不存在时，relay 应先处理生命周期 tombstone，
        // 不能为了一个已无接收方的旧输出再解码内部 payload。
        assert_eq!(
            report,
            ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            }
        );
        assert_eq!(
            decode_mux(mux_rx.try_recv().unwrap()),
            RelayMuxEnvelope::ClientDisconnected {
                client_id: RelayClientId(client.id),
            }
        );
    }

    #[test]
    fn queued_client_frame_is_dropped_after_client_disconnect() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (mux_tx, _mux_rx) = channel();
        let (client_tx, _client_rx) = channel();

        let mux = state
            .register(server_id, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        let client = state
            .register(server_id, ConnectionRole::Client, client_tx)
            .unwrap();
        state.unregister(&client);

        let prepared = prepare_relay_outbound(
            &state,
            server_id,
            ConnectionRole::DaemonMux,
            mux.id,
            RelayOutbound::MuxClientFrame {
                client_id: RelayClientId(client.id),
                frame: OpaqueFrame::Text("late-flow".to_owned()),
            },
        );

        // disconnect 之后残留在普通队列里的 flow/data 不能再发给 daemon。
        assert_eq!(prepared, PreparedRelayOutbound::Drop);
    }

    #[test]
    fn disconnect_cleans_room_without_affecting_other_server_id() {
        let state = RelayState::default();
        let server_a = server_id(1);
        let server_b = server_id(2);
        let (mux_a_tx, _mux_a_rx) = channel();
        let (mux_b_tx, _mux_b_rx) = channel();

        let daemon_mux_a = state
            .register(server_a, ConnectionRole::DaemonMux, mux_a_tx)
            .unwrap();
        state
            .register(server_b, ConnectionRole::DaemonMux, mux_b_tx)
            .unwrap();

        state.unregister(&daemon_mux_a);

        assert_eq!(state.room_count(), 1);
    }

    #[test]
    fn daemon_mux_disconnect_closes_clients_for_same_server_id() {
        let state = RelayState::default();
        let server = server_id(1);
        let (mux_tx, _mux_rx) = channel();
        let (client_tx, mut client_rx) = channel();

        let daemon_mux = state
            .register(server, ConnectionRole::DaemonMux, mux_tx)
            .unwrap();
        state
            .register(server, ConnectionRole::Client, client_tx)
            .unwrap();

        state.unregister(&daemon_mux);

        assert_eq!(state.room_count(), 0);
        // daemon mux 已经不可用时，client 不能继续挂在房间里等待一个永远不会来的响应。
        assert_eq!(client_rx.try_recv().unwrap(), RelayOutbound::Close);
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

    fn decode_mux(outbound: RelayOutbound) -> RelayMuxEnvelope {
        match outbound {
            RelayOutbound::Frame(frame) => return mux_envelope_from_opaque_frame(frame).unwrap(),
            RelayOutbound::MuxClientFrame { client_id, frame } => {
                let envelope = RelayMuxEnvelope::ClientFrame {
                    client_id,
                    frame: frame.into(),
                };
                return envelope;
            }
            other => {
                panic!("expected mux envelope, got {other:?}");
            }
        }
    }

    fn assert_no_queued_close(receiver: &mut TestReceiver) {
        // 中文注释：sender 被 room 清理后可能已经 drop；Empty 和 Disconnected 都说明
        // 没有把 Close 硬塞进已满队列，endpoint 退出依赖的是独立 close 信号。
        match receiver.try_recv() {
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            other => panic!("expected no queued close frame, got {other:?}"),
        }
    }
}
