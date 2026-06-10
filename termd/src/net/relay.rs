//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use axum::body::{Body, Bytes};
#[cfg(test)]
use base64::{Engine as _, engine::general_purpose};
use futures_util::{Sink, SinkExt, StreamExt};
use rustls::{ClientConfig, RootCertStore};
use termd_proto::{
    Envelope as ProtoEnvelope, MessageType as ProtoMessageType, Nonce as ProtoNonce,
    PROTOCOL_PACKET_VERSION, ProtocolVersion as ProtoProtocolVersion, RelayClientId,
    RelayControlEnvelope, RelayHttpTunnelFrame, RouteHelloPayload as ProtoRouteHelloPayload,
    RouteReadyPayload as ProtoRouteReadyPayload, RouteRole as ProtoRouteRole, ServerId, SessionId,
    decode_relay_data_control, decode_relay_http_tunnel_frame,
    encode_relay_http_tunnel_response_body, encode_relay_http_tunnel_response_end,
    encode_relay_http_tunnel_response_head,
};
#[cfg(test)]
use termd_proto::{
    RelayMuxEnvelope, RelayOpaqueFrame, decode_binary_relay_mux_envelope,
    encode_binary_relay_mux_envelope, encode_relay_data_control,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::{
    Connector,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tracing::{debug, info, trace, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::RelayReconnectConfig;

#[cfg(test)]
use super::protocol::ProtocolConnectionDebugTraffic;
use super::protocol::{JsonEnvelope, ProtocolConnection, ProtocolError, ProtocolWireMessage};
use super::server::SharedDaemonProtocol;
use super::server::handle_http_file_tunnel_stream_request;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 512 * 1024;
const MIN_RELAY_RETRY_DELAY_MS: u64 = 1;
const MIN_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 1;
// relay mux transport 失败只会断开当前 relay 连接并触发重连，不关闭持久 session/supervisor。
// 公网 relay 往往还隔着 TLS 和反向代理，2s 级 deadline 容易把短暂抖动误判成断线。
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_IDLE_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_ROUTE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_SEND_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
const RELAY_CONTROL_WRITE_COMPLETION_DEADLINE: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const RELAY_PONG_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
const RELAY_PONG_DEADLINE: Duration = Duration::from_millis(50);
#[cfg(test)]
const RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const RELAY_RECONNECT_STABLE_RESET_AFTER: Duration = Duration::from_secs(60);
// relay 是加密 dumb pipe，daemon 侧不能依赖 relay 解析业务分片。
// 允许 MB 级 terminal snapshot 通过，同时保留明确的单帧/单消息内存上限。
const RELAY_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const RELAY_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
#[cfg(test)]
const RELAY_TRAFFIC_LOG_INTERVAL: Duration = Duration::from_secs(1);
const RELAY_SEND_SLOW_LOG_THRESHOLD: Duration = Duration::from_millis(50);
const RELAY_SEND_DEBUG_LOG_THRESHOLD: Duration = Duration::from_millis(10);
const RELAY_SEND_DEBUG_BATCH_ENVELOPES: usize = 8;
const RELAY_SEND_DEBUG_BATCH_BYTES: usize = 512 * 1024;
const RELAY_SEND_INFO_BATCH_ENVELOPES: usize = 64;
const RELAY_SEND_INFO_BATCH_BYTES: usize = 8 * 1024 * 1024;
const RELAY_PUSH_EVENT_QUEUE_CAPACITY: usize = 2048;
#[cfg(test)]
const RELAY_MUX_CONTROL_QUEUE_CAPACITY: usize = 256;
#[cfg(test)]
const RELAY_MUX_DATA_QUEUE_CAPACITY: usize = 32;
// 中文注释：daemon->relay data writer 只保留一个待写批次，让 WebSocket 写速率成为真实背压。
// HTTP tunnel 下载不能在 daemon 侧按 256KiB * 2048 继续堆积。
const RELAY_DATA_WIRE_QUEUE_CAPACITY: usize = 1;
// 中文注释：relay client 接入不能依赖即时新建公网 TLS/WebSocket。daemon 预先维持少量
// idle data pipe，client 到达时 relay 只做本地配对，避免反代或网络偶发慢连接进入用户路径。
#[cfg(not(test))]
const RELAY_IDLE_DATA_POOL_TARGET: usize = 8;
#[cfg(test)]
const RELAY_IDLE_DATA_POOL_TARGET: usize = 0;
const RELAY_IDLE_DATA_REFILL_MIN_DELAY: Duration = Duration::from_secs(1);
const RELAY_IDLE_DATA_REFILL_MAX_DELAY: Duration = Duration::from_secs(5);
const RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK: usize = 64;
const RELAY_PUSH_DRAIN_MAX_TRANSPORTED_BYTES_PER_TICK: usize = 16 * 1024 * 1024;
const RELAY_PUSH_DRAIN_MAX_ELAPSED_PER_TICK: Duration = Duration::from_millis(4);
const RELAY_PUSH_DRAIN_RETRY_DELAY: Duration = Duration::from_millis(5);
const RELAY_OUTPUT_PUSH_COALESCE_DELAY: Duration = Duration::from_millis(10);

type RelayWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
type RelaySender = futures_util::stream::SplitSink<RelayWs, Message>;
type RelayReceiver = futures_util::stream::SplitStream<RelayWs>;
type RelayDataTaskMap = HashMap<RelayClientId, JoinHandle<()>>;
type RelayIdleDataTaskMap = HashMap<u64, JoinHandle<()>>;

#[derive(Debug, Clone, Copy)]
enum RelayIdleDataEvent {
    Ready {
        task_id: u64,
    },
    Assigned {
        task_id: u64,
        client_id: RelayClientId,
    },
    Closed {
        task_id: u64,
    },
}

enum RelayEstablishedDataOutcome {
    SocketClosed,
}

#[derive(Debug)]
enum RelayDataWriterOutcome<S> {
    Reusable(S),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayEstablishedDataEnd {
    SocketClosed,
    ClientDisconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RelayPushEvent {
    Output {
        client_id: RelayClientId,
        session_id: SessionId,
    },
    Cwd {
        client_id: RelayClientId,
        session_id: SessionId,
    },
    Resize {
        client_id: RelayClientId,
        session_id: SessionId,
    },
}

impl RelayPushEvent {
    #[cfg(test)]
    fn label(self) -> &'static str {
        match self {
            Self::Output { .. } => "output",
            Self::Cwd { .. } => "cwd",
            Self::Resize { .. } => "resize",
        }
    }

    fn client_id(self) -> RelayClientId {
        match self {
            Self::Output { client_id, .. }
            | Self::Cwd { client_id, .. }
            | Self::Resize { client_id, .. } => client_id,
        }
    }

    #[cfg(test)]
    fn session_id(self) -> SessionId {
        match self {
            Self::Output { session_id, .. }
            | Self::Cwd { session_id, .. }
            | Self::Resize { session_id, .. } => session_id,
        }
    }
}

#[derive(Debug, Default)]
struct RelayPushEventQueue {
    pending: VecDeque<RelayPushEvent>,
    pending_set: HashSet<RelayPushEvent>,
}

impl RelayPushEventQueue {
    fn enqueue(&mut self, event: RelayPushEvent) {
        if self.pending_set.contains(&event) {
            return;
        }
        self.pending_set.insert(event);
        self.pending.push_back(event);
    }

    fn requeue_front(&mut self, event: RelayPushEvent) {
        if self.pending_set.contains(&event) {
            return;
        }
        // 中文注释：writer 承压时不能先读取并加密 terminal cache。
        // 将事件放回队首，下一轮仍按原顺序继续推送，不消耗 E2EE sequence。
        self.pending_set.insert(event);
        self.pending.push_front(event);
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn len(&self) -> usize {
        self.pending.len()
    }

    fn pop_front(&mut self) -> Option<RelayPushEvent> {
        let event = self.pending.pop_front()?;
        self.pending_set.remove(&event);
        Some(event)
    }

    fn remove_client(&mut self, client_id: RelayClientId) {
        // 中文注释：client 断开后，尚未加密的 watcher 事件已经没有接收方。
        // 立即清理 pending，避免旧会话输出在 daemon mux 内继续排队。
        let before = self.pending.len();
        self.pending
            .retain(|event| relay_push_event_client_id(*event) != client_id);
        self.pending_set
            .retain(|event| relay_push_event_client_id(*event) != client_id);
        let removed = before.saturating_sub(self.pending.len());
        if removed > 0 {
            debug!(
                client_id = client_id.0,
                removed,
                queue_pending = self.pending.len(),
                "relay mux pending events removed for inactive client"
            );
        }
    }
}

#[cfg(test)]
type RelayActiveClients = Arc<Mutex<HashSet<RelayClientId>>>;
#[cfg(test)]
type RelayMuxOrdering = Arc<Mutex<RelayMuxOrderingState>>;

#[cfg(test)]
#[derive(Debug, Default)]
struct RelayMuxOrderingState {
    next_enqueue: HashMap<RelayClientId, u64>,
    next_send: HashMap<RelayClientId, u64>,
    pending: HashMap<RelayClientId, usize>,
}

#[cfg(test)]
fn mark_relay_client_active(active_clients: &RelayActiveClients, client_id: RelayClientId) {
    if let Ok(mut active) = active_clients.lock() {
        active.insert(client_id);
    }
}

#[cfg(test)]
fn mark_relay_client_inactive(active_clients: &RelayActiveClients, client_id: RelayClientId) {
    if let Ok(mut active) = active_clients.lock() {
        active.remove(&client_id);
    }
}

#[cfg(test)]
fn relay_client_is_active(active_clients: &RelayActiveClients, client_id: RelayClientId) -> bool {
    active_clients
        .lock()
        .is_ok_and(|active| active.contains(&client_id))
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
struct RelayTrafficBucket {
    calls: u64,
    envelopes: u64,
    bytes: u64,
}

#[cfg(test)]
impl RelayTrafficBucket {
    fn record(&mut self, envelopes: usize, bytes: usize) {
        self.calls = self.calls.saturating_add(1);
        self.envelopes = self.envelopes.saturating_add(envelopes as u64);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    fn is_empty(self) -> bool {
        self.calls == 0 && self.envelopes == 0 && self.bytes == 0
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct RelayTrafficCounters {
    in_text: RelayTrafficBucket,
    in_binary: RelayTrafficBucket,
    in_ping: RelayTrafficBucket,
    in_pong: RelayTrafficBucket,
    in_close: RelayTrafficBucket,
    in_frame: RelayTrafficBucket,
    out_response: RelayTrafficBucket,
    out_push_output: RelayTrafficBucket,
    out_push_cwd: RelayTrafficBucket,
    out_push_resize: RelayTrafficBucket,
    out_mux_keepalive: RelayTrafficBucket,
    out_idle_ping: RelayTrafficBucket,
    out_pong: RelayTrafficBucket,
    send_errors: u64,
}

#[cfg(test)]
impl RelayTrafficCounters {
    fn record_in(&mut self, message: &Message) {
        match message {
            Message::Text(raw) => self.in_text.record(1, raw.len()),
            Message::Binary(raw) => self.in_binary.record(1, raw.len()),
            Message::Ping(payload) => self.in_ping.record(0, payload.len()),
            Message::Pong(payload) => self.in_pong.record(0, payload.len()),
            Message::Close(_) => self.in_close.record(0, 0),
            Message::Frame(frame) => self.in_frame.record(0, frame.payload().len()),
        }
    }

    fn record_out(&mut self, kind: RelayOutKind, envelopes: usize, bytes: usize) {
        if kind.is_payload_batch() && envelopes == 0 && bytes == 0 {
            return;
        }
        match kind {
            RelayOutKind::Response => self.out_response.record(envelopes, bytes),
            RelayOutKind::FileTunnelBody => self.out_response.record(envelopes, bytes),
            RelayOutKind::PushOutput => self.out_push_output.record(envelopes, bytes),
            RelayOutKind::PushCwd => self.out_push_cwd.record(envelopes, bytes),
            RelayOutKind::PushResize => self.out_push_resize.record(envelopes, bytes),
            RelayOutKind::MuxKeepalive => self.out_mux_keepalive.record(envelopes, bytes),
            RelayOutKind::IdlePing => self.out_idle_ping.record(envelopes, bytes),
            RelayOutKind::Pong => self.out_pong.record(envelopes, bytes),
        }
    }

    fn record_send_error(&mut self) {
        self.send_errors = self.send_errors.saturating_add(1);
    }

    fn has_activity(&self) -> bool {
        !self.in_text.is_empty()
            || !self.in_binary.is_empty()
            || !self.in_ping.is_empty()
            || !self.in_pong.is_empty()
            || !self.in_close.is_empty()
            || !self.in_frame.is_empty()
            || !self.out_response.is_empty()
            || !self.out_push_output.is_empty()
            || !self.out_push_cwd.is_empty()
            || !self.out_push_resize.is_empty()
            || !self.out_mux_keepalive.is_empty()
            || !self.out_pong.is_empty()
            || self.send_errors > 0
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct RelayTransportDebugSnapshot {
    since_last_inbound_ms: u64,
    since_last_outbound_ms: u64,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct RelayHeartbeatDebug {
    last_inbound_at: Instant,
    last_outbound_at: Instant,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
}

#[cfg(test)]
impl RelayHeartbeatDebug {
    fn new(now: Instant) -> Self {
        Self {
            last_inbound_at: now,
            last_outbound_at: now,
            last_inbound_kind: "none",
            last_outbound_kind: "none",
        }
    }

    fn record_inbound(&mut self, kind: &'static str, _bytes: usize) {
        let now = Instant::now();
        self.last_inbound_at = now;
        self.last_inbound_kind = kind;
    }

    fn record_outbound(&mut self, kind: &'static str, _bytes: usize) {
        let now = Instant::now();
        self.last_outbound_at = now;
        self.last_outbound_kind = kind;
    }

    fn snapshot(&self) -> RelayTransportDebugSnapshot {
        RelayTransportDebugSnapshot {
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayOutKind {
    Response,
    FileTunnelBody,
    PushOutput,
    PushCwd,
    PushResize,
    #[cfg(test)]
    MuxKeepalive,
    #[cfg(test)]
    IdlePing,
    Pong,
}

impl RelayOutKind {
    #[cfg(test)]
    fn is_payload_batch(self) -> bool {
        // 空业务 batch 不会写 WebSocket；忽略它，避免把无输出的 watcher 唤醒记成流量。
        !matches!(self, Self::Pong | Self::IdlePing)
    }

    #[cfg(test)]
    fn uses_data_lane(self) -> bool {
        matches!(self, Self::PushOutput | Self::PushCwd | Self::PushResize)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::FileTunnelBody => "file_tunnel_body",
            Self::PushOutput => "push_output",
            Self::PushCwd => "push_cwd",
            Self::PushResize => "push_resize",
            #[cfg(test)]
            Self::MuxKeepalive => "mux_keepalive",
            #[cfg(test)]
            Self::IdlePing => "idle_ping",
            Self::Pong => "pong",
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
enum RelayMuxWrite {
    Envelopes {
        kind: RelayOutKind,
        client_id: Option<RelayClientId>,
        order: Option<u64>,
        envelopes: Vec<RelayMuxEnvelope>,
        completion: Option<oneshot::Sender<Result<(), RelayConnectorError>>>,
    },
    Raw {
        kind: RelayOutKind,
        message: Message,
    },
}

#[derive(Debug)]
enum RelayDataWrite {
    Wire {
        kind: RelayOutKind,
        messages: Vec<ProtocolWireMessage>,
    },
    Raw {
        kind: RelayOutKind,
        message: Message,
    },
}

impl RelayDataWrite {
    fn debug_snapshot(&self) -> RelayMuxWriteDebugSnapshot {
        match self {
            Self::Wire { kind, messages } => RelayMuxWriteDebugSnapshot {
                kind: *kind,
                #[cfg(test)]
                client_id: None,
                #[cfg(test)]
                order: None,
                envelopes: messages.len(),
                bytes: protocol_wire_messages_wire_len(messages),
                raw: false,
            },
            Self::Raw { kind, message } => RelayMuxWriteDebugSnapshot {
                kind: *kind,
                #[cfg(test)]
                client_id: None,
                #[cfg(test)]
                order: None,
                envelopes: 0,
                bytes: relay_message_bytes(message),
                raw: true,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RelayMuxWriteDebugSnapshot {
    kind: RelayOutKind,
    #[cfg(test)]
    client_id: Option<RelayClientId>,
    #[cfg(test)]
    order: Option<u64>,
    envelopes: usize,
    bytes: usize,
    raw: bool,
}

#[cfg(test)]
impl RelayMuxWrite {
    fn debug_snapshot(&self) -> RelayMuxWriteDebugSnapshot {
        match self {
            Self::Envelopes {
                kind,
                client_id,
                order,
                envelopes,
                completion: _,
            } => RelayMuxWriteDebugSnapshot {
                kind: *kind,
                client_id: *client_id,
                order: *order,
                envelopes: envelopes.len(),
                bytes: relay_mux_envelopes_wire_len(envelopes),
                raw: false,
            },
            Self::Raw { kind, message } => RelayMuxWriteDebugSnapshot {
                kind: *kind,
                client_id: None,
                order: None,
                envelopes: 0,
                bytes: relay_message_bytes(message),
                raw: true,
            },
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct RelayMuxWriterQueues {
    control: mpsc::Sender<RelayMuxWrite>,
    data: mpsc::Sender<RelayMuxWrite>,
    ordering: RelayMuxOrdering,
}

#[cfg(test)]
impl RelayMuxWriterQueues {
    fn new() -> (
        Self,
        mpsc::Receiver<RelayMuxWrite>,
        mpsc::Receiver<RelayMuxWrite>,
    ) {
        let (control_tx, control_rx) =
            mpsc::channel::<RelayMuxWrite>(RELAY_MUX_CONTROL_QUEUE_CAPACITY);
        let (data_tx, data_rx) = mpsc::channel::<RelayMuxWrite>(RELAY_MUX_DATA_QUEUE_CAPACITY);
        let ordering = Arc::new(Mutex::new(RelayMuxOrderingState::default()));
        (
            Self {
                control: control_tx,
                data: data_tx,
                ordering,
            },
            control_rx,
            data_rx,
        )
    }

    fn sender_for_kind(&self, kind: RelayOutKind) -> &mpsc::Sender<RelayMuxWrite> {
        if kind.uses_data_lane() {
            &self.data
        } else {
            &self.control
        }
    }

    fn ordering(&self) -> RelayMuxOrdering {
        self.ordering.clone()
    }
}

#[cfg(test)]
fn relay_mux_write_order(write: &RelayMuxWrite) -> Option<(RelayClientId, u64)> {
    match write {
        RelayMuxWrite::Envelopes {
            client_id: Some(client_id),
            order: Some(order),
            ..
        } => Some((*client_id, *order)),
        RelayMuxWrite::Envelopes { .. } | RelayMuxWrite::Raw { .. } => None,
    }
}

#[cfg(test)]
fn relay_mux_assign_order(ordering: &RelayMuxOrdering, client_id: RelayClientId) -> Option<u64> {
    let mut state = ordering.lock().ok()?;
    let entry = state.next_enqueue.entry(client_id).or_insert(0);
    let order = *entry;
    *entry = entry.saturating_add(1);
    let pending = state.pending.entry(client_id).or_insert(0);
    *pending = pending.saturating_add(1);
    Some(order)
}

#[cfg(test)]
fn relay_mux_write_is_sendable(write: &RelayMuxWrite, ordering: &RelayMuxOrdering) -> bool {
    let Some((client_id, order)) = relay_mux_write_order(write) else {
        return true;
    };
    ordering
        .lock()
        .is_ok_and(|state| state.next_send.get(&client_id).copied().unwrap_or(0) == order)
}

#[cfg(test)]
fn relay_mux_finish_order(
    ordering: &RelayMuxOrdering,
    active_clients: &RelayActiveClients,
    client_id: RelayClientId,
    order: u64,
) {
    if let Ok(mut state) = ordering.lock() {
        let next_send = state.next_send.entry(client_id).or_insert(0);
        if *next_send == order {
            *next_send = next_send.saturating_add(1);
        }
        if let Some(pending) = state.pending.get_mut(&client_id) {
            *pending = pending.saturating_sub(1);
            if *pending == 0 {
                state.pending.remove(&client_id);
                if !relay_client_is_active(active_clients, client_id) {
                    // 中文注释：client 断开且队列已清空时，释放该 client 的排序状态。
                    // 新 client 会拿新的 relay client_id；即使 id 被复用，也从 0 重新开始。
                    state.next_enqueue.remove(&client_id);
                    state.next_send.remove(&client_id);
                }
            }
        }
    }
}

#[cfg(test)]
fn relay_mux_pop_ready_deferred_write(
    deferred: &mut VecDeque<RelayMuxWrite>,
    ordering: &RelayMuxOrdering,
) -> Option<RelayMuxWrite> {
    // 中文注释：两条内部 lane 允许跨 client 插队，但同一 client 必须按加密序号发送。
    // 不同 client 的 E2EE sequence 独立，因此可以挑出第一个可发送项。
    let position = deferred
        .iter()
        .position(|write| relay_mux_write_is_sendable(write, ordering))?;
    deferred.remove(position)
}

#[cfg(test)]
fn relay_message_kind(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
        Message::Frame(_) => "frame",
    }
}

fn relay_message_bytes(message: &Message) -> usize {
    match message {
        Message::Text(raw) => raw.len(),
        Message::Binary(raw) => raw.len(),
        Message::Ping(payload) | Message::Pong(payload) => payload.len(),
        Message::Close(_) => 0,
        Message::Frame(frame) => frame.payload().len(),
    }
}

#[cfg(test)]
fn relay_mux_envelope_kind(envelope: &RelayMuxEnvelope) -> &'static str {
    match envelope {
        RelayMuxEnvelope::Keepalive { .. } => "keepalive",
        RelayMuxEnvelope::KeepaliveAck { .. } => "keepalive_ack",
        RelayMuxEnvelope::ClientConnected { .. } => "client_connected",
        RelayMuxEnvelope::ClientDisconnected { .. } => "client_disconnected",
        RelayMuxEnvelope::ClientFrame { .. } => "client_frame",
        RelayMuxEnvelope::DaemonFrame { .. } => "daemon_frame",
    }
}

#[cfg(test)]
fn relay_daemon_mux_idle_ping_enabled() -> bool {
    // daemon 是 relay mux 主干连接的 owner；空闲时由 daemon 主动发标准 WebSocket Ping。
    // 中文注释：Ping 只用于让代理/NAT 看见连接活动。不能把 Pong 当业务 ACK，也不能
    // 因为 Pong 被慢链路或旧 client 输出排队延迟，就主动判 relay 主干不可用。
    true
}

fn relay_idle_ping_due(
    now: Instant,
    last_activity: Instant,
    last_idle_ping_sent_at: Instant,
    heartbeat_interval: Duration,
) -> bool {
    // 中文注释：daemon -> relay 的 Ping 只用于保活出站方向。
    // 任何 control/data 写出都算 activity，必须重新等待完整 heartbeat interval。
    now.duration_since(last_activity) >= heartbeat_interval
        && now.duration_since(last_idle_ping_sent_at) >= heartbeat_interval
}

#[cfg(test)]
fn relay_daemon_mux_inbound_idle_timeout_enabled() -> bool {
    // daemon->relay 是一条长期主干连接：空闲时可能只有 daemon 发出的 WebSocket Ping，
    // relay/Pong 不进入业务数据流。这里不能因为“没有业务入站帧”主动断开，否则会让健康
    // 主干每 120s 自杀一次，并在 Web 侧表现成 relay 离线或操作超时。
    false
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
struct RelayWatcherCounts {
    output: usize,
    cwd: usize,
    resize: usize,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
struct RelayMuxDebugSnapshot {
    clients: usize,
    packet_mode_clients: usize,
    attached_sessions: usize,
    watched_sessions: usize,
    terminal_streams: usize,
    zero_credit_terminal_streams: usize,
    total_output_credit: u64,
    pending_raw_chunks: usize,
    pending_terminal_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayReconnectPolicy {
    initial_delay: Duration,
    max_delay: Duration,
    heartbeat_interval: Duration,
}

impl RelayReconnectPolicy {
    pub fn from_config(config: RelayReconnectConfig) -> Self {
        let initial_delay =
            duration_from_millis_floor(config.initial_delay_ms, MIN_RELAY_RETRY_DELAY_MS);
        let configured_max =
            duration_from_millis_floor(config.max_delay_ms, MIN_RELAY_RETRY_DELAY_MS);
        let max_delay = configured_max.max(initial_delay);
        let heartbeat_interval = duration_from_millis_floor(
            config.heartbeat_interval_ms,
            MIN_RELAY_HEARTBEAT_INTERVAL_MS,
        );

        Self {
            initial_delay,
            max_delay,
            heartbeat_interval,
        }
    }

    pub fn first_retry_delay(self) -> Duration {
        self.initial_delay
    }

    pub fn next_retry_delay(self, current: Duration) -> Duration {
        current
            .checked_mul(2)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
            .max(self.initial_delay)
    }

    pub fn heartbeat_interval(self) -> Duration {
        self.heartbeat_interval
    }
}

impl Default for RelayReconnectPolicy {
    fn default() -> Self {
        Self::from_config(RelayReconnectConfig::default())
    }
}

fn duration_from_millis_floor(value: u64, floor_ms: u64) -> Duration {
    Duration::from_millis(value.max(floor_ms))
}

fn relay_websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(RELAY_MAX_MESSAGE_SIZE),
        max_frame_size: Some(RELAY_MAX_FRAME_SIZE),
        ..WebSocketConfig::default()
    }
}

#[derive(Debug, Error)]
pub enum RelayConnectorError {
    #[error("unsupported relay URL; expected ws://host:port or wss://host:port")]
    UnsupportedUrl,
    #[error("relay daemon mux websocket connect timed out")]
    ConnectTimeout,
    #[error("failed to connect relay daemon mux websocket")]
    ConnectFailed,
    #[error("relay route_ready timed out")]
    RouteReadyTimeout,
    #[error("relay websocket receive failed")]
    ReceiveFailed,
    #[error("relay websocket send timed out")]
    SendTimeout,
    #[error("relay websocket send failed")]
    SendFailed,
    #[error("relay mux keepalive ack timed out")]
    MuxKeepaliveTimeout,
    #[error("relay websocket idle timeout")]
    IdleTimeout,
    #[error("relay mux envelope is invalid")]
    InvalidEnvelope,
    #[error("relay mux frame is invalid")]
    InvalidFrame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayBaseUrl {
    scheme: RelayUrlScheme,
    authority: String,
    base_path: RelayBasePath,
}

impl RelayBaseUrl {
    pub fn parse(value: &str) -> Result<Self, RelayConnectorError> {
        let (scheme, rest) = if let Some(rest) = value.strip_prefix("ws://") {
            (RelayUrlScheme::Ws, rest)
        } else if let Some(rest) = value.strip_prefix("wss://") {
            (RelayUrlScheme::Wss, rest)
        } else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };
        if rest.is_empty() || rest.contains('?') || rest.contains('#') {
            return Err(RelayConnectorError::UnsupportedUrl);
        }

        let (authority, raw_path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, Some(path)),
            None => (rest, None),
        };
        validate_authority(authority)?;
        let base_path = RelayBasePath::parse(raw_path)?;
        Ok(Self {
            scheme,
            authority: authority.to_owned(),
            base_path,
        })
    }

    /// 返回去掉尾随斜杠后的 canonical endpoint 形式，便于配置层做去重。
    pub fn canonical_url(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme.as_str(),
            self.authority,
            self.base_path.canonical_suffix()
        )
    }

    pub fn daemon_mux_url(&self, _server_id: ServerId) -> String {
        self.unified_ws_url()
    }

    pub fn daemon_mux_url_with_auth(
        &self,
        _server_id: ServerId,
        auth_token: Option<&str>,
    ) -> String {
        self.unified_ws_url_with_auth(auth_token)
    }

    pub fn client_url_template(&self) -> String {
        self.client_url_template_with_auth(None)
    }

    pub fn client_url_template_with_auth(&self, auth_token: Option<&str>) -> String {
        self.unified_ws_url_with_auth(auth_token)
    }

    fn unified_ws_url(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme.as_str(),
            self.authority,
            self.base_path.endpoint_suffix()
        )
    }

    fn unified_ws_url_with_auth(&self, auth_token: Option<&str>) -> String {
        let base = self.unified_ws_url();
        match auth_token {
            Some(auth_token) => format!(
                "{base}?relay_token={}",
                percent_encode_query_value(auth_token)
            ),
            None => base,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayUrlScheme {
    Ws,
    Wss,
}

impl RelayUrlScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Wss => "wss",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayBasePath {
    canonical_suffix: String,
    endpoint_suffix: String,
}

impl RelayBasePath {
    fn parse(raw_path: Option<&str>) -> Result<Self, RelayConnectorError> {
        let Some(raw_path) = raw_path else {
            return Ok(Self {
                canonical_suffix: String::new(),
                endpoint_suffix: "/ws".to_owned(),
            });
        };
        let trimmed = raw_path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(Self {
                canonical_suffix: String::new(),
                endpoint_suffix: "/ws".to_owned(),
            });
        }
        if trimmed.contains("//")
            || trimmed
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            || trimmed.ends_with("/client")
            || trimmed.ends_with("/daemon")
            || trimmed.ends_with("/daemon-mux")
        {
            return Err(RelayConnectorError::UnsupportedUrl);
        }

        // 允许 relay 被反向代理到公开前缀，例如 `/termd/ws`。
        // 公开 base path 必须以 `ws` 结尾，避免把完整业务 path 误当成 relay base。
        if trimmed != "ws" && !trimmed.ends_with("/ws") {
            return Err(RelayConnectorError::UnsupportedUrl);
        }

        Ok(Self {
            canonical_suffix: format!("/{trimmed}"),
            endpoint_suffix: format!("/{trimmed}"),
        })
    }

    fn canonical_suffix(&self) -> &str {
        &self.canonical_suffix
    }

    fn endpoint_suffix(&self) -> &str {
        &self.endpoint_suffix
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayProxyUrl {
    scheme: RelayProxyScheme,
    authority: String,
}

impl RelayProxyUrl {
    pub fn parse(value: &str) -> Result<Self, RelayConnectorError> {
        let trimmed = value.trim();
        let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("http://") {
            (RelayProxyScheme::Http, rest)
        } else if let Some(rest) = trimmed.strip_prefix("socks5://") {
            (RelayProxyScheme::Socks5, rest)
        } else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };

        if rest.is_empty() || rest.contains('?') || rest.contains('#') {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        let authority = rest.trim_end_matches('/');
        if authority.contains('/') || authority_contains_credentials(authority) {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        validate_proxy_authority(authority)?;

        Ok(Self {
            scheme,
            authority: authority.to_owned(),
        })
    }

    pub fn canonical_url(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority)
    }

    pub fn scheme(&self) -> RelayProxyScheme {
        self.scheme
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayProxyScheme {
    Http,
    Socks5,
}

impl RelayProxyScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        }
    }
}

pub async fn connect_relay_mux(
    relay_url: &str,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    connect_relay_mux_with_auth(relay_url, None, protocol).await
}

pub async fn connect_relay_mux_with_auth(
    relay_url: &str,
    auth_token: Option<&str>,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    connect_relay_mux_base(base, auth_token, protocol).await
}

pub async fn connect_relay_mux_base(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    connect_relay_control_base_once(
        base,
        auth_token,
        None,
        protocol,
        RelayReconnectPolicy::default().heartbeat_interval(),
    )
    .await
}

pub async fn run_relay_mux_with_reconnect(
    relay_url: &str,
    auth_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    run_relay_mux_with_reconnect_base(base, auth_token, proxy, policy, protocol).await
}

pub async fn run_relay_mux_with_reconnect_base(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let mut retry_delay = policy.first_retry_delay();

    loop {
        let attempt_started_at = Instant::now();
        let result = connect_relay_control_base_once(
            base.clone(),
            auth_token,
            proxy.clone(),
            protocol.clone(),
            policy.heartbeat_interval(),
        )
        .await;

        match &result {
            Ok(()) => warn!(
                retry_delay_ms = retry_delay.as_millis(),
                "relay daemon mux closed; reconnecting after backoff"
            ),
            Err(error) => warn!(
                %error,
                retry_delay_ms = retry_delay.as_millis(),
                "relay daemon mux failed; reconnecting after backoff"
            ),
        }

        tokio::time::sleep(retry_delay).await;
        retry_delay = if attempt_started_at.elapsed() >= RELAY_RECONNECT_STABLE_RESET_AFTER {
            // mux 曾经稳定存活过，下一次应按快速重连处理，避免一次长连接后的偶发断线
            // 还沿用之前的最大 backoff。
            policy.first_retry_delay()
        } else {
            policy.next_retry_delay(retry_delay)
        };
    }
}

async fn connect_relay_control_base_once(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    heartbeat_interval: Duration,
) -> Result<(), RelayConnectorError> {
    let server_id = { protocol.lock().await.server_id() };
    let relay_endpoint = base.canonical_url();
    let url = base.daemon_mux_url_with_auth(server_id, auth_token);
    // 中文注释：同一条 daemon control 生命周期内派生出的 data pipe 必须共享同一个
    // route_generation；relay 依赖它拒绝上一代 mux 迟到接入的 idle data pipe。
    let route_generation = relay_route_nonce();
    let (mut sender, mut receiver) = connect_relay_route_socket(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonControl,
        Some(route_generation.clone()),
        None,
        None,
    )
    .await?;
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_activity = Instant::now();
    let mut last_idle_ping_sent_at = Instant::now();
    let mut idle_ping_nonce = 0_u64;
    let mut data_tasks = RelayDataTaskMap::new();
    let mut idle_data_tasks = RelayIdleDataTaskMap::new();
    let mut idle_data_connecting = HashSet::<u64>::new();
    let mut idle_data_waiting = HashSet::<u64>::new();
    let mut next_idle_data_task_id = 1_u64;
    let mut next_idle_data_refill_at = Instant::now();
    let mut idle_data_refill_delay = RELAY_IDLE_DATA_REFILL_MIN_DELAY;
    let (idle_data_event_tx, mut idle_data_event_rx) = mpsc::channel::<RelayIdleDataEvent>(32);
    ensure_relay_idle_data_pool(
        &base,
        auth_token,
        &proxy,
        protocol.clone(),
        server_id,
        &route_generation,
        &mut idle_data_tasks,
        &mut idle_data_connecting,
        &mut idle_data_waiting,
        &mut next_idle_data_task_id,
        &idle_data_event_tx,
        Instant::now(),
        next_idle_data_refill_at,
    );

    let result = loop {
        prune_finished_relay_data_tasks(&mut data_tasks);
        if prune_finished_relay_idle_data_tasks(
            &mut idle_data_tasks,
            &mut idle_data_connecting,
            &mut idle_data_waiting,
        ) {
            schedule_relay_idle_data_refill_backoff(
                &mut next_idle_data_refill_at,
                &mut idle_data_refill_delay,
            );
        }
        ensure_relay_idle_data_pool(
            &base,
            auth_token,
            &proxy,
            protocol.clone(),
            server_id,
            &route_generation,
            &mut idle_data_tasks,
            &mut idle_data_connecting,
            &mut idle_data_waiting,
            &mut next_idle_data_task_id,
            &idle_data_event_tx,
            Instant::now(),
            next_idle_data_refill_at,
        );
        tokio::select! {
            biased;

            inbound = receiver.next() => {
                let Some(message) = inbound else {
                    break Ok(());
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(
                            relay = %relay_endpoint,
                            ?server_id,
                            %error,
                            "relay daemon control receive failed"
                        );
                        break Err(RelayConnectorError::ReceiveFailed);
                    }
                };
                match message {
                    Message::Text(raw) => {
                        last_activity = Instant::now();
                        let envelope: RelayControlEnvelope = match serde_json::from_str(raw.as_str()) {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        if let Err(error) = handle_relay_control_envelope(
                            envelope,
                            base.clone(),
                            auth_token.map(str::to_owned),
                            proxy.clone(),
                            protocol.clone(),
                            server_id,
                            route_generation.clone(),
                            &mut data_tasks,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    Message::Binary(raw) => {
                        last_activity = Instant::now();
                        let envelope: RelayControlEnvelope = match serde_json::from_slice(&raw) {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        if let Err(error) = handle_relay_control_envelope(
                            envelope,
                            base.clone(),
                            auth_token.map(str::to_owned),
                            proxy.clone(),
                            protocol.clone(),
                            server_id,
                            route_generation.clone(),
                            &mut data_tasks,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    Message::Ping(payload) => {
                        if let Err(error) = send_relay_message_with_deadline(
                            &mut sender,
                            Message::Pong(payload),
                            RELAY_PONG_DEADLINE,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    Message::Pong(_) => {
                        last_activity = Instant::now();
                    }
                    Message::Close(_) => break Ok(()),
                    Message::Frame(_) => {}
                }
            }
            _ = heartbeat.tick() => {
                let now = Instant::now();
                if relay_idle_ping_due(now, last_activity, last_idle_ping_sent_at, heartbeat_interval) {
                    idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
                    let payload = idle_ping_nonce.to_be_bytes().to_vec();
                    if let Err(error) = send_relay_message_with_deadline(
                        &mut sender,
                        Message::Ping(payload),
                        RELAY_SEND_DEADLINE,
                    )
                    .await
                    {
                        break Err(error);
                    }
                    last_activity = Instant::now();
                    last_idle_ping_sent_at = last_activity;
                    trace!(
                        relay = %relay_endpoint,
                        ?server_id,
                        "relay daemon control idle ping sent"
                    );
                }
            }
            maybe_event = idle_data_event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(RelayConnectorError::ReceiveFailed);
                };
                handle_relay_idle_data_event(
                    &mut idle_data_tasks,
                    &mut idle_data_connecting,
                    &mut idle_data_waiting,
                    event,
                    &mut next_idle_data_refill_at,
                    &mut idle_data_refill_delay,
                );
            }
            _ = tokio::time::sleep_until(next_idle_data_refill_at), if relay_idle_data_pool_refill_waiting(&idle_data_connecting, &idle_data_waiting, next_idle_data_refill_at) => {}
        }
    };

    let aborted = abort_all_relay_data_tasks(&mut data_tasks);
    let aborted_idle = abort_all_relay_idle_data_tasks(&mut idle_data_tasks);
    if aborted > 0 {
        debug!(
            relay = %relay_endpoint,
            ?server_id,
            aborted,
            "relay daemon control aborted data pipes on shutdown"
        );
    }
    if aborted_idle > 0 {
        debug!(
            relay = %relay_endpoint,
            ?server_id,
            aborted = aborted_idle,
            "relay daemon control aborted idle data pipes on shutdown"
        );
    }
    result
}

async fn handle_relay_control_envelope(
    envelope: RelayControlEnvelope,
    base: RelayBaseUrl,
    auth_token: Option<String>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    route_generation: ProtoNonce,
    data_tasks: &mut RelayDataTaskMap,
) -> Result<(), RelayConnectorError> {
    match envelope {
        RelayControlEnvelope::OpenData {
            client_id,
            data_token,
        } => {
            prune_finished_relay_data_tasks(data_tasks);
            if abort_relay_data_task(data_tasks, client_id) {
                debug!(
                    client_id = client_id.0,
                    "relay daemon control aborted stale data pipe before replacement"
                );
            }
            debug!(
                client_id = client_id.0,
                "relay daemon control requested data pipe"
            );
            let task = tokio::spawn(async move {
                if let Err(error) = run_relay_data_connection(
                    base,
                    auth_token,
                    proxy,
                    protocol,
                    server_id,
                    route_generation,
                    client_id,
                    data_token,
                )
                .await
                {
                    warn!(
                        client_id = client_id.0,
                        %error,
                        "relay daemon data pipe closed"
                    );
                }
            });
            data_tasks.insert(client_id, task);
        }
        RelayControlEnvelope::ClientDisconnected { client_id } => {
            if abort_relay_data_task(data_tasks, client_id) {
                debug!(
                    client_id = client_id.0,
                    "relay daemon control aborted data pipe after client disconnect"
                );
            } else {
                debug!(
                    client_id = client_id.0,
                    "relay daemon control observed client disconnect"
                );
            }
        }
        RelayControlEnvelope::DataReady => {
            // 中文注释：data_ready 只允许 daemon data pipe 发给 relay；control 线收到说明
            // 对端版本或路由异常。这里忽略，避免影响 control 长连接生命周期。
            debug!("relay daemon control ignored unexpected data_ready");
        }
    }
    Ok(())
}

fn prune_finished_relay_data_tasks(data_tasks: &mut RelayDataTaskMap) {
    data_tasks.retain(|_, task| !task.is_finished());
}

fn abort_relay_data_task(data_tasks: &mut RelayDataTaskMap, client_id: RelayClientId) -> bool {
    let Some(task) = data_tasks.remove(&client_id) else {
        return false;
    };
    task.abort();
    true
}

fn abort_all_relay_data_tasks(data_tasks: &mut RelayDataTaskMap) -> usize {
    let aborted = data_tasks.len();
    for (_, task) in data_tasks.drain() {
        task.abort();
    }
    aborted
}

fn prune_finished_relay_idle_data_tasks(
    idle_data_tasks: &mut RelayIdleDataTaskMap,
    idle_data_connecting: &mut HashSet<u64>,
    idle_data_waiting: &mut HashSet<u64>,
) -> bool {
    let mut removed_available_task = false;
    idle_data_tasks.retain(|task_id, task| {
        let keep = !task.is_finished();
        if !keep {
            if idle_data_connecting.remove(task_id) || idle_data_waiting.remove(task_id) {
                removed_available_task = true;
            }
        }
        keep
    });
    removed_available_task
}

fn abort_all_relay_idle_data_tasks(idle_data_tasks: &mut RelayIdleDataTaskMap) -> usize {
    let aborted = idle_data_tasks.len();
    for (_, task) in idle_data_tasks.drain() {
        task.abort();
    }
    aborted
}

fn handle_relay_idle_data_event(
    idle_data_tasks: &mut RelayIdleDataTaskMap,
    idle_data_connecting: &mut HashSet<u64>,
    idle_data_waiting: &mut HashSet<u64>,
    event: RelayIdleDataEvent,
    next_refill_at: &mut Instant,
    refill_delay: &mut Duration,
) {
    match event {
        RelayIdleDataEvent::Ready { task_id } => {
            if idle_data_tasks.contains_key(&task_id) && !idle_data_waiting.contains(&task_id) {
                let was_connecting = idle_data_connecting.remove(&task_id);
                if !was_connecting
                    && relay_idle_data_pool_slots(idle_data_connecting, idle_data_waiting)
                        >= RELAY_IDLE_DATA_POOL_TARGET
                {
                    if let Some(task) = idle_data_tasks.remove(&task_id) {
                        task.abort();
                    }
                    debug!(
                        task_id,
                        idle_connecting = idle_data_connecting.len(),
                        idle_waiting = idle_data_waiting.len(),
                        "relay daemon idle data pipe closed after surplus ready"
                    );
                    return;
                }
                idle_data_waiting.insert(task_id);
                reset_relay_idle_data_refill_backoff(next_refill_at, refill_delay);
                debug!(
                    task_id,
                    idle_connecting = idle_data_connecting.len(),
                    idle_waiting = idle_data_waiting.len(),
                    "relay daemon idle data pipe ready"
                );
            }
        }
        RelayIdleDataEvent::Assigned { task_id, client_id } => {
            idle_data_waiting.remove(&task_id);
            schedule_relay_idle_data_refill_after_assignment(next_refill_at, refill_delay);
            debug!(
                task_id,
                client_id = client_id.0,
                idle_waiting = idle_data_waiting.len(),
                "relay daemon idle data pipe assigned"
            );
        }
        RelayIdleDataEvent::Closed { task_id } => {
            let was_connecting = idle_data_connecting.remove(&task_id);
            let was_waiting = idle_data_waiting.remove(&task_id);
            idle_data_tasks.remove(&task_id);
            if was_connecting || was_waiting {
                schedule_relay_idle_data_refill_backoff(next_refill_at, refill_delay);
            } else {
                reset_relay_idle_data_refill_backoff(next_refill_at, refill_delay);
            }
            debug!(
                task_id,
                idle_connecting = idle_data_connecting.len(),
                idle_waiting = idle_data_waiting.len(),
                was_connecting,
                was_waiting,
                "relay daemon idle data pipe task closed"
            );
        }
    }
}

fn reset_relay_idle_data_refill_backoff(next_refill_at: &mut Instant, refill_delay: &mut Duration) {
    *next_refill_at = Instant::now();
    *refill_delay = RELAY_IDLE_DATA_REFILL_MIN_DELAY;
}

fn schedule_relay_idle_data_refill_after_assignment(
    next_refill_at: &mut Instant,
    refill_delay: &mut Duration,
) {
    // 中文注释：短连接常在数百毫秒内把同一条 data pipe 回收到 idle 池。
    // assignment 后立刻补池会制造多余 TLS/WebSocket 连接；延迟补池让短切换优先复用。
    *next_refill_at = Instant::now() + RELAY_IDLE_DATA_REFILL_MIN_DELAY;
    *refill_delay = RELAY_IDLE_DATA_REFILL_MIN_DELAY;
}

fn schedule_relay_idle_data_refill_backoff(
    next_refill_at: &mut Instant,
    refill_delay: &mut Duration,
) {
    let now = Instant::now();
    *next_refill_at = now + *refill_delay;
    *refill_delay = refill_delay
        .saturating_mul(2)
        .min(RELAY_IDLE_DATA_REFILL_MAX_DELAY);
}

fn relay_idle_data_pool_refill_waiting(
    idle_data_connecting: &HashSet<u64>,
    idle_data_waiting: &HashSet<u64>,
    next_refill_at: Instant,
) -> bool {
    RELAY_IDLE_DATA_POOL_TARGET > 0
        && relay_idle_data_pool_slots(idle_data_connecting, idle_data_waiting)
            < RELAY_IDLE_DATA_POOL_TARGET
        && Instant::now() < next_refill_at
}

fn relay_idle_data_pool_slots(
    idle_data_connecting: &HashSet<u64>,
    idle_data_waiting: &HashSet<u64>,
) -> usize {
    idle_data_connecting.len() + idle_data_waiting.len()
}

#[allow(clippy::too_many_arguments)]
fn ensure_relay_idle_data_pool(
    base: &RelayBaseUrl,
    auth_token: Option<&str>,
    proxy: &Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    route_generation: &ProtoNonce,
    idle_data_tasks: &mut RelayIdleDataTaskMap,
    idle_data_connecting: &mut HashSet<u64>,
    idle_data_waiting: &mut HashSet<u64>,
    next_idle_data_task_id: &mut u64,
    idle_data_event_tx: &mpsc::Sender<RelayIdleDataEvent>,
    now: Instant,
    next_refill_at: Instant,
) {
    if RELAY_IDLE_DATA_POOL_TARGET == 0 || now < next_refill_at {
        return;
    }
    while relay_idle_data_pool_slots(idle_data_connecting, idle_data_waiting)
        < RELAY_IDLE_DATA_POOL_TARGET
    {
        let task_id = *next_idle_data_task_id;
        *next_idle_data_task_id = (*next_idle_data_task_id).saturating_add(1);
        idle_data_connecting.insert(task_id);
        let task = tokio::spawn(run_relay_idle_data_connection(
            base.clone(),
            auth_token.map(str::to_owned),
            proxy.clone(),
            protocol.clone(),
            server_id,
            route_generation.clone(),
            task_id,
            idle_data_event_tx.clone(),
        ));
        idle_data_tasks.insert(task_id, task);
        debug!(
            task_id,
            idle_connecting = idle_data_connecting.len(),
            idle_waiting = idle_data_waiting.len(),
            "relay daemon started idle data pipe"
        );
    }
}

async fn run_relay_data_connection(
    base: RelayBaseUrl,
    auth_token: Option<String>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    route_generation: ProtoNonce,
    client_id: RelayClientId,
    data_token: ProtoNonce,
) -> Result<(), RelayConnectorError> {
    let relay_endpoint = base.canonical_url();
    let url = base.daemon_mux_url_with_auth(server_id, auth_token.as_deref());
    let (sender, mut receiver) = connect_relay_route_socket_with_timeout(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonData,
        Some(route_generation),
        Some(client_id),
        Some(data_token),
        RELAY_DATA_CONNECT_TIMEOUT,
    )
    .await?;
    let _ = run_relay_established_data_connection(
        relay_endpoint,
        protocol,
        server_id,
        client_id,
        sender,
        &mut receiver,
    )
    .await?;
    Ok(())
}

async fn run_relay_idle_data_connection(
    base: RelayBaseUrl,
    auth_token: Option<String>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    route_generation: ProtoNonce,
    task_id: u64,
    idle_data_event_tx: mpsc::Sender<RelayIdleDataEvent>,
) {
    let relay_endpoint = base.canonical_url();
    let result: Result<(), RelayConnectorError> = async {
        let url = base.daemon_mux_url_with_auth(server_id, auth_token.as_deref());
        let (mut sender, mut receiver) = connect_relay_route_socket_with_timeout(
            &url,
            proxy.as_ref(),
            server_id,
            ProtoRouteRole::DaemonData,
            Some(route_generation),
            None,
            None,
            RELAY_IDLE_DATA_CONNECT_TIMEOUT,
        )
        .await?;
        let _ = idle_data_event_tx
            .send(RelayIdleDataEvent::Ready { task_id })
            .await;
        debug!(
            relay = %relay_endpoint,
            ?server_id,
            task_id,
            "relay daemon idle data pipe connected"
        );
        loop {
            let (client_id, _data_token) = read_relay_idle_data_assignment(
                &relay_endpoint,
                task_id,
                &mut sender,
                &mut receiver,
            )
            .await?;
            let _ = idle_data_event_tx
                .send(RelayIdleDataEvent::Assigned { task_id, client_id })
                .await;
            debug!(
                relay = %relay_endpoint,
                ?server_id,
                task_id,
                client_id = client_id.0,
                data_token_present = true,
                "relay daemon idle data pipe received assignment"
            );
            match run_relay_established_data_connection(
                relay_endpoint.clone(),
                protocol.clone(),
                server_id,
                client_id,
                sender,
                &mut receiver,
            )
            .await?
            {
                RelayEstablishedDataOutcome::SocketClosed => break Ok(()),
            }
        }
    }
    .await;

    match result {
        Ok(()) => {
            let _ = idle_data_event_tx
                .send(RelayIdleDataEvent::Closed { task_id })
                .await;
        }
        Err(error) => {
            debug!(
                relay = %base.canonical_url(),
                ?server_id,
                task_id,
                %error,
                "relay daemon idle data pipe stopped"
            );
            let _ = idle_data_event_tx
                .send(RelayIdleDataEvent::Closed { task_id })
                .await;
        }
    }
}

async fn run_relay_established_data_connection(
    relay_endpoint: String,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    client_id: RelayClientId,
    sender: RelaySender,
    receiver: &mut RelayReceiver,
) -> Result<RelayEstablishedDataOutcome, RelayConnectorError> {
    let (write_tx, write_rx) = mpsc::channel::<RelayDataWrite>(RELAY_DATA_WIRE_QUEUE_CAPACITY);
    let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel::<()>(1);
    let (writer_stop_tx, writer_stop_rx) = oneshot::channel();
    let writer_task = tokio::spawn(run_relay_data_writer(
        relay_endpoint.clone(),
        client_id,
        write_rx,
        writer_failed_tx,
        sender,
        writer_stop_rx,
    ));

    let (connection, initial_messages) = {
        let protocol = protocol.lock().await;
        protocol.start_connection()
    };
    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();
    connections.insert(client_id, connection);
    let initial_wire_messages = initial_messages
        .into_iter()
        .map(ProtocolWireMessage::Json)
        .collect::<Vec<_>>();
    enqueue_relay_data_wire(&write_tx, RelayOutKind::Response, initial_wire_messages).await?;

    let (push_event_tx, mut push_event_rx) =
        mpsc::channel::<RelayPushEvent>(RELAY_PUSH_EVENT_QUEUE_CAPACITY);
    let mut pending_push_events = RelayPushEventQueue::default();
    let mut push_drain_wake_pending = false;
    let mut watched_output_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_cwd_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_resize_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watcher_tasks = HashMap::<RelayClientId, Vec<JoinHandle<()>>>::new();

    let result = loop {
        tokio::select! {
            biased;

            inbound = receiver.next() => {
                let Some(message) = inbound else {
                    break Ok(RelayEstablishedDataEnd::SocketClosed);
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            %error,
                            "relay daemon data receive failed"
                        );
                        break Err(RelayConnectorError::ReceiveFailed);
                    }
                };
                match message {
                    Message::Ping(payload) => {
                        let control = decode_relay_data_control(&payload);
                        if let Some(control) = control {
                            match control {
                                RelayControlEnvelope::ClientDisconnected { client_id: disconnected_client_id }
                                    if disconnected_client_id == client_id =>
                                {
                                    debug!(
                                        relay = %relay_endpoint,
                                        client_id = client_id.0,
                                        "relay daemon data received client disconnect"
                                    );
                                    break Ok(RelayEstablishedDataEnd::ClientDisconnected);
                                }
                                RelayControlEnvelope::ClientDisconnected { client_id: disconnected_client_id } => {
                                    warn!(
                                        relay = %relay_endpoint,
                                        client_id = client_id.0,
                                        disconnected_client_id = disconnected_client_id.0,
                                        "relay daemon data ignored mismatched client disconnect"
                                    );
                                }
                                RelayControlEnvelope::OpenData { .. } | RelayControlEnvelope::DataReady => {
                                    debug!(
                                        relay = %relay_endpoint,
                                        client_id = client_id.0,
                                        ?control,
                                        "relay daemon data ignored unexpected control ping"
                                    );
                                }
                            }
                        }
                        let queued = try_enqueue_relay_data_raw(
                            &write_tx,
                            RelayOutKind::Pong,
                            Message::Pong(payload),
                        )?;
                        if !queued {
                            trace!(
                                relay = %relay_endpoint,
                                client_id = client_id.0,
                                "relay daemon data dropped pong because writer queue is full"
                            );
                        }
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => break Ok(RelayEstablishedDataEnd::SocketClosed),
                    other => {
                        let Some(inbound) = relay_data_message_to_inbound(other)? else {
                            break Ok(RelayEstablishedDataEnd::SocketClosed);
                        };
                        let wire_message = match inbound {
                            RelayDataInbound::Wire(wire_message) => wire_message,
                        };
                        if let ProtocolWireMessage::Binary(raw) = &wire_message
                            && let Some(RelayHttpTunnelFrame::RequestHead { method, path, headers }) =
                                decode_relay_http_tunnel_frame(raw)
                        {
                            debug!(
                                relay = %relay_endpoint,
                                client_id = client_id.0,
                                method = %method,
                                path = %path,
                                headers = headers.len(),
                                "relay daemon data received HTTP tunnel request head"
                            );
                            break handle_relay_http_tunnel_stream(
                                &relay_endpoint,
                                protocol.clone(),
                                client_id,
                                method,
                                path,
                                headers,
                                &write_tx,
                                receiver,
                                &mut writer_failed_rx,
                            )
                            .await;
                        }
                        let responses = {
                            let Some(connection) = connections.get_mut(&client_id) else {
                                break Ok(RelayEstablishedDataEnd::SocketClosed);
                            };
                            let mut protocol = protocol.lock().await;
                            connection.handle_wire_message(&mut protocol, wire_message)
                        };
                        if let Some(connection) = connections.get_mut(&client_id) {
                            queue_relay_deferred_output_wakeups(
                                client_id,
                                connection,
                                &mut pending_push_events,
                            );
                        }
                        enqueue_relay_data_wire(&write_tx, RelayOutKind::Response, responses)
                            .await?;
                        let initial_output_sessions = sync_relay_watchers_for_client(
                            Some(client_id),
                            &connections,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_cwd_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        )
                        .await;
                        queue_relay_initial_output_events(
                            Some(client_id),
                            &initial_output_sessions,
                            &mut pending_push_events,
                        );
                        drain_relay_data_push_events(
                            &relay_endpoint,
                            server_id,
                            client_id,
                            &protocol,
                            &mut connections,
                            &mut pending_push_events,
                            &write_tx,
                            &mut push_drain_wake_pending,
                        )
                        .await?;
                    }
                }
            }
            maybe_failed = writer_failed_rx.recv() => {
                if maybe_failed.is_some() {
                    break Err(RelayConnectorError::SendFailed);
                }
                break Ok(RelayEstablishedDataEnd::SocketClosed);
            }
            maybe_event = push_event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Ok(RelayEstablishedDataEnd::SocketClosed);
                };
                pending_push_events.enqueue(event);
                drain_relay_data_push_events(
                    &relay_endpoint,
                    server_id,
                    client_id,
                    &protocol,
                    &mut connections,
                    &mut pending_push_events,
                    &write_tx,
                    &mut push_drain_wake_pending,
                )
                .await?;
            }
            _ = tokio::time::sleep(RELAY_PUSH_DRAIN_RETRY_DELAY), if push_drain_wake_pending => {
                push_drain_wake_pending = false;
                drain_relay_data_push_events(
                    &relay_endpoint,
                    server_id,
                    client_id,
                    &protocol,
                    &mut connections,
                    &mut pending_push_events,
                    &write_tx,
                    &mut push_drain_wake_pending,
                )
                .await?;
            }
        }
    };

    if let Some(mut connection) = connections.remove(&client_id) {
        let mut protocol = protocol.lock().await;
        connection.close(&mut protocol);
    }
    drop_relay_client_runtime(
        client_id,
        &mut pending_push_events,
        &mut watched_output_sessions,
        &mut watched_cwd_sessions,
        &mut watched_resize_sessions,
        &mut watcher_tasks,
    );
    abort_relay_watcher_tasks(watcher_tasks);
    match result {
        Ok(RelayEstablishedDataEnd::ClientDisconnected) => {
            // 中文注释：client 断开后不再复用当前 daemon data pipe。
            // relay 侧 control/data 分队列可能让旧 client 的 frame 残留在同一条
            // WebSocket 上；关闭整条 data 线能从机制上避免旧 frame 污染下一次 attach/upload。
            drop(write_tx);
            let _ = writer_stop_tx.send(());
            let _ = writer_task.await;
            Ok(RelayEstablishedDataOutcome::SocketClosed)
        }
        Ok(RelayEstablishedDataEnd::SocketClosed) => {
            drop(write_tx);
            writer_task.abort();
            let _ = writer_task.await;
            Ok(RelayEstablishedDataOutcome::SocketClosed)
        }
        Err(error) => {
            drop(write_tx);
            writer_task.abort();
            let _ = writer_task.await;
            Err(error)
        }
    }
}

async fn read_relay_idle_data_assignment(
    relay_endpoint: &str,
    task_id: u64,
    sender: &mut RelaySender,
    receiver: &mut RelayReceiver,
) -> Result<(RelayClientId, ProtoNonce), RelayConnectorError> {
    loop {
        let Some(message) = receiver.next().await else {
            return Err(RelayConnectorError::ReceiveFailed);
        };
        let message = message.map_err(|_| RelayConnectorError::ReceiveFailed)?;
        match message {
            Message::Text(raw) => {
                let envelope: RelayControlEnvelope = serde_json::from_str(raw.as_str())
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                if let RelayControlEnvelope::OpenData {
                    client_id,
                    data_token,
                } = envelope
                {
                    return Ok((client_id, data_token));
                }
                if matches!(
                    envelope,
                    RelayControlEnvelope::ClientDisconnected { .. }
                        | RelayControlEnvelope::DataReady
                ) {
                    continue;
                }
                return Err(RelayConnectorError::InvalidEnvelope);
            }
            Message::Binary(raw) => {
                let envelope: RelayControlEnvelope = serde_json::from_slice(&raw)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                if let RelayControlEnvelope::OpenData {
                    client_id,
                    data_token,
                } = envelope
                {
                    return Ok((client_id, data_token));
                }
                if matches!(
                    envelope,
                    RelayControlEnvelope::ClientDisconnected { .. }
                        | RelayControlEnvelope::DataReady
                ) {
                    continue;
                }
                return Err(RelayConnectorError::InvalidEnvelope);
            }
            Message::Ping(payload) => {
                send_relay_message_with_deadline(
                    sender,
                    Message::Pong(payload.clone()),
                    RELAY_PONG_DEADLINE,
                )
                .await?;
                if let Some(envelope) = decode_relay_data_control(&payload) {
                    if let RelayControlEnvelope::OpenData {
                        client_id,
                        data_token,
                    } = envelope
                    {
                        return Ok((client_id, data_token));
                    }
                    if matches!(
                        envelope,
                        RelayControlEnvelope::ClientDisconnected { .. }
                            | RelayControlEnvelope::DataReady
                    ) {
                        continue;
                    }
                    return Err(RelayConnectorError::InvalidEnvelope);
                }
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Err(RelayConnectorError::ReceiveFailed),
        }
        trace!(
            relay = %relay_endpoint,
            task_id,
            "relay daemon idle data pipe ignored non-assignment frame"
        );
    }
}

async fn handle_relay_http_tunnel_stream(
    relay_endpoint: &str,
    protocol: SharedDaemonProtocol,
    client_id: RelayClientId,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    write_tx: &mpsc::Sender<RelayDataWrite>,
    receiver: &mut RelayReceiver,
    writer_failed_rx: &mut mpsc::Receiver<()>,
) -> Result<RelayEstablishedDataEnd, RelayConnectorError> {
    debug!(
        relay = %relay_endpoint,
        client_id = client_id.0,
        method = %method,
        path = %path,
        headers = headers.len(),
        "relay daemon HTTP tunnel stream started"
    );
    let (body_tx, body_rx) =
        mpsc::channel::<Result<Bytes, io::Error>>(RELAY_DATA_WIRE_QUEUE_CAPACITY);
    let body_stream = futures_util::stream::unfold(body_rx, |mut body_rx| async move {
        body_rx.recv().await.map(|item| (item, body_rx))
    });
    let request_body = Body::from_stream(body_stream);
    let response_write_tx = write_tx.clone();
    let abort_response_on_client_disconnect = is_relay_http_tunnel_download(&method, &path);
    let mut response_task = tokio::spawn(send_relay_http_tunnel_response(
        protocol,
        method,
        path,
        headers,
        request_body,
        response_write_tx,
    ));
    let mut body_tx = Some(body_tx);
    let mut request_open = true;
    let mut response_done = false;

    loop {
        tokio::select! {
            biased;

            response = &mut response_task, if !response_done => {
                match response {
                    Ok(Ok(())) => {
                        debug!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            "relay daemon HTTP tunnel response task completed; waiting for relay close"
                        );
                        response_done = true;
                        body_tx.take();
                        continue;
                    }
                    Ok(Err(error)) => {
                        warn!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            %error,
                            "relay daemon HTTP tunnel response task failed"
                        );
                        return Err(error);
                    }
                    Err(error) => {
                        warn!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            %error,
                            "relay daemon HTTP tunnel response task panicked"
                        );
                        return Err(RelayConnectorError::SendFailed);
                    }
                };
            }
            maybe_failed = writer_failed_rx.recv() => {
                if !response_done {
                    cleanup_relay_http_tunnel_response_task(
                        &mut body_tx,
                        response_task,
                        abort_response_on_client_disconnect,
                    )
                    .await;
                }
                if maybe_failed.is_some() {
                    warn!(
                        relay = %relay_endpoint,
                        client_id = client_id.0,
                        "relay daemon HTTP tunnel writer failed"
                    );
                    return Err(RelayConnectorError::SendFailed);
                }
                debug!(
                    relay = %relay_endpoint,
                    client_id = client_id.0,
                    "relay daemon HTTP tunnel writer closed"
                );
                return Ok(RelayEstablishedDataEnd::SocketClosed);
            }
            inbound = receiver.next() => {
                let Some(message) = inbound else {
                    if !response_done {
                        cleanup_relay_http_tunnel_response_task(
                            &mut body_tx,
                            response_task,
                            abort_response_on_client_disconnect,
                        )
                        .await;
                    }
                    return Ok(RelayEstablishedDataEnd::SocketClosed);
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            %error,
                            "relay daemon HTTP tunnel receive failed"
                        );
                        if !response_done {
                            cleanup_relay_http_tunnel_response_task(
                                &mut body_tx,
                                response_task,
                                abort_response_on_client_disconnect,
                            )
                            .await;
                        }
                        return Err(RelayConnectorError::ReceiveFailed);
                    }
                };
                match message {
                    Message::Ping(payload) => {
                        let control = decode_relay_data_control(&payload);
                        if let Some(RelayControlEnvelope::ClientDisconnected {
                            client_id: disconnected_client_id,
                        }) = control
                        {
                            if !response_done {
                                cleanup_relay_http_tunnel_response_task(
                                    &mut body_tx,
                                    response_task,
                                    abort_response_on_client_disconnect,
                                )
                                .await;
                            }
                            if disconnected_client_id == client_id {
                                return Ok(RelayEstablishedDataEnd::ClientDisconnected);
                            }
                            warn!(
                                relay = %relay_endpoint,
                                client_id = client_id.0,
                                disconnected_client_id = disconnected_client_id.0,
                                "relay daemon HTTP tunnel ignored mismatched client disconnect"
                            );
                            return Ok(RelayEstablishedDataEnd::SocketClosed);
                        }
                        let queued = try_enqueue_relay_data_raw(
                            write_tx,
                            RelayOutKind::Pong,
                            Message::Pong(payload),
                        )?;
                        if !queued {
                            trace!(
                                relay = %relay_endpoint,
                                client_id = client_id.0,
                                "relay daemon HTTP tunnel dropped pong because writer queue is full"
                            );
                        }
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        debug!(
                            relay = %relay_endpoint,
                            client_id = client_id.0,
                            "relay daemon HTTP tunnel received websocket close"
                        );
                        if !response_done {
                            cleanup_relay_http_tunnel_response_task(
                                &mut body_tx,
                                response_task,
                                abort_response_on_client_disconnect,
                            )
                            .await;
                        }
                        return Ok(RelayEstablishedDataEnd::SocketClosed);
                    }
                    other => {
                        let Some(inbound) = relay_data_message_to_inbound(other)? else {
                            continue;
                        };
                        if response_done {
                            trace!(
                                relay = %relay_endpoint,
                                client_id = client_id.0,
                                "relay daemon HTTP tunnel ignored frame after response"
                            );
                            continue;
                        }
                        match inbound {
                            RelayDataInbound::Wire(ProtocolWireMessage::Binary(raw)) => {
                                if !request_open {
                                    cleanup_relay_http_tunnel_response_task(
                                        &mut body_tx,
                                        response_task,
                                        abort_response_on_client_disconnect,
                                    )
                                    .await;
                                    return Err(RelayConnectorError::InvalidEnvelope);
                                }
                                match decode_relay_http_tunnel_frame(&raw) {
                                    Some(RelayHttpTunnelFrame::RequestBody { body }) => {
                                        trace!(
                                            relay = %relay_endpoint,
                                            client_id = client_id.0,
                                            bytes = body.len(),
                                            "relay daemon HTTP tunnel received request body chunk"
                                        );
                                        if let Some(tx) = body_tx.as_ref()
                                            && tx.send(Ok(Bytes::from(body))).await.is_err()
                                        {
                                            request_open = false;
                                            body_tx.take();
                                        }
                                    }
                                    Some(RelayHttpTunnelFrame::RequestEnd) => {
                                        debug!(
                                            relay = %relay_endpoint,
                                            client_id = client_id.0,
                                            "relay daemon HTTP tunnel received request end"
                                        );
                                        request_open = false;
                                        body_tx.take();
                                    }
                                    _ => {
                                        cleanup_relay_http_tunnel_response_task(
                                            &mut body_tx,
                                            response_task,
                                            abort_response_on_client_disconnect,
                                        )
                                        .await;
                                        return Err(RelayConnectorError::InvalidEnvelope);
                                    }
                                }
                            }
                            RelayDataInbound::Wire(ProtocolWireMessage::Json(_)) => {
                                cleanup_relay_http_tunnel_response_task(
                                    &mut body_tx,
                                    response_task,
                                    abort_response_on_client_disconnect,
                                )
                                .await;
                                return Err(RelayConnectorError::InvalidEnvelope);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn cleanup_relay_http_tunnel_response_task(
    body_tx: &mut Option<mpsc::Sender<Result<Bytes, io::Error>>>,
    response_task: JoinHandle<Result<(), RelayConnectorError>>,
    abort_response: bool,
) {
    // 中文注释：upload 断线时要关闭请求 body；daemon 端会保留已 patch 区间，
    // 后续显式 abort 或 idle prune 再清理未完成目标。
    // download 断线时 client 已经不存在，继续读文件/排队响应只会占住 data pipe。
    body_tx.take();
    if abort_response {
        response_task.abort();
    }
    let _ = response_task.await;
}

fn is_relay_http_tunnel_download(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path == "/api/files/download"
}

async fn send_relay_http_tunnel_response(
    protocol: SharedDaemonProtocol,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Body,
    write_tx: mpsc::Sender<RelayDataWrite>,
) -> Result<(), RelayConnectorError> {
    debug!(
        method = %method,
        path = %path,
        headers = headers.len(),
        "relay daemon HTTP tunnel handler started"
    );
    let response =
        handle_http_file_tunnel_stream_request(protocol, method, path, headers, body).await;
    let status = response.status().as_u16();
    debug!(status, "relay daemon HTTP tunnel handler returned response");
    enqueue_relay_data_raw(
        &write_tx,
        RelayOutKind::Response,
        Message::Binary(encode_relay_http_tunnel_response_head(status)),
    )
    .await?;
    debug!(status, "relay daemon HTTP tunnel response head queued");

    let mut body = response.into_body().into_data_stream();
    let mut chunks = 0_usize;
    let mut bytes = 0_usize;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| RelayConnectorError::ReceiveFailed)?;
        if chunk.is_empty() {
            continue;
        }
        chunks = chunks.saturating_add(1);
        bytes = bytes.saturating_add(chunk.len());
        enqueue_relay_data_raw(
            &write_tx,
            RelayOutKind::FileTunnelBody,
            Message::Binary(encode_relay_http_tunnel_response_body(chunk.to_vec())),
        )
        .await?;
    }
    enqueue_relay_data_raw(
        &write_tx,
        RelayOutKind::FileTunnelBody,
        Message::Binary(encode_relay_http_tunnel_response_end()),
    )
    .await?;
    debug!(
        status,
        chunks, bytes, "relay daemon HTTP tunnel response body queued"
    );
    Ok(())
}

async fn enqueue_relay_data_wire(
    tx: &mpsc::Sender<RelayDataWrite>,
    kind: RelayOutKind,
    messages: Vec<ProtocolWireMessage>,
) -> Result<usize, RelayConnectorError> {
    if messages.is_empty() {
        return Ok(0);
    }
    let bytes = protocol_wire_messages_wire_len(&messages);
    tx.send(RelayDataWrite::Wire { kind, messages })
        .await
        .map(|()| bytes)
        .map_err(|_| RelayConnectorError::SendFailed)
}

async fn enqueue_relay_data_raw(
    tx: &mpsc::Sender<RelayDataWrite>,
    kind: RelayOutKind,
    message: Message,
) -> Result<(), RelayConnectorError> {
    tx.send(RelayDataWrite::Raw { kind, message })
        .await
        .map_err(|_| RelayConnectorError::SendFailed)
}

fn try_enqueue_relay_data_raw(
    tx: &mpsc::Sender<RelayDataWrite>,
    kind: RelayOutKind,
    message: Message,
) -> Result<bool, RelayConnectorError> {
    match tx.try_send(RelayDataWrite::Raw { kind, message }) {
        Ok(()) => Ok(true),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(RelayConnectorError::SendFailed),
    }
}

fn try_reserve_relay_data_push_slot(
    tx: &mpsc::Sender<RelayDataWrite>,
) -> Result<Option<mpsc::Permit<'_, RelayDataWrite>>, RelayConnectorError> {
    match tx.try_reserve() {
        Ok(permit) => Ok(Some(permit)),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(None),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(RelayConnectorError::SendFailed),
    }
}

fn enqueue_relay_data_wire_with_permit(
    permit: mpsc::Permit<'_, RelayDataWrite>,
    kind: RelayOutKind,
    messages: Vec<ProtocolWireMessage>,
) -> usize {
    let bytes = protocol_wire_messages_wire_len(&messages);
    permit.send(RelayDataWrite::Wire { kind, messages });
    bytes
}

async fn run_relay_data_writer<S>(
    relay_endpoint: String,
    client_id: RelayClientId,
    mut write_rx: mpsc::Receiver<RelayDataWrite>,
    writer_failed_tx: mpsc::Sender<()>,
    mut sender: S,
    mut stop_rx: oneshot::Receiver<()>,
) -> RelayDataWriterOutcome<S>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    loop {
        tokio::select! {
            biased;

            _ = &mut stop_rx => {
                // 中文注释：client 已断开时，旧输出队列必须整体丢弃，不能继续写到
                // 已废弃的 data pipe；这里尽力发送 close，让 relay 尽快释放对应通道。
                let _ = send_relay_message(&mut sender, Message::Close(None), Some(RELAY_SEND_DEADLINE))
                    .await;
                return RelayDataWriterOutcome::Closed;
            }
            maybe_write = write_rx.recv() => {
                let Some(write) = maybe_write else {
                    return RelayDataWriterOutcome::Reusable(sender);
                };
                let snapshot = write.debug_snapshot();
                trace!(
                    relay = %relay_endpoint,
                    client_id = client_id.0,
                    kind = snapshot.kind.label(),
                    messages = snapshot.envelopes,
                    bytes = snapshot.bytes,
                    raw = snapshot.raw,
                    "relay daemon data writer dequeued frame"
                );
                let sent = tokio::select! {
                    biased;
                    _ = &mut stop_rx => {
                        // 中文注释：writer 在真正写出过程中收到 stop，说明旧 client 的 body
                        // 已经可能卡在不可取消的 send 阶段。此时不能把 sender 回收到 idle 池。
                        return RelayDataWriterOutcome::Closed;
                    }
                    sent = send_relay_data_write(&relay_endpoint, client_id, &mut sender, write) => sent,
                };
                if !sent {
                    let _ = writer_failed_tx.try_send(());
                    return RelayDataWriterOutcome::Closed;
                }
            }
        }
    }
}

async fn send_relay_data_write<S>(
    relay_endpoint: &str,
    client_id: RelayClientId,
    sender: &mut S,
    write: RelayDataWrite,
) -> bool
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match write {
        RelayDataWrite::Wire { kind, messages } => {
            send_relay_data_wire_messages(relay_endpoint, client_id, sender, messages, kind)
                .await
                .is_ok()
        }
        RelayDataWrite::Raw { kind, message } => {
            send_relay_message(sender, message, relay_send_deadline(kind))
                .await
                .is_ok()
        }
    }
}

async fn send_relay_data_wire_messages<S>(
    relay_endpoint: &str,
    client_id: RelayClientId,
    sender: &mut S,
    messages: Vec<ProtocolWireMessage>,
    kind: RelayOutKind,
) -> Result<usize, RelayConnectorError>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let message_count = messages.len();
    let mut bytes = 0_usize;
    let started_at = Instant::now();
    for message in messages {
        match message {
            ProtocolWireMessage::Json(envelope) => {
                let raw = serde_json::to_string(&envelope)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                bytes = bytes.saturating_add(raw.len());
                send_relay_message_with_deadline(
                    sender,
                    Message::Text(raw.into()),
                    RELAY_SEND_DEADLINE,
                )
                .await?;
            }
            ProtocolWireMessage::Binary(raw) => {
                let len = raw.len();
                send_relay_message_with_deadline(sender, Message::Binary(raw), RELAY_SEND_DEADLINE)
                    .await?;
                bytes = bytes.saturating_add(len);
            }
        }
    }
    log_relay_send(
        relay_endpoint,
        kind.label(),
        message_count,
        bytes,
        started_at.elapsed(),
        "relay daemon data send batch",
    );
    trace!(
        relay = %relay_endpoint,
        client_id = client_id.0,
        kind = kind.label(),
        messages = message_count,
        bytes,
        "relay daemon data wire batch sent"
    );
    Ok(bytes)
}

enum RelayDataInbound {
    Wire(ProtocolWireMessage),
}

fn relay_data_message_to_inbound(
    message: Message,
) -> Result<Option<RelayDataInbound>, RelayConnectorError> {
    match message {
        Message::Text(raw) => serde_json::from_str(raw.as_str())
            .map(|envelope| Some(RelayDataInbound::Wire(ProtocolWireMessage::Json(envelope))))
            .map_err(|_| RelayConnectorError::InvalidEnvelope),
        Message::Binary(raw) => Ok(Some(RelayDataInbound::Wire(ProtocolWireMessage::Binary(
            raw.to_vec(),
        )))),
        Message::Close(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
    }
}

async fn drain_relay_data_push_events(
    relay_endpoint: &str,
    server_id: ServerId,
    client_id: RelayClientId,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    pending_push_events: &mut RelayPushEventQueue,
    write_tx: &mpsc::Sender<RelayDataWrite>,
    push_drain_wake_pending: &mut bool,
) -> Result<(), RelayConnectorError> {
    let started_at = Instant::now();
    let mut drained_events = 0_usize;
    let mut sent_bytes = 0_usize;
    while pending_push_events.has_pending() {
        let Some(event) = pending_push_events.pop_front() else {
            break;
        };
        if event.client_id() != client_id {
            continue;
        }
        let (_, session_id, kind) = relay_push_event_parts(event);
        let Some(connection) = connections.get_mut(&client_id) else {
            continue;
        };
        let push_permit = match try_reserve_relay_data_push_slot(write_tx)? {
            Some(permit) => permit,
            None => {
                // 中文注释：terminal 输出走 data lane；当下行 writer 正在被慢网络/浏览器背压
                // 卡住时，不能在这里 await 容量，否则同一 data pipe 的 stdin/close 也读不到。
                pending_push_events.requeue_front(event);
                if pending_push_events.has_pending() {
                    *push_drain_wake_pending = true;
                }
                trace!(
                    relay = %relay_endpoint,
                    ?server_id,
                    client_id = client_id.0,
                    session_id = ?session_id,
                    kind = kind.label(),
                    queue_capacity = write_tx.capacity(),
                    queue_pending = pending_push_events.len(),
                    "relay data writer queue is full"
                );
                break;
            }
        };
        let responses = match kind {
            RelayOutKind::PushOutput => {
                let (lock_wait, messages) = {
                    let lock_started = Instant::now();
                    let mut protocol = protocol.lock().await;
                    let messages = connection.drain_session_output_messages_for_push(
                        &mut protocol,
                        session_id,
                        OUTPUT_FLUSH_MAX_BYTES_PER_SESSION,
                    );
                    (lock_started.elapsed(), messages)
                };
                if lock_wait >= RELAY_SEND_DEBUG_LOG_THRESHOLD {
                    debug!(
                        relay = %relay_endpoint,
                        ?server_id,
                        client_id = client_id.0,
                        session_id = ?session_id,
                        lock_wait_ms = lock_wait.as_millis(),
                        "relay data output collection latency"
                    );
                }
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::PushCwd => {
                let messages = {
                    let mut protocol = protocol.lock().await;
                    connection.read_session_cwd_update_messages(&mut protocol, session_id)
                };
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::PushResize => {
                let messages = {
                    let mut protocol = protocol.lock().await;
                    connection.read_session_resize_update_messages(&mut protocol, session_id)
                };
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::Response | RelayOutKind::FileTunnelBody | RelayOutKind::Pong => {
                Vec::new()
            }
            #[cfg(test)]
            RelayOutKind::MuxKeepalive | RelayOutKind::IdlePing => Vec::new(),
        };
        queue_relay_deferred_output_wakeups(client_id, connection, pending_push_events);
        let response_count = responses.len();
        if response_count == 0 {
            drop(push_permit);
            drained_events = drained_events.saturating_add(1);
            continue;
        }
        let bytes = enqueue_relay_data_wire_with_permit(push_permit, kind, responses);
        drained_events = drained_events.saturating_add(1);
        sent_bytes = sent_bytes.saturating_add(bytes);
        trace!(
            relay = %relay_endpoint,
            ?server_id,
            client_id = client_id.0,
            session_id = ?session_id,
            kind = kind.label(),
            response_count,
            bytes,
            queue_pending = pending_push_events.len(),
            "relay daemon data push batch queued"
        );
        if relay_push_drain_budget_exhausted(drained_events, sent_bytes, started_at) {
            log_relay_push_drain_reschedule(
                relay_endpoint,
                server_id,
                kind,
                drained_events,
                sent_bytes,
                pending_push_events.len(),
                started_at.elapsed(),
            );
            if pending_push_events.has_pending() {
                *push_drain_wake_pending = true;
            }
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
async fn connect_relay_mux_base_once(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    heartbeat_interval: Duration,
) -> Result<(), RelayConnectorError> {
    let server_id = { protocol.lock().await.server_id() };
    let relay_endpoint = base.canonical_url();
    let url = base.daemon_mux_url_with_auth(server_id, auth_token);
    // 中文注释：daemon 到 relay 只有一条主 WebSocket。
    // relay 仍用 route generation 判断新 mux 是否替换旧 mux，避免旧连接跨代污染新 client。
    let route_generation = relay_route_nonce();
    let (sender, mut control_receiver) = connect_relay_mux_socket(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonMux,
        route_generation.clone(),
    )
    .await?;
    let (writer_queues, writer_control_rx, writer_data_rx) = RelayMuxWriterQueues::new();
    let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel::<()>(1);
    let active_clients: RelayActiveClients = Arc::new(Mutex::new(HashSet::new()));
    // 中文注释：daemon->relay 是一条全双工 WebSocket。写 relay 可能因为公网链路慢而
    // 阻塞，必须放到独立 writer task；主循环继续读取 client 输入、关闭和新连接帧。
    let writer_task = tokio::spawn(run_relay_mux_writer(
        relay_endpoint.clone(),
        writer_control_rx,
        writer_data_rx,
        writer_failed_tx,
        sender,
        active_clients.clone(),
        writer_queues.ordering(),
    ));

    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();
    let (push_event_tx, mut push_event_rx) =
        mpsc::channel::<RelayPushEvent>(RELAY_PUSH_EVENT_QUEUE_CAPACITY);
    let mut push_drain_wake_pending = false;
    let mut watched_output_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_cwd_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_resize_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watcher_tasks = HashMap::<RelayClientId, Vec<JoinHandle<()>>>::new();
    let mut pending_push_events = RelayPushEventQueue::default();
    let mut idle_deadline = Instant::now() + RELAY_IDLE_TIMEOUT;
    let mut traffic = RelayTrafficCounters::default();
    let mut last_traffic_log = Instant::now();
    let mut heartbeat_debug = RelayHeartbeatDebug::new(Instant::now());
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_activity = Instant::now();
    let mut last_idle_ping_sent_at = Instant::now();
    let mut idle_ping_nonce: u64 = 0;

    let result = loop {
        // relay control frame 必须先于业务输出处理，避免大量 PTY 输出让 Ping/Pong 被调度延迟。
        // 中文注释：主循环只读 relay 入站和推进协议状态，写出统一交给 writer task。
        // 这样旧终端的大量 stdout 不会阻塞新 client 的握手、输入或断开清理。
        tokio::select! {
            biased;

            _ = tokio::time::sleep_until(idle_deadline), if relay_daemon_mux_inbound_idle_timeout_enabled() => {
                break Err(RelayConnectorError::IdleTimeout);
            }
            inbound = control_receiver.next() => {
                let Some(message) = inbound else {
                    break Ok(());
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(
                            relay = %relay_endpoint,
                            ?server_id,
                            %error,
                            heartbeat_debug = ?heartbeat_debug.snapshot(),
                            "relay daemon mux receive failed"
                        );
                        break Err(RelayConnectorError::ReceiveFailed);
                    }
                };
                idle_deadline = Instant::now() + RELAY_IDLE_TIMEOUT;
                traffic.record_in(&message);
                heartbeat_debug
                    .record_inbound(relay_message_kind(&message), relay_message_bytes(&message));
                trace!(
                    relay = %relay_endpoint,
                    ?server_id,
                    message_kind = relay_message_kind(&message),
                    message_bytes = relay_message_bytes(&message),
                    "relay daemon mux inbound frame received"
                );

                match message {
                    Message::Text(raw) => {
                        let envelope: RelayMuxEnvelope = match serde_json::from_str(raw.as_str()) {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        let client_id = relay_envelope_client_id(&envelope);
                        trace!(
                            relay = %relay_endpoint,
                            ?server_id,
                            client_id = client_id.map(|id| id.0),
                            envelope = relay_mux_envelope_kind(&envelope),
                            "relay daemon mux envelope decoded"
                        );
                        match handle_relay_mux_keepalive_control(
                            &relay_endpoint,
                            &writer_queues,
                            &active_clients,
                            &envelope,
                        )
                        .await
                        {
                            Ok(Some(bytes)) => {
                                if bytes > 0 {
                                    let now = Instant::now();
                                    last_activity = now;
                                    heartbeat_debug.record_outbound("mux_keepalive_ack", bytes);
                                    traffic.record_out(RelayOutKind::MuxKeepalive, 1, bytes);
                                }
                                maybe_log_relay_traffic(
                                    &relay_endpoint,
                                    &mut traffic,
                                    &mut last_traffic_log,
                                    &mut connections,
                                    relay_watcher_counts(
                                        &watched_output_sessions,
                                        &watched_cwd_sessions,
                                        &watched_resize_sessions,
                                    ),
                                    false,
                                );
                                continue;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                traffic.record_send_error();
                                break Err(error);
                            }
                        }
                        let responses = match handle_mux_envelope(envelope, &protocol, &mut connections, &active_clients).await {
                            Ok(responses) => responses,
                            Err(error) => break Err(error),
                        };
                        if let Some(client_id) = client_id {
                            if let Some(connection) = connections.get_mut(&client_id) {
                                queue_relay_deferred_output_wakeups(
                                    client_id,
                                    connection,
                                    &mut pending_push_events,
                                );
                            } else {
                                drop_relay_client_runtime(
                                    client_id,
                                    &mut pending_push_events,
                                    &mut watched_output_sessions,
                                    &mut watched_cwd_sessions,
                                    &mut watched_resize_sessions,
                                    &mut watcher_tasks,
                                );
                            }
                        }
                        let response_count = responses.len();
                        let response_bytes = match enqueue_relay_mux_envelopes(
                            &relay_endpoint,
                            &writer_queues,
                            &active_clients,
                            client_id,
                            RelayOutKind::Response,
                            responses,
                        ).await {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                traffic.record_send_error();
                                break Err(error);
                            }
                        };
                        if response_bytes > 0 {
                            let now = Instant::now();
                            last_activity = now;
                            heartbeat_debug.record_outbound("response", response_bytes);
                        }
                        traffic.record_out(
                            RelayOutKind::Response,
                            if response_bytes > 0 { response_count } else { 0 },
                            response_bytes,
                        );
                        let initial_output_sessions = sync_relay_watchers_for_client(
                            client_id,
                            &connections,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_cwd_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        )
                        .await;
                        queue_relay_initial_output_events(
                            client_id,
                            &initial_output_sessions,
                            &mut pending_push_events,
                        );
                        queue_relay_push_drain_wakeup(
                            &pending_push_events,
                            &mut push_drain_wake_pending,
                        );
                        maybe_log_relay_traffic(
                            &relay_endpoint,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connections,
                            relay_watcher_counts(
                                &watched_output_sessions,
                                &watched_cwd_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                    }
                    Message::Binary(raw) => {
                        let envelope: RelayMuxEnvelope = match decode_binary_relay_mux_envelope(&raw)
                            .or_else(|_| serde_json::from_slice(&raw).map_err(|_| ()))
                        {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        let client_id = relay_envelope_client_id(&envelope);
                        trace!(
                            relay = %relay_endpoint,
                            ?server_id,
                            client_id = client_id.map(|id| id.0),
                            envelope = relay_mux_envelope_kind(&envelope),
                            "relay daemon mux envelope decoded"
                        );
                        match handle_relay_mux_keepalive_control(
                            &relay_endpoint,
                            &writer_queues,
                            &active_clients,
                            &envelope,
                        )
                        .await
                        {
                            Ok(Some(bytes)) => {
                                if bytes > 0 {
                                    let now = Instant::now();
                                    last_activity = now;
                                    heartbeat_debug.record_outbound("mux_keepalive_ack", bytes);
                                    traffic.record_out(RelayOutKind::MuxKeepalive, 1, bytes);
                                }
                                maybe_log_relay_traffic(
                                    &relay_endpoint,
                                    &mut traffic,
                                    &mut last_traffic_log,
                                    &mut connections,
                                    relay_watcher_counts(
                                        &watched_output_sessions,
                                        &watched_cwd_sessions,
                                        &watched_resize_sessions,
                                    ),
                                    false,
                                );
                                continue;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                traffic.record_send_error();
                                break Err(error);
                            }
                        }
                        let responses = match handle_mux_envelope(envelope, &protocol, &mut connections, &active_clients).await {
                            Ok(responses) => responses,
                            Err(error) => break Err(error),
                        };
                        if let Some(client_id) = client_id {
                            if let Some(connection) = connections.get_mut(&client_id) {
                                queue_relay_deferred_output_wakeups(
                                    client_id,
                                    connection,
                                    &mut pending_push_events,
                                );
                            } else {
                                drop_relay_client_runtime(
                                    client_id,
                                    &mut pending_push_events,
                                    &mut watched_output_sessions,
                                    &mut watched_cwd_sessions,
                                    &mut watched_resize_sessions,
                                    &mut watcher_tasks,
                                );
                            }
                        }
                        let response_count = responses.len();
                        let response_bytes = match enqueue_relay_mux_envelopes(
                            &relay_endpoint,
                            &writer_queues,
                            &active_clients,
                            client_id,
                            RelayOutKind::Response,
                            responses,
                        ).await {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                traffic.record_send_error();
                                break Err(error);
                            }
                        };
                        if response_bytes > 0 {
                            let now = Instant::now();
                            last_activity = now;
                            heartbeat_debug.record_outbound("response", response_bytes);
                        }
                        traffic.record_out(
                            RelayOutKind::Response,
                            if response_bytes > 0 { response_count } else { 0 },
                            response_bytes,
                        );
                        let initial_output_sessions = sync_relay_watchers_for_client(
                            client_id,
                            &connections,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_cwd_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        )
                        .await;
                        queue_relay_initial_output_events(
                            client_id,
                            &initial_output_sessions,
                            &mut pending_push_events,
                        );
                        queue_relay_push_drain_wakeup(
                            &pending_push_events,
                            &mut push_drain_wake_pending,
                        );
                        maybe_log_relay_traffic(
                            &relay_endpoint,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connections,
                            relay_watcher_counts(
                                &watched_output_sessions,
                                &watched_cwd_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                    }
                    Message::Ping(payload) => {
                        let pong_bytes = payload.len();
                        if let Err(error) = enqueue_relay_control_message(
                            &writer_queues,
                            RelayOutKind::Pong,
                            Message::Pong(payload),
                        )
                        .await
                        {
                            traffic.record_send_error();
                            break Err(error);
                        }
                        // 中文注释：回复 relay 主动 Ping 只是一次本地写入尝试。
                        // 在 daemon->relay 半断时，这个 Pong 可能同样到不了 relay，
                        // 所以不能用它推迟 daemon 自己的 Ping/Pong 探测。
                        traffic.record_out(RelayOutKind::Pong, 0, pong_bytes);
                        maybe_log_relay_traffic(
                            &relay_endpoint,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connections,
                            relay_watcher_counts(
                                &watched_output_sessions,
                                &watched_cwd_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                    }
                    Message::Pong(payload) => {
                        // 中文注释：Pong 是标准 WebSocket 控制帧，只记录为 transport 活动。
                        // 这里不做“未收到 Pong 就断线”的 ACK 语义，避免慢 relay/旧 client
                        // 输出排队时误杀健康的 daemon->relay 主干。
                        let _ = payload;
                        last_activity = Instant::now();
                        maybe_log_relay_traffic(
                            &relay_endpoint,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connections,
                            relay_watcher_counts(
                                &watched_output_sessions,
                                &watched_cwd_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                    }
                    Message::Close(_) => break Ok(()),
                    Message::Frame(_) => {}
                }
            }
            maybe_failed = writer_failed_rx.recv() => {
                if maybe_failed.is_some() {
                    traffic.record_send_error();
                    warn!(
                        relay = %relay_endpoint,
                        ?server_id,
                        "relay mux writer reported failure"
                    );
                }
                break Err(RelayConnectorError::SendFailed);
            }
            _ = heartbeat.tick() => {
                let now = Instant::now();
                if relay_daemon_mux_idle_ping_enabled()
                    && relay_idle_ping_due(
                        now,
                        last_activity,
                        last_idle_ping_sent_at,
                        heartbeat_interval,
                    )
                {
                    idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
                    let payload = idle_ping_nonce.to_be_bytes().to_vec();
                    let ping_bytes = payload.len();
                    if let Err(error) = enqueue_relay_control_message(
                        &writer_queues,
                        RelayOutKind::IdlePing,
                        Message::Ping(payload),
                    )
                    .await
                    {
                        traffic.record_send_error();
                        break Err(error);
                    }
                    let sent_at = Instant::now();
                    last_idle_ping_sent_at = sent_at;
                    last_activity = sent_at;
                    traffic.record_out(RelayOutKind::IdlePing, 0, ping_bytes);
                    maybe_log_relay_traffic(
                        &relay_endpoint,
                        &mut traffic,
                        &mut last_traffic_log,
                        &mut connections,
                        relay_watcher_counts(
                            &watched_output_sessions,
                                                &watched_cwd_sessions,
                            &watched_resize_sessions,
                        ),
                        false,
                    );
                }
            }
            _ = tokio::time::sleep(RELAY_PUSH_DRAIN_RETRY_DELAY), if push_drain_wake_pending => {
                // 中文注释：内部 deferred output 不能再塞回 watcher 的有界 mpsc 队列自唤醒。
                // 高输出和快切时该队列可能正好满，若唤醒丢失，pending snapshot/tail 会停在
                // daemon mux 内。用 mux 主循环自己的定时唤醒承接下一轮 drain。
                push_drain_wake_pending = false;
                trace!(
                    relay = %relay_endpoint,
                    ?server_id,
                    queue_pending = pending_push_events.len(),
                    "relay mux deferred push drain wakeup fired"
                );
                match drain_relay_push_events(
                    &relay_endpoint,
                    server_id,
                    &protocol,
                    &mut connections,
                    &mut pending_push_events,
                    &writer_queues,
                    &active_clients,
                    &mut traffic,
                    &mut push_drain_wake_pending,
                )
                .await
                {
                    Ok(sent) => {
                        if sent {
                            let now = Instant::now();
                            last_activity = now;
                            heartbeat_debug.record_outbound("push", 0);
                        }
                    }
                    Err(error) => {
                        traffic.record_send_error();
                        break Err(error);
                    }
                }
                maybe_log_relay_traffic(
                    &relay_endpoint,
                    &mut traffic,
                    &mut last_traffic_log,
                    &mut connections,
                    relay_watcher_counts(
                        &watched_output_sessions,
                        &watched_cwd_sessions,
                        &watched_resize_sessions,
                    ),
                    false,
                );
            }
            maybe_event = push_event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(RelayConnectorError::SendFailed);
                };
                idle_deadline = Instant::now() + RELAY_IDLE_TIMEOUT;
                push_drain_wake_pending = false;
                trace!(
                    relay = %relay_endpoint,
                    ?server_id,
                    client_id = event.client_id().0,
                    session_id = ?event.session_id(),
                    event = event.label(),
                    queue_pending_before = pending_push_events.len(),
                    "relay mux push event received from watcher"
                );
                pending_push_events.enqueue(event);
                match drain_relay_push_events(
                    &relay_endpoint,
                    server_id,
                    &protocol,
                    &mut connections,
                    &mut pending_push_events,
                    &writer_queues,
                    &active_clients,
                    &mut traffic,
                    &mut push_drain_wake_pending,
                )
                .await
                {
                    Ok(sent) => {
                        if sent {
                            let now = Instant::now();
                            last_activity = now;
                            heartbeat_debug.record_outbound("push", 0);
                        }
                    }
                    Err(error) => {
                        traffic.record_send_error();
                        break Err(error);
                    }
                }
                maybe_log_relay_traffic(
                    &relay_endpoint,
                    &mut traffic,
                    &mut last_traffic_log,
                    &mut connections,
                    relay_watcher_counts(
                        &watched_output_sessions,
                        &watched_cwd_sessions,
                        &watched_resize_sessions,
                    ),
                    false,
                );
            }
        }
    };

    maybe_log_relay_traffic(
        &relay_endpoint,
        &mut traffic,
        &mut last_traffic_log,
        &mut connections,
        relay_watcher_counts(
            &watched_output_sessions,
            &watched_cwd_sessions,
            &watched_resize_sessions,
        ),
        true,
    );

    abort_relay_watcher_tasks(watcher_tasks);
    close_relay_connections(protocol, connections).await;
    drop(writer_queues);
    writer_task.abort();
    result
}

#[cfg(test)]
fn relay_watcher_counts(
    watched_output_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
    watched_cwd_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
) -> RelayWatcherCounts {
    RelayWatcherCounts {
        output: watched_output_sessions.values().map(HashSet::len).sum(),
        cwd: watched_cwd_sessions.values().map(HashSet::len).sum(),
        resize: watched_resize_sessions.values().map(HashSet::len).sum(),
    }
}

#[cfg(test)]
fn relay_mux_debug_snapshot(
    connections: &HashMap<RelayClientId, ProtocolConnection>,
) -> RelayMuxDebugSnapshot {
    let mut snapshot = RelayMuxDebugSnapshot {
        clients: connections.len(),
        ..RelayMuxDebugSnapshot::default()
    };

    for connection in connections.values() {
        let connection_snapshot = connection.debug_snapshot();
        if connection_snapshot.packet_mode {
            snapshot.packet_mode_clients = snapshot.packet_mode_clients.saturating_add(1);
        }
        snapshot.attached_sessions = snapshot
            .attached_sessions
            .saturating_add(connection_snapshot.attached_sessions);
        snapshot.watched_sessions = snapshot
            .watched_sessions
            .saturating_add(connection_snapshot.watched_sessions);
        snapshot.terminal_streams = snapshot
            .terminal_streams
            .saturating_add(connection_snapshot.terminal_streams);
        snapshot.zero_credit_terminal_streams = snapshot
            .zero_credit_terminal_streams
            .saturating_add(connection_snapshot.zero_credit_terminal_streams);
        snapshot.total_output_credit = snapshot
            .total_output_credit
            .saturating_add(connection_snapshot.total_output_credit);
        snapshot.pending_raw_chunks = snapshot
            .pending_raw_chunks
            .saturating_add(connection_snapshot.pending_raw_chunks);
        snapshot.pending_terminal_frames = snapshot
            .pending_terminal_frames
            .saturating_add(connection_snapshot.pending_terminal_frames);
    }

    snapshot
}

#[cfg(test)]
fn relay_protocol_debug_traffic(
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
) -> ProtocolConnectionDebugTraffic {
    let mut traffic = ProtocolConnectionDebugTraffic::default();
    for connection in connections.values_mut() {
        traffic.merge(connection.take_debug_traffic());
    }
    traffic
}

#[cfg(test)]
fn maybe_log_relay_traffic(
    relay_endpoint: &str,
    traffic: &mut RelayTrafficCounters,
    last_logged_at: &mut Instant,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    watchers: RelayWatcherCounts,
    force: bool,
) {
    if !traffic.has_activity() {
        return;
    }
    if !force && last_logged_at.elapsed() < RELAY_TRAFFIC_LOG_INTERVAL {
        return;
    }

    let flow = relay_mux_debug_snapshot(connections);
    let protocol_traffic = relay_protocol_debug_traffic(connections);
    if relay_traffic_should_promote_to_info(traffic, &protocol_traffic) {
        info_relay_traffic(relay_endpoint, traffic, &protocol_traffic, watchers, flow);
    } else {
        debug_relay_traffic(relay_endpoint, traffic, &protocol_traffic, watchers, flow);
    }
    *traffic = RelayTrafficCounters::default();
    *last_logged_at = Instant::now();
}

#[cfg(test)]
fn relay_traffic_should_promote_to_info(
    traffic: &RelayTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
) -> bool {
    // relay 是所有公网 client 共享的单条 daemon mux；只把异常放大、背压或发送错误提升到 info。
    traffic.send_errors > 0
        || traffic.out_push_output.calls > 1_000
        || traffic.out_response.calls > 1_000
        || protocol_traffic.inbound_flow_packets > 200
        || protocol_traffic.method_count_exceeds(20)
        || protocol_traffic.inbound_stream_chunks > 100
        || protocol_traffic.outbound_stream_chunks > 100
}

#[cfg(test)]
fn info_relay_traffic(
    relay_endpoint: &str,
    traffic: &RelayTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    watchers: RelayWatcherCounts,
    flow: RelayMuxDebugSnapshot,
) {
    info!(
        relay = relay_endpoint,
        ?traffic,
        ?protocol_traffic,
        watchers_output = watchers.output,
        watchers_cwd = watchers.cwd,
        watchers_resize = watchers.resize,
        flow_clients = flow.clients,
        flow_packet_mode_clients = flow.packet_mode_clients,
        flow_attached_sessions = flow.attached_sessions,
        flow_watched_sessions = flow.watched_sessions,
        flow_terminal_streams = flow.terminal_streams,
        flow_zero_credit_terminal_streams = flow.zero_credit_terminal_streams,
        flow_total_output_credit = flow.total_output_credit,
        flow_pending_raw_chunks = flow.pending_raw_chunks,
        flow_pending_terminal_frames = flow.pending_terminal_frames,
        "relay mux traffic counters"
    );
}

#[cfg(test)]
fn debug_relay_traffic(
    relay_endpoint: &str,
    traffic: &RelayTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    watchers: RelayWatcherCounts,
    flow: RelayMuxDebugSnapshot,
) {
    trace!(
        relay = relay_endpoint,
        ?traffic,
        ?protocol_traffic,
        watchers_output = watchers.output,
        watchers_cwd = watchers.cwd,
        watchers_resize = watchers.resize,
        flow_clients = flow.clients,
        flow_packet_mode_clients = flow.packet_mode_clients,
        flow_attached_sessions = flow.attached_sessions,
        flow_watched_sessions = flow.watched_sessions,
        flow_terminal_streams = flow.terminal_streams,
        flow_zero_credit_terminal_streams = flow.zero_credit_terminal_streams,
        flow_total_output_credit = flow.total_output_credit,
        flow_pending_raw_chunks = flow.pending_raw_chunks,
        flow_pending_terminal_frames = flow.pending_terminal_frames,
        "relay mux traffic counters"
    );
}

#[cfg(test)]
async fn connect_relay_mux_socket(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: ProtoNonce,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    connect_relay_route_socket(
        url,
        proxy,
        server_id,
        role,
        Some(route_generation),
        None,
        None,
    )
    .await
}

async fn connect_relay_route_socket(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: Option<ProtoNonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<ProtoNonce>,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    connect_relay_route_socket_with_timeout(
        url,
        proxy,
        server_id,
        role,
        route_generation,
        client_id,
        data_token,
        RELAY_CONNECT_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn connect_relay_route_socket_with_timeout(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: Option<ProtoNonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<ProtoNonce>,
    connect_timeout: Duration,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    let (socket, _) = connect_relay_websocket(url, proxy, connect_timeout).await?;
    let (mut sender, mut receiver) = socket.split();
    send_route_hello(
        &mut sender,
        server_id,
        role,
        route_generation,
        client_id,
        data_token,
    )
    .await?;
    read_route_ready(&mut sender, &mut receiver, server_id, role).await?;
    Ok((sender, receiver))
}

async fn send_route_hello(
    sender: &mut RelaySender,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: Option<ProtoNonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<ProtoNonce>,
) -> Result<(), RelayConnectorError> {
    let envelope = ProtoEnvelope::new(
        ProtoMessageType::RouteHello,
        ProtoRouteHelloPayload {
            server_id,
            role,
            protocol_version: ProtoProtocolVersion(PROTOCOL_PACKET_VERSION),
            nonce: relay_route_nonce(),
            route_generation,
            client_id,
            data_token,
            timestamp_ms: current_unix_timestamp_millis(),
        },
    );
    let raw = serde_json::to_string(&envelope).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    send_relay_message_with_deadline(sender, Message::Text(raw.into()), RELAY_SEND_DEADLINE).await
}

async fn read_route_ready(
    sender: &mut RelaySender,
    receiver: &mut RelayReceiver,
    expected_server_id: ServerId,
    expected_role: ProtoRouteRole,
) -> Result<(), RelayConnectorError> {
    let route_deadline = Instant::now() + RELAY_ROUTE_READY_TIMEOUT;
    loop {
        let Some(message) = tokio::time::timeout_at(route_deadline, receiver.next())
            .await
            .map_err(|_| RelayConnectorError::RouteReadyTimeout)?
        else {
            return Err(RelayConnectorError::ReceiveFailed);
        };
        let message = message.map_err(|_| RelayConnectorError::ReceiveFailed)?;

        match message {
            Message::Text(raw) => {
                let envelope: JsonEnvelope = serde_json::from_str(raw.as_str())
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                validate_route_ready(envelope, expected_server_id, expected_role)?;
                return Ok(());
            }
            Message::Binary(raw) => {
                let envelope: JsonEnvelope = serde_json::from_slice(&raw)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                validate_route_ready(envelope, expected_server_id, expected_role)?;
                return Ok(());
            }
            Message::Ping(payload) => {
                send_relay_message_with_deadline(
                    sender,
                    Message::Pong(payload),
                    RELAY_PONG_DEADLINE,
                )
                .await?;
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Err(RelayConnectorError::ReceiveFailed),
        }
    }
}

fn validate_route_ready(
    envelope: JsonEnvelope,
    expected_server_id: ServerId,
    expected_role: ProtoRouteRole,
) -> Result<(), RelayConnectorError> {
    if envelope.kind != ProtoMessageType::RouteReady {
        return Err(RelayConnectorError::InvalidEnvelope);
    }
    let payload: ProtoRouteReadyPayload = serde_json::from_value(envelope.payload)
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    if payload.server_id != expected_server_id || payload.role != expected_role {
        return Err(RelayConnectorError::InvalidEnvelope);
    }
    Ok(())
}

fn relay_route_nonce() -> ProtoNonce {
    ProtoNonce(format!("relay-route-{}", ServerId::new().0))
}

#[cfg(test)]
async fn handle_relay_mux_keepalive_control(
    relay_endpoint: &str,
    writer_queues: &RelayMuxWriterQueues,
    active_clients: &RelayActiveClients,
    envelope: &RelayMuxEnvelope,
) -> Result<Option<usize>, RelayConnectorError> {
    match envelope {
        RelayMuxEnvelope::KeepaliveAck { nonce } => {
            // 中文注释：旧 relay/daemon 可能仍会发送 mux ack。新模型不再用应用层
            // ACK 判活，收到后只记录并丢弃，避免慢输出时因为 ack 排队误杀主干连接。
            debug!(nonce = *nonce, "relay daemon mux keepalive ack ignored");
            Ok(Some(0))
        }
        RelayMuxEnvelope::Keepalive { nonce } => {
            // 中文注释：兼容旧 relay 主动发来的 mux keepalive；新 daemon 自己不再发送
            // 需要 ack 的 mux keepalive，长期保活只依赖 WebSocket ping/pong 和真实读写错误。
            let bytes = enqueue_relay_mux_envelopes(
                relay_endpoint,
                writer_queues,
                active_clients,
                None,
                RelayOutKind::MuxKeepalive,
                vec![RelayMuxEnvelope::KeepaliveAck { nonce: *nonce }],
            )
            .await?;
            Ok(Some(bytes))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
async fn handle_mux_envelope(
    envelope: RelayMuxEnvelope,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    active_clients: &RelayActiveClients,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    match envelope {
        RelayMuxEnvelope::Keepalive { .. } | RelayMuxEnvelope::KeepaliveAck { .. } => {
            // mux keepalive 是 transport lifecycle 信号，正常会在主循环里提前消费。
            // 这里保留空处理，避免单测或旧调用路径把它误送进 session 协议。
            Ok(Vec::new())
        }
        RelayMuxEnvelope::ClientConnected { client_id } => {
            let (connection, initial_messages) = {
                let protocol = protocol.lock().await;
                protocol.start_connection()
            };
            connections.insert(client_id, connection);
            mark_relay_client_active(active_clients, client_id);
            debug!(
                client_id = client_id.0,
                "relay client connected to daemon mux"
            );
            client_envelopes(client_id, initial_messages)
        }
        RelayMuxEnvelope::ClientDisconnected { client_id } => {
            mark_relay_client_inactive(active_clients, client_id);
            if let Some(mut connection) = connections.remove(&client_id) {
                let mut protocol = protocol.lock().await;
                connection.close(&mut protocol);
            }
            debug!(
                client_id = client_id.0,
                "relay client disconnected from daemon mux"
            );
            Ok(Vec::new())
        }
        RelayMuxEnvelope::ClientFrame { client_id, frame } => {
            if !connections.contains_key(&client_id) {
                warn!(
                    client_id = client_id.0,
                    "dropping relay frame for unknown client"
                );
                return Ok(Vec::new());
            };

            trace!(
                client_id = client_id.0,
                frame_kind = relay_opaque_frame_kind(&frame),
                frame_bytes = relay_opaque_frame_transport_bytes(&frame),
                "relay client frame received by daemon mux"
            );
            let frame = match wire_message_from_mux_frame(frame) {
                Ok(frame) => frame,
                Err(error) => {
                    // relay client 是非可信输入源；坏业务 frame 只能影响该 client，不能杀掉
                    // daemon outbound connector 或 direct daemon。
                    mark_relay_client_inactive(active_clients, client_id);
                    close_client_connection(protocol, connections, client_id).await;
                    warn!(
                        client_id = client_id.0,
                        %error,
                        "closed relay client after invalid daemon protocol frame"
                    );
                    return Ok(Vec::new());
                }
            };
            trace!(
                client_id = client_id.0,
                frame_kind = protocol_wire_message_kind(&frame),
                frame_bytes = protocol_wire_message_bytes(&frame),
                "relay client frame decoded by daemon mux"
            );
            let responses = {
                let connection = connections
                    .get_mut(&client_id)
                    .expect("connection existence checked before frame parsing");
                let mut protocol = protocol.lock().await;
                connection.handle_wire_message(&mut protocol, frame)
            };
            trace!(
                client_id = client_id.0,
                responses = responses.len(),
                response_bytes = protocol_wire_messages_wire_len(&responses),
                "relay client frame handled by daemon protocol"
            );
            client_wire_messages(client_id, responses)
        }
        RelayMuxEnvelope::DaemonFrame { .. } => Err(RelayConnectorError::InvalidEnvelope),
    }
}

#[cfg(test)]
fn relay_opaque_frame_kind(frame: &RelayOpaqueFrame) -> &'static str {
    match frame {
        RelayOpaqueFrame::Text { .. } => "text",
        RelayOpaqueFrame::Binary { .. } => "binary",
    }
}

#[cfg(test)]
fn relay_opaque_frame_transport_bytes(frame: &RelayOpaqueFrame) -> usize {
    match frame {
        RelayOpaqueFrame::Text { data } => data.len(),
        RelayOpaqueFrame::Binary { data_base64 } => data_base64.len(),
    }
}

fn protocol_wire_messages_wire_len(messages: &[ProtocolWireMessage]) -> usize {
    messages.iter().map(protocol_wire_message_bytes).sum()
}

#[cfg(test)]
fn protocol_wire_message_kind(message: &ProtocolWireMessage) -> &'static str {
    match message {
        ProtocolWireMessage::Json(_) => "json",
        ProtocolWireMessage::Binary(_) => "binary",
    }
}

fn protocol_wire_message_bytes(message: &ProtocolWireMessage) -> usize {
    match message {
        ProtocolWireMessage::Json(envelope) => match serde_json::to_vec(envelope) {
            Ok(raw) => raw.len(),
            Err(_) => 0,
        },
        ProtocolWireMessage::Binary(raw) => raw.len(),
    }
}

#[cfg(test)]
fn relay_envelope_client_id(envelope: &RelayMuxEnvelope) -> Option<RelayClientId> {
    match envelope {
        RelayMuxEnvelope::ClientConnected { client_id }
        | RelayMuxEnvelope::ClientDisconnected { client_id }
        | RelayMuxEnvelope::ClientFrame { client_id, .. } => Some(*client_id),
        RelayMuxEnvelope::Keepalive { .. }
        | RelayMuxEnvelope::KeepaliveAck { .. }
        | RelayMuxEnvelope::DaemonFrame { .. } => None,
    }
}

async fn sync_relay_watchers_for_client(
    client_id: Option<RelayClientId>,
    connections: &HashMap<RelayClientId, ProtocolConnection>,
    protocol: &SharedDaemonProtocol,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_cwd_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    push_event_tx: &mpsc::Sender<RelayPushEvent>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) -> Vec<SessionId> {
    let mut initial_output_sessions = Vec::new();
    let Some(client_id) = client_id else {
        return initial_output_sessions;
    };
    let Some(connection) = connections.get(&client_id) else {
        remove_relay_watchers_for_client(
            client_id,
            watched_output_sessions,
            watched_cwd_sessions,
            watched_resize_sessions,
            watcher_tasks,
        );
        return initial_output_sessions;
    };

    let (output_signals, cwd_signals, resize_signals) = {
        let protocol = protocol.lock().await;
        (
            connection.attached_output_signals(&protocol),
            connection.attached_cwd_signals(&protocol),
            connection.attached_resize_signals(&protocol),
        )
    };
    let desired_output_sessions: HashSet<_> = output_signals
        .iter()
        .map(|(session_id, _)| *session_id)
        .collect();
    let desired_cwd_sessions: HashSet<_> = cwd_signals
        .iter()
        .map(|(session_id, _)| *session_id)
        .collect();
    let desired_resize_sessions: HashSet<_> = resize_signals
        .iter()
        .map(|(session_id, _)| *session_id)
        .collect();

    let current_output = watched_output_sessions
        .get(&client_id)
        .cloned()
        .unwrap_or_default();
    let current_cwd = watched_cwd_sessions
        .get(&client_id)
        .cloned()
        .unwrap_or_default();
    let current_resize = watched_resize_sessions
        .get(&client_id)
        .cloned()
        .unwrap_or_default();
    if !current_output.is_subset(&desired_output_sessions)
        || !current_cwd.is_subset(&desired_cwd_sessions)
        || !current_resize.is_subset(&desired_resize_sessions)
    {
        // 中文注释：一个 relay client 快速切换 session 后，旧 terminal stream 会取消 watched。
        // watcher 是独立 task，逐个精确移除会复杂且容易漏；发现 desired/current 不一致时
        // 重建该 client 的 watcher，保证旧 session 不再持续唤醒输出队列。
        debug!(
            client_id = client_id.0,
            "rebuilding relay watchers after subscription set changed"
        );
        remove_relay_watchers_for_client(
            client_id,
            watched_output_sessions,
            watched_cwd_sessions,
            watched_resize_sessions,
            watcher_tasks,
        );
    }

    for (session_id, mut signal) in output_signals {
        let watched = watched_output_sessions.entry(client_id).or_default();
        if !watched.insert(session_id) {
            continue;
        }
        signal.borrow_and_update();
        initial_output_sessions.push(session_id);

        let push_event_tx = push_event_tx.clone();
        watcher_tasks
            .entry(client_id)
            .or_default()
            .push(tokio::spawn(async move {
                loop {
                    if signal.changed().await.is_err() {
                        break;
                    }
                    tokio::time::sleep(RELAY_OUTPUT_PUSH_COALESCE_DELAY).await;
                    // 中文注释：relay Web 经常跨公网，高频 PTY 输出如果逐 signal 推送会形成
                    // 大量百字节小包。短暂 coalesce 后由 daemon terminal log 批量 drain。
                    signal.borrow_and_update();
                    if push_event_tx
                        .send(RelayPushEvent::Output {
                            client_id,
                            session_id,
                        })
                        .await
                        .is_err()
                    {
                        debug!(
                            client_id = client_id.0,
                            session_id = ?session_id,
                            event = "output",
                            "relay mux output watcher stopped because event queue closed"
                        );
                        break;
                    }
                    trace!(
                        client_id = client_id.0,
                        session_id = ?session_id,
                        event = "output",
                        "relay mux output watcher enqueued event"
                    );
                }
            }));
    }

    for (session_id, mut signal) in cwd_signals {
        let watched = watched_cwd_sessions.entry(client_id).or_default();
        if !watched.insert(session_id) {
            continue;
        }
        // 中文注释：relay 侧 cwd watcher 语义必须与直连 `/ws` 保持一致；
        // 新订阅时先消费当前版本，避免 attach 前已存在的 cwd version 被当成新事件。
        signal.borrow_and_update();

        let push_event_tx = push_event_tx.clone();
        watcher_tasks
            .entry(client_id)
            .or_default()
            .push(tokio::spawn(async move {
                loop {
                    if signal.changed().await.is_err() {
                        break;
                    }
                    if push_event_tx
                        .send(RelayPushEvent::Cwd {
                            client_id,
                            session_id,
                        })
                        .await
                        .is_err()
                    {
                        debug!(
                            client_id = client_id.0,
                            session_id = ?session_id,
                            event = "cwd",
                            "relay mux cwd watcher stopped because event queue closed"
                        );
                        break;
                    }
                    trace!(
                        client_id = client_id.0,
                        session_id = ?session_id,
                        event = "cwd",
                        "relay mux cwd watcher enqueued event"
                    );
                }
            }));
    }

    for (session_id, mut signal) in resize_signals {
        let watched = watched_resize_sessions.entry(client_id).or_default();
        if !watched.insert(session_id) {
            continue;
        }
        signal.borrow_and_update();

        let push_event_tx = push_event_tx.clone();
        watcher_tasks
            .entry(client_id)
            .or_default()
            .push(tokio::spawn(async move {
                loop {
                    if signal.changed().await.is_err() {
                        break;
                    }
                    if push_event_tx
                        .send(RelayPushEvent::Resize {
                            client_id,
                            session_id,
                        })
                        .await
                        .is_err()
                    {
                        debug!(
                            client_id = client_id.0,
                            session_id = ?session_id,
                            event = "resize",
                            "relay mux resize watcher stopped because event queue closed"
                        );
                        break;
                    }
                    trace!(
                        client_id = client_id.0,
                        session_id = ?session_id,
                        event = "resize",
                        "relay mux resize watcher enqueued event"
                    );
                }
            }));
    }

    initial_output_sessions
}

fn queue_relay_initial_output_events(
    client_id: Option<RelayClientId>,
    initial_output_sessions: &[SessionId],
    pending_push_events: &mut RelayPushEventQueue,
) {
    let Some(client_id) = client_id else {
        return;
    };
    for session_id in initial_output_sessions {
        // attach/create 后的大 snapshot 不在请求处理分支内同步发送；先回到 select，
        // 让 relay 发来的 ClientDisconnected、Ping 或用户输入有机会抢在旧输出前面。
        pending_push_events.enqueue(RelayPushEvent::Output {
            client_id,
            session_id: *session_id,
        });
        debug!(
            client_id = client_id.0,
            session_id = ?session_id,
            queue_pending = pending_push_events.len(),
            "relay mux initial output event queued"
        );
    }
}

fn queue_relay_deferred_output_wakeups(
    client_id: RelayClientId,
    connection: &mut ProtocolConnection,
    pending_push_events: &mut RelayPushEventQueue,
) {
    for session_id in connection.take_deferred_output_wakeups() {
        // 中文注释：terminal 输出不再等待 flow ACK/credit；这里仅处理 batch/transport
        // 上限导致的后续 drain，避免单个 relay client 占住 daemon 全局协议锁。
        pending_push_events.enqueue(RelayPushEvent::Output {
            client_id,
            session_id,
        });
        debug!(
            client_id = client_id.0,
            session_id = ?session_id,
            queue_pending = pending_push_events.len(),
            "relay mux deferred output event queued"
        );
    }
}

fn drop_relay_client_runtime(
    client_id: RelayClientId,
    pending_push_events: &mut RelayPushEventQueue,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_cwd_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) {
    // 中文注释：relay client 的 WebSocket 已经断开或协议层已关闭时，daemon 端必须
    // 一次性清理该 client 的所有 runtime：尚未发送的 push、watcher 订阅和后台 task。
    // 这样旧 client 的大量输出不会继续排队，也不会拖慢新的 client stream。
    pending_push_events.remove_client(client_id);
    remove_relay_watchers_for_client(
        client_id,
        watched_output_sessions,
        watched_cwd_sessions,
        watched_resize_sessions,
        watcher_tasks,
    );
}

#[cfg(test)]
async fn enqueue_relay_mux_envelopes(
    relay_endpoint: &str,
    writer_queues: &RelayMuxWriterQueues,
    active_clients: &RelayActiveClients,
    client_id: Option<RelayClientId>,
    kind: RelayOutKind,
    envelopes: Vec<RelayMuxEnvelope>,
) -> Result<usize, RelayConnectorError> {
    if envelopes.is_empty() {
        return Ok(0);
    }
    if client_id.is_some_and(|client_id| !relay_client_is_active(active_clients, client_id)) {
        // 中文注释：client 已断开时，旧输出不能再占用 daemon->relay 主干。
        // 新连接会通过 daemon mirror cache 重新拿 snapshot/tail。
        debug!(
            relay = relay_endpoint,
            client_id = client_id.map(|id| id.0),
            kind = kind.label(),
            envelopes = envelopes.len(),
            "relay mux dropped outbound batch for inactive client"
        );
        return Ok(0);
    }
    let envelope_count = envelopes.len();
    let bytes = relay_mux_envelopes_wire_len(&envelopes);
    let order = match client_id {
        Some(client_id) => Some(
            relay_mux_assign_order(&writer_queues.ordering, client_id)
                .ok_or(RelayConnectorError::SendFailed)?,
        ),
        None => None,
    };
    trace!(
        relay = relay_endpoint,
        client_id = client_id.map(|id| id.0),
        order,
        kind = kind.label(),
        envelopes = envelope_count,
        bytes,
        queue_capacity = writer_queues.sender_for_kind(kind).capacity(),
        "relay mux enqueueing outbound batch"
    );
    let writer_queue = writer_queues.sender_for_kind(kind);
    if !kind.uses_data_lane() {
        // 中文注释：control lane 承载 hello/e2ee/auth 等前置握手。relay 已经给 browser
        // 回了 route_ready，若这里继续 await 满队列，前端只会挂到 handshake timeout。
        // 队列满说明 daemon->relay mux 本地 writer 已不健康，快速失败后由外层重建 mux。
        // 入队成功也不能直接算握手成功：如果 writer 卡在旧输出里，browser 只会在
        // route_ready 后等到 handshake timeout。这里等待本地 writer 完成一次真实写出。
        let (completion_tx, completion_rx) = oneshot::channel();
        let write = RelayMuxWrite::Envelopes {
            kind,
            client_id,
            order,
            envelopes,
            completion: Some(completion_tx),
        };
        writer_queue.try_send(write).map_err(|error| {
            if let (Some(client_id), Some(order)) = (client_id, order) {
                relay_mux_finish_order(&writer_queues.ordering, active_clients, client_id, order);
            }
            match error {
                mpsc::error::TrySendError::Full(_) => {
                    warn!(
                        relay = relay_endpoint,
                        client_id = client_id.map(|id| id.0),
                        order,
                        kind = kind.label(),
                        envelopes = envelope_count,
                        bytes,
                        "relay mux control queue full while enqueueing handshake/response batch"
                    );
                    RelayConnectorError::SendFailed
                }
                mpsc::error::TrySendError::Closed(_) => RelayConnectorError::SendFailed,
            }
        })?;
        trace!(
            relay = relay_endpoint,
            client_id = client_id.map(|id| id.0),
            order,
            kind = kind.label(),
            envelopes = envelope_count,
            bytes,
            queue_capacity = writer_queue.capacity(),
            "relay mux writer queue accepted control batch"
        );
        return match timeout(RELAY_CONTROL_WRITE_COMPLETION_DEADLINE, completion_rx).await {
            Ok(Ok(Ok(()))) => {
                trace!(
                    relay = relay_endpoint,
                    client_id = client_id.map(|id| id.0),
                    order,
                    kind = kind.label(),
                    envelopes = envelope_count,
                    bytes,
                    "relay mux writer completed control batch"
                );
                Ok(bytes)
            }
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_closed)) => Err(RelayConnectorError::SendFailed),
            Err(_elapsed) => {
                warn!(
                    relay = relay_endpoint,
                    client_id = client_id.map(|id| id.0),
                    order,
                    kind = kind.label(),
                    envelopes = envelope_count,
                    bytes,
                    timeout_ms = RELAY_CONTROL_WRITE_COMPLETION_DEADLINE.as_millis(),
                    "relay mux control batch was not written before deadline"
                );
                Err(RelayConnectorError::SendTimeout)
            }
        };
    }

    let write = RelayMuxWrite::Envelopes {
        kind,
        client_id,
        order,
        envelopes,
        completion: None,
    };
    writer_queue
        .send(write)
        .await
        .map(|()| {
            trace!(
                relay = relay_endpoint,
                client_id = client_id.map(|id| id.0),
                order,
                kind = kind.label(),
                envelopes = envelope_count,
                bytes,
                queue_capacity = writer_queue.capacity(),
                "relay mux writer queue accepted batch"
            );
            bytes
        })
        .map_err(|_| {
            if let (Some(client_id), Some(order)) = (client_id, order) {
                relay_mux_finish_order(&writer_queues.ordering, active_clients, client_id, order);
            }
            RelayConnectorError::SendFailed
        })
}

#[cfg(test)]
fn enqueue_relay_mux_envelopes_with_permit(
    relay_endpoint: &str,
    ordering: &RelayMuxOrdering,
    permit: mpsc::Permit<'_, RelayMuxWrite>,
    active_clients: &RelayActiveClients,
    client_id: Option<RelayClientId>,
    kind: RelayOutKind,
    envelopes: Vec<RelayMuxEnvelope>,
) -> usize {
    if envelopes.is_empty() {
        return 0;
    }
    if client_id.is_some_and(|client_id| !relay_client_is_active(active_clients, client_id)) {
        debug!(
            relay = relay_endpoint,
            client_id = client_id.map(|id| id.0),
            kind = kind.label(),
            envelopes = envelopes.len(),
            "relay mux dropped reserved outbound batch for inactive client"
        );
        return 0;
    }
    let envelope_count = envelopes.len();
    let bytes = relay_mux_envelopes_wire_len(&envelopes);
    let order = match client_id {
        Some(client_id) => {
            let Some(order) = relay_mux_assign_order(ordering, client_id) else {
                debug!(
                    relay = relay_endpoint,
                    client_id = client_id.0,
                    kind = kind.label(),
                    envelopes = envelope_count,
                    "relay mux dropped reserved outbound batch because ordering state is unavailable"
                );
                return 0;
            };
            Some(order)
        }
        None => None,
    };
    permit.send(RelayMuxWrite::Envelopes {
        kind,
        client_id,
        order,
        envelopes,
        completion: None,
    });
    trace!(
        relay = relay_endpoint,
        client_id = client_id.map(|id| id.0),
        order,
        kind = kind.label(),
        envelopes = envelope_count,
        bytes,
        "relay mux writer queue accepted reserved push batch"
    );
    bytes
}

#[cfg(test)]
fn try_reserve_relay_mux_push_slot(
    writer_queues: &RelayMuxWriterQueues,
) -> Result<Option<mpsc::Permit<'_, RelayMuxWrite>>, RelayConnectorError> {
    match writer_queues.data.try_reserve() {
        Ok(permit) => Ok(Some(permit)),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(None),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(RelayConnectorError::SendFailed),
    }
}

#[cfg(test)]
async fn enqueue_relay_control_message(
    writer_queues: &RelayMuxWriterQueues,
    kind: RelayOutKind,
    message: Message,
) -> Result<(), RelayConnectorError> {
    let message_kind = relay_message_kind(&message);
    let bytes = relay_message_bytes(&message);
    trace!(
        kind = kind.label(),
        message_kind,
        bytes,
        queue_capacity = writer_queues.control.capacity(),
        "relay mux enqueueing raw control frame"
    );
    // 中文注释：Ping/Pong/close 这类 control frame 不能反过来阻塞 mux 主循环。
    // 队列满时直接让外层重连，比继续排队更符合 dumb-pipe 的传输失败语义。
    writer_queues
        .control
        .try_send(RelayMuxWrite::Raw { kind, message })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_) => {
                RelayConnectorError::SendFailed
            }
        })
}

#[cfg(test)]
async fn drain_relay_push_events(
    relay_endpoint: &str,
    server_id: ServerId,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    pending_push_events: &mut RelayPushEventQueue,
    writer_queues: &RelayMuxWriterQueues,
    active_clients: &RelayActiveClients,
    traffic: &mut RelayTrafficCounters,
    push_drain_wake_pending: &mut bool,
) -> Result<bool, RelayConnectorError> {
    let started_at = Instant::now();
    let mut drained_events = 0_usize;
    let mut sent_bytes = 0_usize;
    let mut wrote_to_relay = false;
    while pending_push_events.has_pending() {
        let Some(event) = pending_push_events.pop_front() else {
            break;
        };
        let (client_id, session_id, kind) = relay_push_event_parts(event);
        trace!(
            relay = %relay_endpoint,
            ?server_id,
            client_id = client_id.0,
            session_id = ?session_id,
            kind = kind.label(),
            queue_pending_after_pop = pending_push_events.len(),
            "relay mux push event dequeued"
        );
        let Some(connection) = connections.get_mut(&client_id) else {
            trace!(
                relay = %relay_endpoint,
                ?server_id,
                client_id = client_id.0,
                session_id = ?session_id,
                kind = kind.label(),
                "relay mux dropped push event for missing client connection"
            );
            continue;
        };
        let push_permit = match try_reserve_relay_mux_push_slot(writer_queues)? {
            Some(permit) => permit,
            None => {
                // 中文注释：terminal/file/resize push 只使用 data lane；control/response
                // lane 仍可继续发送新 client 握手、create response 和 ping/pong。
                // 此时不读取 terminal cache，也不做 E2EE 加密，避免 seq 前进后又无法发送。
                pending_push_events.requeue_front(event);
                queue_relay_push_drain_wakeup(pending_push_events, push_drain_wake_pending);
                trace!(
                    relay = %relay_endpoint,
                    ?server_id,
                    client_id = client_id.0,
                    session_id = ?session_id,
                    kind = kind.label(),
                    queue_capacity = writer_queues.data.capacity(),
                    queue_pending = pending_push_events.len(),
                    "relay mux data writer queue is full"
                );
                break;
            }
        };
        let responses = match kind {
            RelayOutKind::PushOutput => {
                let lock_started = Instant::now();
                let collect_started = Instant::now();
                let (lock_wait, messages) = {
                    let mut protocol = protocol.lock().await;
                    let lock_wait = lock_started.elapsed();
                    // 中文注释：全局 daemon protocol lock 只覆盖 runtime/状态读取。
                    // E2EE 加密和 mux 封包在锁外做，避免 relay 大输出拖慢直连 WebSocket。
                    let messages = connection.drain_session_output_messages_for_push(
                        &mut protocol,
                        session_id,
                        OUTPUT_FLUSH_MAX_BYTES_PER_SESSION,
                    );
                    (lock_wait, messages)
                };
                let collect_elapsed = collect_started.elapsed();
                if lock_wait >= RELAY_SEND_DEBUG_LOG_THRESHOLD
                    || collect_elapsed >= RELAY_SEND_DEBUG_LOG_THRESHOLD
                {
                    debug!(
                        relay = %relay_endpoint,
                        ?server_id,
                        client_id = client_id.0,
                        session_id = ?session_id,
                        lock_wait_ms = lock_wait.as_millis(),
                        collect_ms = collect_elapsed.as_millis(),
                        "relay mux output collection latency"
                    );
                }
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::PushCwd => {
                let messages = {
                    let mut protocol = protocol.lock().await;
                    connection.read_session_cwd_update_messages(&mut protocol, session_id)
                };
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::PushResize => {
                let messages = {
                    let mut protocol = protocol.lock().await;
                    connection.read_session_resize_update_messages(&mut protocol, session_id)
                };
                connection.encrypt_collected_inner_messages_wire(messages)
            }
            RelayOutKind::Response
            | RelayOutKind::FileTunnelBody
            | RelayOutKind::MuxKeepalive
            | RelayOutKind::IdlePing
            | RelayOutKind::Pong => Vec::new(),
        };
        queue_relay_deferred_output_wakeups(client_id, connection, pending_push_events);
        let response_count = responses.len();
        if response_count == 0 {
            drained_events = drained_events.saturating_add(1);
            trace!(
                relay = %relay_endpoint,
                ?server_id,
                client_id = client_id.0,
                session_id = ?session_id,
                kind = kind.label(),
                "relay mux push event produced no output"
            );
            if relay_push_drain_budget_exhausted(drained_events, sent_bytes, started_at) {
                log_relay_push_drain_reschedule(
                    relay_endpoint,
                    server_id,
                    kind,
                    drained_events,
                    sent_bytes,
                    pending_push_events.len(),
                    started_at.elapsed(),
                );
                queue_relay_push_drain_wakeup(pending_push_events, push_drain_wake_pending);
                break;
            }
            continue;
        }
        let envelopes = client_wire_messages(client_id, responses)?;
        let batch_bytes = relay_mux_envelopes_wire_len(&envelopes);
        if batch_bytes >= RELAY_SEND_DEBUG_BATCH_BYTES {
            debug!(
                relay = %relay_endpoint,
                ?server_id,
                client_id = client_id.0,
                kind = kind.label(),
                queued_envelopes = envelopes.len(),
                queued_bytes = batch_bytes,
                "relay mux queued large deferred output"
            );
        }
        let sent = enqueue_relay_mux_envelopes_with_permit(
            relay_endpoint,
            &writer_queues.ordering,
            push_permit,
            active_clients,
            Some(client_id),
            kind,
            envelopes,
        );
        trace!(
            relay = %relay_endpoint,
            ?server_id,
            client_id = client_id.0,
            session_id = ?session_id,
            kind = kind.label(),
            response_count,
            batch_bytes,
            sent_bytes = sent,
            queue_pending = pending_push_events.len(),
            "relay mux push batch queued to writer"
        );
        wrote_to_relay |= sent > 0;
        traffic.record_out(kind, if sent > 0 { response_count } else { 0 }, sent);
        drained_events = drained_events.saturating_add(1);
        sent_bytes = sent_bytes.saturating_add(sent);
        if relay_push_drain_budget_exhausted(drained_events, sent_bytes, started_at) {
            // 中文注释：这里主动让 relay mux 主循环回到 select!，让输入、attach、disconnect
            // 和 Ping/Pong 有机会插队处理；下一轮继续把输出推进到 writer queue。
            log_relay_push_drain_reschedule(
                relay_endpoint,
                server_id,
                kind,
                drained_events,
                sent_bytes,
                pending_push_events.len(),
                started_at.elapsed(),
            );
            queue_relay_push_drain_wakeup(pending_push_events, push_drain_wake_pending);
            break;
        }
    }
    Ok(wrote_to_relay)
}

fn log_relay_push_drain_reschedule(
    relay_endpoint: &str,
    server_id: ServerId,
    kind: RelayOutKind,
    drained_events: usize,
    sent_bytes: usize,
    pending_events: usize,
    elapsed: Duration,
) {
    if pending_events == 0 {
        return;
    }
    if pending_events >= 16
        || sent_bytes >= RELAY_SEND_DEBUG_BATCH_BYTES
        || elapsed >= RELAY_SEND_DEBUG_LOG_THRESHOLD
    {
        debug!(
            relay = relay_endpoint,
            ?server_id,
            kind = kind.label(),
            drained_events,
            sent_bytes,
            pending_events,
            elapsed_ms = elapsed.as_millis(),
            "relay mux deferred output rescheduled"
        );
    }
}

#[cfg(test)]
fn queue_relay_push_drain_wakeup(
    pending_push_events: &RelayPushEventQueue,
    push_drain_wake_pending: &mut bool,
) {
    if *push_drain_wake_pending || !pending_push_events.has_pending() {
        return;
    }
    *push_drain_wake_pending = true;
    debug!(
        queue_pending = pending_push_events.len(),
        "relay mux push drain wakeup scheduled"
    );
}

fn relay_push_drain_budget_exhausted(
    drained_events: usize,
    transported_bytes: usize,
    started_at: Instant,
) -> bool {
    let elapsed_budget_exhausted =
        drained_events > 0 && started_at.elapsed() >= RELAY_PUSH_DRAIN_MAX_ELAPSED_PER_TICK;
    drained_events >= RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK
        || transported_bytes >= RELAY_PUSH_DRAIN_MAX_TRANSPORTED_BYTES_PER_TICK
        || elapsed_budget_exhausted
}

#[cfg(test)]
fn relay_mux_envelopes_wire_len(envelopes: &[RelayMuxEnvelope]) -> usize {
    envelopes
        .iter()
        .map(|envelope| match serde_json::to_vec(envelope) {
            Ok(raw) => raw.len(),
            Err(_) => 0,
        })
        .sum()
}

fn relay_push_event_parts(event: RelayPushEvent) -> (RelayClientId, SessionId, RelayOutKind) {
    match event {
        RelayPushEvent::Output {
            client_id,
            session_id,
        } => (client_id, session_id, RelayOutKind::PushOutput),
        RelayPushEvent::Cwd {
            client_id,
            session_id,
        } => (client_id, session_id, RelayOutKind::PushCwd),
        RelayPushEvent::Resize {
            client_id,
            session_id,
        } => (client_id, session_id, RelayOutKind::PushResize),
    }
}

fn relay_push_event_client_id(event: RelayPushEvent) -> RelayClientId {
    match event {
        RelayPushEvent::Output { client_id, .. }
        | RelayPushEvent::Cwd { client_id, .. }
        | RelayPushEvent::Resize { client_id, .. } => client_id,
    }
}

fn remove_relay_watchers_for_client(
    client_id: RelayClientId,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_cwd_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) {
    watched_output_sessions.remove(&client_id);
    watched_cwd_sessions.remove(&client_id);
    watched_resize_sessions.remove(&client_id);
    if let Some(tasks) = watcher_tasks.remove(&client_id) {
        for task in tasks {
            task.abort();
        }
    }
}

fn abort_relay_watcher_tasks(watcher_tasks: HashMap<RelayClientId, Vec<JoinHandle<()>>>) {
    for tasks in watcher_tasks.into_values() {
        for task in tasks {
            task.abort();
        }
    }
}

fn relay_send_deadline(kind: RelayOutKind) -> Option<Duration> {
    match kind {
        RelayOutKind::FileTunnelBody => None,
        RelayOutKind::Pong => Some(RELAY_PONG_DEADLINE),
        _ => Some(RELAY_SEND_DEADLINE),
    }
}

#[cfg(test)]
fn client_envelopes(
    client_id: RelayClientId,
    envelopes: Vec<JsonEnvelope>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    envelopes
        .into_iter()
        .map(|envelope| {
            let raw = serde_json::to_string(&envelope)
                .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
            Ok(RelayMuxEnvelope::DaemonFrame {
                client_id,
                frame: RelayOpaqueFrame::Text { data: raw },
            })
        })
        .collect()
}

#[cfg(test)]
fn client_wire_messages(
    client_id: RelayClientId,
    messages: Vec<ProtocolWireMessage>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    messages
        .into_iter()
        .map(|message| match message {
            ProtocolWireMessage::Json(envelope) => {
                let raw = serde_json::to_string(&envelope)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                Ok(RelayMuxEnvelope::DaemonFrame {
                    client_id,
                    frame: RelayOpaqueFrame::Text { data: raw },
                })
            }
            ProtocolWireMessage::Binary(raw) => Ok(RelayMuxEnvelope::DaemonFrame {
                client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: general_purpose::STANDARD.encode(raw),
                },
            }),
        })
        .collect()
}

#[cfg(test)]
async fn run_relay_mux_writer(
    relay_endpoint: String,
    mut control_rx: mpsc::Receiver<RelayMuxWrite>,
    mut data_rx: mpsc::Receiver<RelayMuxWrite>,
    writer_failed_tx: mpsc::Sender<()>,
    mut sender: RelaySender,
    active_clients: RelayActiveClients,
    ordering: RelayMuxOrdering,
) {
    // 中文注释：writer task 独占 WebSocket Sink；control lane 承载 hello/auth/create
    // response 和 ping/pong，data lane 承载 terminal push。两者仍写同一条 WebSocket，
    // 但低带宽下不能让大量 terminal output 排在新 client 握手前面。
    let mut control_open = true;
    let mut data_open = true;
    let mut prefer_data_once = false;
    let mut deferred_writes = VecDeque::<RelayMuxWrite>::new();
    while control_open || data_open || !deferred_writes.is_empty() {
        if let Some(write) = relay_mux_pop_ready_deferred_write(&mut deferred_writes, &ordering) {
            prefer_data_once = true;
            if !handle_relay_mux_writer_write(
                &relay_endpoint,
                &mut sender,
                &active_clients,
                &ordering,
                write,
                "deferred",
            )
            .await
            {
                let _ = writer_failed_tx.try_send(());
                break;
            }
            continue;
        }

        if prefer_data_once {
            tokio::select! {
                biased;

                maybe_write = data_rx.recv(), if data_open => {
                    prefer_data_once = false;
                    let Some(write) = maybe_write else {
                        data_open = false;
                        continue;
                    };
                    if !relay_mux_write_is_sendable(&write, &ordering) {
                        let snapshot = write.debug_snapshot();
                        prefer_data_once = false;
                        debug!(
                            relay = %relay_endpoint,
                            client_id = snapshot.client_id.map(|id| id.0),
                            order = snapshot.order,
                            kind = snapshot.kind.label(),
                            deferred_writes = deferred_writes.len() + 1,
                            "relay mux writer deferred data behind same-client earlier frame"
                        );
                        deferred_writes.push_back(write);
                        continue;
                    }
                    if !handle_relay_mux_writer_write(
                        &relay_endpoint,
                        &mut sender,
                        &active_clients,
                        &ordering,
                        write,
                        "data",
                    ).await {
                        let _ = writer_failed_tx.try_send(());
                        break;
                    }
                }
                maybe_write = control_rx.recv(), if control_open => {
                    let Some(write) = maybe_write else {
                        control_open = false;
                        continue;
                    };
                    prefer_data_once = true;
                    if !relay_mux_write_is_sendable(&write, &ordering) {
                        let snapshot = write.debug_snapshot();
                        debug!(
                            relay = %relay_endpoint,
                            client_id = snapshot.client_id.map(|id| id.0),
                            order = snapshot.order,
                            kind = snapshot.kind.label(),
                            deferred_writes = deferred_writes.len() + 1,
                            "relay mux writer deferred control behind same-client earlier frame"
                        );
                        deferred_writes.push_back(write);
                        continue;
                    }
                    if !handle_relay_mux_writer_write(
                        &relay_endpoint,
                        &mut sender,
                        &active_clients,
                        &ordering,
                        write,
                        "control",
                    ).await {
                        let _ = writer_failed_tx.try_send(());
                        break;
                    }
                }
            }
            continue;
        }

        tokio::select! {
            biased;

            maybe_write = control_rx.recv(), if control_open => {
                let Some(write) = maybe_write else {
                    control_open = false;
                    continue;
                };
                prefer_data_once = true;
                if !relay_mux_write_is_sendable(&write, &ordering) {
                    let snapshot = write.debug_snapshot();
                    debug!(
                        relay = %relay_endpoint,
                        client_id = snapshot.client_id.map(|id| id.0),
                        order = snapshot.order,
                        kind = snapshot.kind.label(),
                        deferred_writes = deferred_writes.len() + 1,
                        "relay mux writer deferred control behind same-client earlier frame"
                    );
                    deferred_writes.push_back(write);
                    continue;
                }
                if !handle_relay_mux_writer_write(
                    &relay_endpoint,
                    &mut sender,
                    &active_clients,
                    &ordering,
                    write,
                    "control",
                ).await {
                    let _ = writer_failed_tx.try_send(());
                    break;
                }
            }
            maybe_write = data_rx.recv(), if data_open => {
                let Some(write) = maybe_write else {
                    data_open = false;
                    continue;
                };
                if !relay_mux_write_is_sendable(&write, &ordering) {
                    let snapshot = write.debug_snapshot();
                    prefer_data_once = false;
                    debug!(
                        relay = %relay_endpoint,
                        client_id = snapshot.client_id.map(|id| id.0),
                        order = snapshot.order,
                        kind = snapshot.kind.label(),
                        deferred_writes = deferred_writes.len() + 1,
                        "relay mux writer deferred data behind same-client earlier frame"
                    );
                    deferred_writes.push_back(write);
                    continue;
                }
                if !handle_relay_mux_writer_write(
                    &relay_endpoint,
                    &mut sender,
                    &active_clients,
                    &ordering,
                    write,
                    "data",
                ).await {
                    let _ = writer_failed_tx.try_send(());
                    break;
                }
            }
        }
    }
    debug!(relay = %relay_endpoint, "relay mux writer stopped");
}

#[cfg(test)]
async fn handle_relay_mux_writer_write(
    relay_endpoint: &str,
    sender: &mut RelaySender,
    active_clients: &RelayActiveClients,
    ordering: &RelayMuxOrdering,
    mut write: RelayMuxWrite,
    lane: &'static str,
) -> bool {
    let snapshot = write.debug_snapshot();
    let completion = relay_mux_write_completion(&mut write);
    trace!(
        relay = %relay_endpoint,
        lane,
        client_id = snapshot.client_id.map(|id| id.0),
        order = snapshot.order,
        kind = snapshot.kind.label(),
        envelopes = snapshot.envelopes,
        bytes = snapshot.bytes,
        raw = snapshot.raw,
        "relay mux writer dequeued frame"
    );
    if let Some(client_id) = snapshot.client_id {
        if !relay_client_is_active(active_clients, client_id) {
            // 中文注释：client 断开可能发生在输出批次入队之后、真正写 socket 之前。
            // 这里再查一次生命周期，避免旧 client 的大输出继续占用 daemon->relay 主干。
            debug!(
                relay = %relay_endpoint,
                lane,
                client_id = client_id.0,
                order = snapshot.order,
                kind = snapshot.kind.label(),
                envelopes = snapshot.envelopes,
                bytes = snapshot.bytes,
                "relay mux writer dropped queued frame for inactive client"
            );
            if let Some(order) = snapshot.order {
                relay_mux_finish_order(ordering, active_clients, client_id, order);
            }
            relay_mux_complete_write(completion, Ok(()));
            return true;
        }
    }
    let sent = send_relay_mux_write(relay_endpoint, sender, active_clients, write).await;
    if sent && let (Some(client_id), Some(order)) = (snapshot.client_id, snapshot.order) {
        relay_mux_finish_order(ordering, active_clients, client_id, order);
    }
    relay_mux_complete_write(
        completion,
        if sent {
            Ok(())
        } else {
            Err(RelayConnectorError::SendFailed)
        },
    );
    sent
}

#[cfg(test)]
fn relay_mux_write_completion(
    write: &mut RelayMuxWrite,
) -> Option<oneshot::Sender<Result<(), RelayConnectorError>>> {
    match write {
        RelayMuxWrite::Envelopes { completion, .. } => completion.take(),
        RelayMuxWrite::Raw { .. } => None,
    }
}

#[cfg(test)]
fn relay_mux_complete_write(
    completion: Option<oneshot::Sender<Result<(), RelayConnectorError>>>,
    result: Result<(), RelayConnectorError>,
) {
    if let Some(completion) = completion {
        // 中文注释：这是 daemon 进程内的 writer 完成信号，不是 relay/browser ACK。
        // receiver 可能已因 mux 重连超时被丢弃；这种情况下不用再传播。
        let _ = completion.send(result);
    }
}

#[cfg(test)]
async fn send_relay_mux_write(
    relay_endpoint: &str,
    sender: &mut RelaySender,
    active_clients: &RelayActiveClients,
    write: RelayMuxWrite,
) -> bool {
    match write {
        RelayMuxWrite::Envelopes {
            kind,
            client_id,
            order: _,
            envelopes,
            completion: _,
        } => send_mux_envelopes_logged(
            relay_endpoint,
            sender,
            active_clients,
            client_id,
            envelopes,
            kind.label(),
        )
        .await
        .is_ok(),
        RelayMuxWrite::Raw { kind, message } => {
            send_relay_message(sender, message, relay_send_deadline(kind))
                .await
                .is_ok()
        }
    }
}

#[cfg(test)]
async fn send_mux_envelopes_logged(
    relay_endpoint: &str,
    sender: &mut RelaySender,
    active_clients: &RelayActiveClients,
    client_id: Option<RelayClientId>,
    envelopes: Vec<RelayMuxEnvelope>,
    label: &'static str,
) -> Result<usize, RelayConnectorError> {
    let envelope_count = envelopes.len();
    let mut bytes = 0_usize;
    let started_at = Instant::now();
    for (index, envelope) in envelopes.into_iter().enumerate() {
        if client_id.is_some_and(|client_id| !relay_client_is_active(active_clients, client_id)) {
            // 中文注释：一个 batch 内也可能跨过 client close 时刻；剩余 envelope
            // 继续发送只会浪费 relay 主干，下一次连接会重新拿 snapshot。
            debug!(
                relay = relay_endpoint,
                client_id = client_id.map(|id| id.0),
                label,
                envelope_index = index,
                envelope_count,
                sent_bytes = bytes,
                "relay mux stopped sending batch for inactive client"
            );
            break;
        }
        let raw = encode_binary_relay_mux_envelope(&envelope)
            .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
        let frame_bytes = raw.len();
        let frame_started_at = Instant::now();
        bytes = bytes.saturating_add(raw.len());
        send_relay_message_with_deadline(sender, Message::Binary(raw.into()), RELAY_SEND_DEADLINE)
            .await?;
        let frame_elapsed = frame_started_at.elapsed();
        if frame_elapsed >= RELAY_SEND_SLOW_LOG_THRESHOLD
            || frame_bytes >= RELAY_SEND_INFO_BATCH_BYTES
        {
            info!(
                relay = relay_endpoint,
                label,
                envelope_index = index,
                envelope_count,
                frame_bytes,
                elapsed_ms = frame_elapsed.as_millis(),
                "relay mux websocket frame send pressure"
            );
        }
    }
    log_relay_send(
        relay_endpoint,
        label,
        envelope_count,
        bytes,
        started_at.elapsed(),
        "relay mux send batch",
    );
    Ok(bytes)
}

async fn send_relay_message_with_deadline<S>(
    sender: &mut S,
    message: Message,
    deadline: Duration,
) -> Result<(), RelayConnectorError>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match timeout(deadline, sender.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(RelayConnectorError::SendFailed),
        Err(_) => Err(RelayConnectorError::SendTimeout),
    }
}

async fn send_relay_message<S>(
    sender: &mut S,
    message: Message,
    deadline: Option<Duration>,
) -> Result<(), RelayConnectorError>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    // 中文注释：文件 tunnel body 是真实字节流，不能被 10s 应用层 deadline 截断；
    // 这里让 WebSocket/TCP 背压自然传播，只有真实 send error 才终止连接。
    match deadline {
        Some(deadline) => send_relay_message_with_deadline(sender, message, deadline).await,
        None => sender
            .send(message)
            .await
            .map_err(|_| RelayConnectorError::SendFailed),
    }
}

fn log_relay_send(
    relay_endpoint: &str,
    label: &str,
    envelopes: usize,
    bytes: usize,
    elapsed: Duration,
    event: &'static str,
) {
    let elapsed_ms = elapsed.as_millis();
    let promote_to_info = elapsed >= RELAY_SEND_SLOW_LOG_THRESHOLD
        || envelopes >= RELAY_SEND_INFO_BATCH_ENVELOPES
        || bytes >= RELAY_SEND_INFO_BATCH_BYTES;
    let emit_debug = elapsed >= RELAY_SEND_DEBUG_LOG_THRESHOLD
        || envelopes >= RELAY_SEND_DEBUG_BATCH_ENVELOPES
        || bytes >= RELAY_SEND_DEBUG_BATCH_BYTES;

    if promote_to_info {
        info!(
            relay = relay_endpoint,
            label, envelopes, bytes, elapsed_ms, "{event}"
        );
    } else if emit_debug {
        debug!(
            relay = relay_endpoint,
            label, envelopes, bytes, elapsed_ms, "{event}"
        );
    }
}

#[cfg(test)]
fn json_envelope_from_mux_frame(
    frame: RelayOpaqueFrame,
) -> Result<JsonEnvelope, RelayConnectorError> {
    match frame {
        RelayOpaqueFrame::Text { data } => {
            serde_json::from_str(&data).map_err(|_| RelayConnectorError::InvalidFrame)
        }
        RelayOpaqueFrame::Binary { data_base64 } => {
            let bytes = general_purpose::STANDARD
                .decode(data_base64)
                .map_err(|_| RelayConnectorError::InvalidFrame)?;
            serde_json::from_slice(&bytes).map_err(|_| RelayConnectorError::InvalidFrame)
        }
    }
}

#[cfg(test)]
fn wire_message_from_mux_frame(
    frame: RelayOpaqueFrame,
) -> Result<ProtocolWireMessage, RelayConnectorError> {
    match frame {
        RelayOpaqueFrame::Text { data } => serde_json::from_str(&data)
            .map(ProtocolWireMessage::Json)
            .map_err(|_| RelayConnectorError::InvalidFrame),
        RelayOpaqueFrame::Binary { data_base64 } => general_purpose::STANDARD
            .decode(data_base64)
            .map(ProtocolWireMessage::Binary)
            .map_err(|_| RelayConnectorError::InvalidFrame),
    }
}

async fn connect_relay_websocket(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    connect_timeout: Duration,
) -> Result<
    (
        RelayWs,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    RelayConnectorError,
> {
    let tls_connector = relay_tls_connector();
    let Some(proxy) = proxy else {
        return timeout(
            connect_timeout,
            tokio_tungstenite::connect_async_tls_with_config(
                url,
                Some(relay_websocket_config()),
                false,
                Some(tls_connector),
            ),
        )
        .await
        .map_err(|_| RelayConnectorError::ConnectTimeout)?
        .map_err(|_| RelayConnectorError::ConnectFailed);
    };

    let target =
        relay_target_from_ws_url(url).ok_or_else(|| RelayConnectorError::UnsupportedUrl)?;
    let stream = timeout(connect_timeout, connect_proxy_tunnel(proxy, &target))
        .await
        .map_err(|_| RelayConnectorError::ConnectTimeout)?
        .map_err(|_| RelayConnectorError::ConnectFailed)?;

    timeout(
        connect_timeout,
        tokio_tungstenite::client_async_tls_with_config(
            url,
            stream,
            Some(relay_websocket_config()),
            Some(tls_connector),
        ),
    )
    .await
    .map_err(|_| RelayConnectorError::ConnectTimeout)?
    .map_err(|_| RelayConnectorError::ConnectFailed)
}

fn relay_tls_connector() -> Connector {
    Connector::Rustls(Arc::new(relay_tls_client_config()))
}

fn relay_tls_client_config() -> ClientConfig {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = relay_tls_kx_groups();

    // 一些代理或 TLS 入口会吞掉 rustls 默认的 X25519MLKEM768 hybrid ClientHello。
    // relay outbound 是兼容性优先的公网长连接，这里显式使用传统 ECDHE 组。
    ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("relay TLS protocol versions should be valid")
        .with_root_certificates(root_store)
        .with_no_client_auth()
}

fn relay_tls_kx_groups() -> Vec<&'static dyn rustls::crypto::SupportedKxGroup> {
    vec![
        rustls::crypto::aws_lc_rs::kx_group::X25519,
        rustls::crypto::aws_lc_rs::kx_group::SECP256R1,
        rustls::crypto::aws_lc_rs::kx_group::SECP384R1,
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayConnectTarget {
    host: String,
    port: u16,
    authority: String,
}

fn relay_target_from_ws_url(url: &str) -> Option<RelayConnectTarget> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("ws://") {
        ("ws", rest)
    } else if let Some(rest) = url.strip_prefix("wss://") {
        ("wss", rest)
    } else {
        return None;
    };
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    let authority = authority
        .split_once('?')
        .map_or(authority, |(authority, _)| authority);
    let (host, port) = parse_target_authority(authority, scheme)?;
    let authority = if authority_has_explicit_port(authority) {
        authority.to_owned()
    } else {
        format_authority(&host, port)
    };
    Some(RelayConnectTarget {
        host,
        port,
        authority,
    })
}

async fn connect_proxy_tunnel(
    proxy: &RelayProxyUrl,
    target: &RelayConnectTarget,
) -> std::io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy.authority()).await?;
    match proxy.scheme() {
        RelayProxyScheme::Http => {
            write_http_connect(&mut stream, target).await?;
        }
        RelayProxyScheme::Socks5 => {
            write_socks5_connect(&mut stream, target).await?;
        }
    }
    Ok(stream)
}

async fn write_http_connect(
    stream: &mut TcpStream,
    target: &RelayConnectTarget,
) -> std::io::Result<()> {
    // 代理只看目标 host:port，relay auth token 仍留在后续 WebSocket 请求内。
    let request = http_connect_request(&target.authority, "");
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut response = Vec::new();
    let mut buf = [0_u8; 256];
    loop {
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "http proxy closed before CONNECT response",
            ));
        }
        response.extend_from_slice(&buf[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if response.len() > 8192 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "http proxy CONNECT response is too large",
            ));
        }
    }

    let response = std::str::from_utf8(&response).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "http proxy CONNECT response is not utf-8",
        )
    })?;
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "http proxy CONNECT response has no status",
            )
        })?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("http proxy CONNECT returned {status}"),
        ))
    }
}

async fn write_socks5_connect(
    stream: &mut TcpStream,
    target: &RelayConnectTarget,
) -> std::io::Result<()> {
    let request = socks5_connect_request(&target.host, target.port)?;
    stream.write_all(&request[..3]).await?;
    stream.flush().await?;
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting != [0x05, 0x00] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socks5 proxy rejected no-auth method",
        ));
    }

    stream.write_all(&request[3..]).await?;
    stream.flush().await?;
    let mut head = [0_u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 || head[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "socks5 proxy failed CONNECT",
        ));
    }

    let remaining = match head[3] {
        0x01 => 4 + 2,
        0x03 => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize + 2
        }
        0x04 => 16 + 2,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "socks5 proxy returned unknown address type",
            ));
        }
    };
    let mut discard = vec![0_u8; remaining];
    stream.read_exact(&mut discard).await?;
    Ok(())
}

fn http_connect_request(target_authority: &str, _proxy_authority: &str) -> String {
    format!(
        "CONNECT {target_authority} HTTP/1.1\r\nHost: {target_authority}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    )
}

fn socks5_connect_request(host: &str, port: u16) -> std::io::Result<Vec<u8>> {
    if host.is_empty() || host.len() > u8::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socks5 target host length is invalid",
        ));
    }
    let mut request = vec![
        0x05,
        0x01,
        0x00, // greeting: version 5, one method, no-auth
        0x05,
        0x01,
        0x00,
        0x03,
        host.len() as u8,
    ];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    Ok(request)
}

fn validate_authority(authority: &str) -> Result<(), RelayConnectorError> {
    if authority.is_empty() || authority.contains('@') {
        return Err(RelayConnectorError::UnsupportedUrl);
    }
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((host, suffix)) = after_bracket.split_once(']') else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };
        if host.is_empty() {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        return match suffix.strip_prefix(':') {
            Some(port) if port.parse::<u16>().is_ok() => Ok(()),
            None if suffix.is_empty() => Ok(()),
            _ => Err(RelayConnectorError::UnsupportedUrl),
        };
    }

    if let Some((host, port)) = authority.rsplit_once(':') {
        // 未加方括号的 IPv6 不属于合法 authority；这里避免把最后一段误判成端口。
        if host.is_empty() || host.contains(':') || port.parse::<u16>().is_err() {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        return Ok(());
    };

    Ok(())
}

fn validate_proxy_authority(authority: &str) -> Result<(), RelayConnectorError> {
    parse_target_authority(authority, "proxy")
        .map(|_| ())
        .ok_or(RelayConnectorError::UnsupportedUrl)
}

fn parse_target_authority(authority: &str, scheme: &str) -> Option<(String, u16)> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, suffix) = after_bracket.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = suffix
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok())
            .or_else(|| default_port_for_scheme(scheme))?;
        return Some((host.to_owned(), port));
    }

    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return None;
        }
        return Some((host.to_owned(), port.parse::<u16>().ok()?));
    }

    let port = default_port_for_scheme(scheme)?;
    if authority.is_empty() || authority.contains(':') {
        return None;
    }
    Some((authority.to_owned(), port))
}

fn default_port_for_scheme(scheme: &str) -> Option<u16> {
    match scheme {
        "ws" => Some(80),
        "wss" => Some(443),
        _ => None,
    }
}

fn authority_has_explicit_port(authority: &str) -> bool {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        return after_bracket
            .split_once(']')
            .is_some_and(|(_, suffix)| suffix.starts_with(':'));
    }
    authority.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok()
    })
}

fn format_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn authority_contains_credentials(authority: &str) -> bool {
    authority.contains('@')
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
async fn close_relay_connections(
    protocol: SharedDaemonProtocol,
    connections: HashMap<RelayClientId, ProtocolConnection>,
) {
    if connections.is_empty() {
        return;
    }

    let mut protocol = protocol.lock().await;
    for (_client_id, mut connection) in connections {
        connection.close(&mut protocol);
    }
}

#[cfg(test)]
async fn close_client_connection(
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    client_id: RelayClientId,
) {
    if let Some(mut connection) = connections.remove(&client_id) {
        let mut protocol = protocol.lock().await;
        connection.close(&mut protocol);
    }
}

impl From<ProtocolError> for RelayConnectorError {
    fn from(_: ProtocolError) -> Self {
        Self::InvalidEnvelope
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::current_unix_timestamp_millis;
    use crate::config::{DaemonConfig, RelayReconnectConfig};
    use crate::net::protocol::{decode_payload, encrypted_frame_from_envelope, envelope_value};
    use crate::net::server::default_protocol;
    use crate::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
    use axum::extract::State;
    use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use futures_util::StreamExt;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use termd_proto::{
        Envelope, MessageType, PingPayload, ProtocolVersion, RouteHelloPayload, RouteReadyPayload,
        RouteRole,
    };
    use tokio::sync::{Notify, mpsc, oneshot};

    static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_protocol(name: &str) -> SharedDaemonProtocol {
        default_protocol(DaemonConfig::default_for_state_path(temp_state_path(name)))
    }

    fn test_active_clients() -> RelayActiveClients {
        Arc::new(Mutex::new(HashSet::new()))
    }

    struct PendingSink {
        ready_polls: Arc<AtomicUsize>,
        ready_notify: Arc<Notify>,
    }

    impl PendingSink {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<Notify>) {
            let ready_polls = Arc::new(AtomicUsize::new(0));
            let ready_notify = Arc::new(Notify::new());
            (
                Self {
                    ready_polls: ready_polls.clone(),
                    ready_notify: ready_notify.clone(),
                },
                ready_polls,
                ready_notify,
            )
        }
    }

    impl Sink<Message> for PendingSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            let this = self.get_mut();
            this.ready_polls.fetch_add(1, Ordering::Relaxed);
            this.ready_notify.notify_waiters();
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            unreachable!("poll_ready never completes in this test sink")
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn relay_push_queue_coalesces_duplicate_pending_events() {
        let client_id = RelayClientId(1);
        let session_id = SessionId::new();
        let event = RelayPushEvent::Output {
            client_id,
            session_id,
        };
        let mut queue = RelayPushEventQueue::default();

        queue.enqueue(event);
        queue.enqueue(event);

        assert_eq!(queue.pop_front(), Some(event));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn relay_push_queue_pop_boundary_allows_new_pending_event() {
        let client_id = RelayClientId(1);
        let session_id = SessionId::new();
        let event = RelayPushEvent::Output {
            client_id,
            session_id,
        };
        let mut queue = RelayPushEventQueue::default();

        queue.enqueue(event);
        assert_eq!(queue.pop_front(), Some(event));
        queue.enqueue(event);
        queue.enqueue(event);

        // 中文注释：事件被 mux 主循环取出后就离开 pending_set。
        // 后续 watcher 看到同一 session 有新输出时，可以重新排入 pending。
        assert_eq!(queue.pop_front(), Some(event));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn relay_push_wakeup_schedules_pending_without_draining() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();
        let mut wake_pending = false;

        queue.enqueue(event);
        queue_relay_push_drain_wakeup(&queue, &mut wake_pending);
        queue_relay_push_drain_wakeup(&queue, &mut wake_pending);

        // 中文注释：唤醒只是把 pending 事件交回 mux 主循环调度，不能同步 drain。
        // 真正的输出责任边界是拿写锁后直接写入 relay WebSocket。
        assert!(queue.has_pending());
        assert!(wake_pending);
    }

    #[test]
    fn relay_push_queue_has_pending_does_not_dequeue() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();

        queue.enqueue(event);

        assert!(queue.has_pending());
        assert_eq!(queue.pop_front(), Some(event));
    }

    #[tokio::test]
    async fn relay_client_runtime_drop_clears_pending_watchers_and_aborts_tasks() {
        let client_id = RelayClientId(1);
        let other_client_id = RelayClientId(2);
        let client_session_id = SessionId::new();
        let other_session_id = SessionId::new();
        let client_event = RelayPushEvent::Output {
            client_id,
            session_id: client_session_id,
        };
        let other_event = RelayPushEvent::Output {
            client_id: other_client_id,
            session_id: other_session_id,
        };
        let mut pending_push_events = RelayPushEventQueue::default();
        pending_push_events.enqueue(client_event);
        pending_push_events.enqueue(RelayPushEvent::Resize {
            client_id,
            session_id: client_session_id,
        });
        pending_push_events.enqueue(other_event);

        let mut watched_output_sessions = HashMap::new();
        watched_output_sessions.insert(client_id, HashSet::from([client_session_id]));
        watched_output_sessions.insert(other_client_id, HashSet::from([other_session_id]));
        let mut watched_cwd_sessions = HashMap::new();
        watched_cwd_sessions.insert(client_id, HashSet::from([client_session_id]));
        let mut watched_resize_sessions = HashMap::new();
        watched_resize_sessions.insert(client_id, HashSet::from([client_session_id]));
        let (drop_tx, drop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _drop_notify = DropNotify(Some(drop_tx));
            std::future::pending::<()>().await;
        });
        let mut watcher_tasks = HashMap::new();
        watcher_tasks.insert(client_id, vec![task]);
        tokio::task::yield_now().await;

        drop_relay_client_runtime(
            client_id,
            &mut pending_push_events,
            &mut watched_output_sessions,
            &mut watched_cwd_sessions,
            &mut watched_resize_sessions,
            &mut watcher_tasks,
        );

        assert_eq!(pending_push_events.pop_front(), Some(other_event));
        assert_eq!(pending_push_events.pop_front(), None);
        assert!(!watched_output_sessions.contains_key(&client_id));
        assert_eq!(
            watched_output_sessions.get(&other_client_id),
            Some(&HashSet::from([other_session_id]))
        );
        assert!(!watched_cwd_sessions.contains_key(&client_id));
        assert!(!watched_resize_sessions.contains_key(&client_id));
        assert!(!watcher_tasks.contains_key(&client_id));
        timeout(Duration::from_millis(50), drop_rx)
            .await
            .expect("client watcher task should be aborted")
            .expect("drop notification should be delivered");
    }

    struct DropNotify(Option<oneshot::Sender<()>>);

    impl Drop for DropNotify {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn relay_push_drain_budget_limits_hot_loop() {
        // 中文注释：relay 主干按高速字节流处理，预算必须是 MB 级；
        // 否则 16KB/64KB 小批量会把千兆链路拆成大量小事务。
        assert!(OUTPUT_FLUSH_MAX_BYTES_PER_SESSION >= 512 * 1024);
        assert!(RELAY_PUSH_DRAIN_MAX_TRANSPORTED_BYTES_PER_TICK >= 8 * 1024 * 1024);
        assert!(!relay_push_drain_budget_exhausted(1, 1024, Instant::now()));
        assert!(relay_push_drain_budget_exhausted(
            RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK,
            0,
            Instant::now()
        ));
        assert!(relay_push_drain_budget_exhausted(
            1,
            RELAY_PUSH_DRAIN_MAX_TRANSPORTED_BYTES_PER_TICK,
            Instant::now()
        ));
        assert!(relay_push_drain_budget_exhausted(
            1,
            0,
            Instant::now() - Duration::from_secs(60)
        ));
    }

    #[test]
    fn relay_push_drain_budget_stops_after_elapsed_window() {
        let started_at = Instant::now() - RELAY_PUSH_DRAIN_MAX_ELAPSED_PER_TICK;

        // 中文注释：时间预算只在至少 drain 过一个事件后生效，空 tick 不应被 reschedule。
        assert!(relay_push_drain_budget_exhausted(1, 1024, started_at));
        assert!(!relay_push_drain_budget_exhausted(0, 1024, started_at));
    }

    #[test]
    fn relay_push_drain_wakeup_is_queued_once_while_pending() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();
        queue.enqueue(event);
        let mut pending = false;

        queue_relay_push_drain_wakeup(&queue, &mut pending);
        queue_relay_push_drain_wakeup(&queue, &mut pending);

        assert!(pending);
    }

    #[test]
    fn relay_push_drain_wakeup_is_independent_from_external_queue() {
        let pending_event = RelayPushEvent::Output {
            client_id: RelayClientId(2),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();
        let mut pending = false;

        queue.enqueue(pending_event);
        queue_relay_push_drain_wakeup(&queue, &mut pending);

        // 中文注释：内部 deferred output 不能依赖同一个有界 watcher 队列自唤醒。
        // 否则外部 watcher 承压时，snapshot/tail 可能停在 daemon mux 内。
        assert!(pending);
    }

    #[test]
    fn relay_mux_envelopes_wire_len_counts_binary_mux_payload() {
        let envelope = RelayMuxEnvelope::DaemonFrame {
            client_id: RelayClientId(7),
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AQIDBA==".to_owned(),
            },
        };

        assert!(relay_mux_envelopes_wire_len(&[envelope]) > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_writer_streams_frames_from_bounded_queue() {
        let expected_frames = RELAY_MUX_CONTROL_QUEUE_CAPACITY;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = mpsc::channel(expected_frames);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut seen = 0_usize;
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Text(_) | Message::Binary(_) => {
                        seen = seen.saturating_add(1);
                        if seen_tx.send(seen).await.is_err() {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        socket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => break,
                }
            }
        });

        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (sender, _) = socket.split();
        let (writer_queues, writer_control_rx, writer_data_rx) = RelayMuxWriterQueues::new();
        let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let writer_task = tokio::spawn(run_relay_mux_writer(
            "ws://relay-writer-test/ws".to_owned(),
            writer_control_rx,
            writer_data_rx,
            writer_failed_tx,
            sender,
            test_active_clients(),
            writer_queues.ordering(),
        ));
        for index in 0..expected_frames {
            let raw = format!("frame-{index}");
            enqueue_relay_control_message(
                &writer_queues,
                RelayOutKind::Response,
                Message::Text(raw.into()),
            )
            .await
            .unwrap();
        }

        // 中文注释：daemon mux 主循环只负责入队；真正 socket 写出由 writer task 顺序完成。
        let reached = timeout(Duration::from_secs(2), async {
            let mut last_seen = 0_usize;
            while let Some(seen) = seen_rx.recv().await {
                last_seen = seen;
                if seen >= expected_frames {
                    return seen;
                }
            }
            last_seen
        })
        .await
        .expect("relay mux writer should keep writing queued frames");
        assert_eq!(reached, expected_frames);
        assert!(writer_failed_rx.try_recv().is_err());

        drop(writer_queues);
        let _ = writer_task.await;
        server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_writer_drops_queued_frames_after_client_disconnects() {
        let stale_client_id = RelayClientId(901);
        let fresh_client_id = RelayClientId(902);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = mpsc::channel(2);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Binary(raw) => {
                        let envelope = decode_binary_relay_mux_envelope(&raw).unwrap();
                        if let RelayMuxEnvelope::DaemonFrame { client_id, .. } = envelope {
                            if seen_tx.send(client_id).await.is_err() {
                                break;
                            }
                        }
                    }
                    Message::Text(raw) => {
                        let envelope: RelayMuxEnvelope =
                            serde_json::from_str(raw.as_str()).unwrap();
                        if let RelayMuxEnvelope::DaemonFrame { client_id, .. } = envelope {
                            if seen_tx.send(client_id).await.is_err() {
                                break;
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        socket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => break,
                }
            }
        });

        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (sender, _) = socket.split();
        let (writer_queues, writer_control_rx, writer_data_rx) = RelayMuxWriterQueues::new();
        let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, stale_client_id);
        mark_relay_client_active(&active_clients, fresh_client_id);

        enqueue_relay_mux_envelopes(
            "ws://relay-writer-drop-test/ws",
            &writer_queues,
            &active_clients,
            Some(stale_client_id),
            RelayOutKind::PushOutput,
            vec![RelayMuxEnvelope::DaemonFrame {
                client_id: stale_client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "c3RhbGU=".to_owned(),
                },
            }],
        )
        .await
        .unwrap();
        // 中文注释：旧 client 的输出已经进入 writer 队列后才断开，这是高输出快切时的真实竞态。
        mark_relay_client_inactive(&active_clients, stale_client_id);
        let writer_task = tokio::spawn(run_relay_mux_writer(
            "ws://relay-writer-drop-test/ws".to_owned(),
            writer_control_rx,
            writer_data_rx,
            writer_failed_tx,
            sender,
            active_clients.clone(),
            writer_queues.ordering(),
        ));
        enqueue_relay_mux_envelopes(
            "ws://relay-writer-drop-test/ws",
            &writer_queues,
            &active_clients,
            Some(fresh_client_id),
            RelayOutKind::Response,
            vec![RelayMuxEnvelope::DaemonFrame {
                client_id: fresh_client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "ZnJlc2g=".to_owned(),
                },
            }],
        )
        .await
        .unwrap();

        let first_seen = timeout(Duration::from_secs(1), seen_rx.recv())
            .await
            .expect("fresh client frame should not be blocked behind stale output")
            .expect("server should report one daemon frame");
        assert_eq!(first_seen, fresh_client_id);
        assert!(
            timeout(Duration::from_millis(100), seen_rx.recv())
                .await
                .is_err(),
            "stale client frame must be dropped before socket write"
        );
        assert!(writer_failed_rx.try_recv().is_err());

        drop(writer_queues);
        let _ = writer_task.await;
        server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_data_writer_stops_without_waiting_for_stalled_file_tunnel_send() {
        let (write_tx, write_rx) = mpsc::channel(1);
        let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (pending_sink, ready_polls, ready_notify) = PendingSink::new();
        let writer_task = tokio::spawn(run_relay_data_writer(
            "ws://relay-writer-stop-test/ws".to_owned(),
            RelayClientId(903),
            write_rx,
            writer_failed_tx,
            pending_sink,
            stop_rx,
        ));

        write_tx
            .send(RelayDataWrite::Raw {
                kind: RelayOutKind::FileTunnelBody,
                message: Message::Binary(vec![0; 1024]),
            })
            .await
            .unwrap();
        while ready_polls.load(Ordering::Relaxed) == 0 {
            ready_notify.notified().await;
        }
        let _ = stop_tx.send(());

        let outcome = timeout(Duration::from_secs(1), writer_task)
            .await
            .expect("writer task should stop after receiving stop signal")
            .expect("writer task should not panic");
        assert!(matches!(outcome, RelayDataWriterOutcome::Closed));
        assert!(writer_failed_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn relay_data_push_drain_does_not_block_when_writer_queue_is_full() {
        let protocol = test_protocol("relay-data-push-drain-backpressure");
        let (connection, _) = {
            let protocol = protocol.lock().await;
            protocol.start_connection()
        };
        let client_id = RelayClientId(914);
        let session_id = SessionId::new();
        let event = RelayPushEvent::Output {
            client_id,
            session_id,
        };
        let mut connections = HashMap::from([(client_id, connection)]);
        let mut pending_push_events = RelayPushEventQueue::default();
        pending_push_events.enqueue(event);
        let (write_tx, mut write_rx) =
            mpsc::channel::<RelayDataWrite>(RELAY_DATA_WIRE_QUEUE_CAPACITY);
        write_tx
            .try_send(RelayDataWrite::Raw {
                kind: RelayOutKind::Pong,
                message: Message::Pong(Vec::new()),
            })
            .expect("test writer queue should start full");
        let mut push_drain_wake_pending = false;

        // 中文注释：下行 writer 队列满时，terminal push 不能 await 队列腾挪；
        // data pipe 主循环必须回到 select 继续读 stdin/close，并靠定时 wakeup 重试输出。
        let result = timeout(
            Duration::from_millis(50),
            drain_relay_data_push_events(
                "ws://relay-data-push-backpressure/ws",
                ServerId::new(),
                client_id,
                &protocol,
                &mut connections,
                &mut pending_push_events,
                &write_tx,
                &mut push_drain_wake_pending,
            ),
        )
        .await;

        assert!(
            result.is_ok(),
            "relay data push drain must not block the data pipe read loop"
        );
        result.unwrap().unwrap();
        assert!(push_drain_wake_pending);
        assert_eq!(pending_push_events.pop_front(), Some(event));
        assert!(write_rx.try_recv().is_ok());
        assert!(write_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_writer_prioritizes_control_response_over_terminal_push() {
        let output_client_id = RelayClientId(911);
        let response_client_id = RelayClientId(912);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = mpsc::channel(2);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Binary(raw) => {
                        let envelope = decode_binary_relay_mux_envelope(&raw).unwrap();
                        if let RelayMuxEnvelope::DaemonFrame { client_id, .. } = envelope {
                            if seen_tx.send(client_id).await.is_err() {
                                break;
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        socket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => break,
                }
            }
        });

        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (sender, _) = socket.split();
        let (writer_queues, writer_control_rx, writer_data_rx) = RelayMuxWriterQueues::new();
        let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, output_client_id);
        mark_relay_client_active(&active_clients, response_client_id);

        enqueue_relay_mux_envelopes(
            "ws://relay-writer-priority-test/ws",
            &writer_queues,
            &active_clients,
            Some(output_client_id),
            RelayOutKind::PushOutput,
            vec![RelayMuxEnvelope::DaemonFrame {
                client_id: output_client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "b3V0cHV0".to_owned(),
                },
            }],
        )
        .await
        .unwrap();
        // 中文注释：这是 writer 优先级单元测试，直接构造 control 队列，避免
        // control enqueue 的“等待真实写出”语义反过来依赖 writer 已启动。
        writer_queues
            .control
            .try_send(RelayMuxWrite::Envelopes {
                kind: RelayOutKind::Response,
                client_id: Some(response_client_id),
                order: Some(0),
                envelopes: vec![RelayMuxEnvelope::DaemonFrame {
                    client_id: response_client_id,
                    frame: RelayOpaqueFrame::Binary {
                        data_base64: "cmVzcG9uc2U=".to_owned(),
                    },
                }],
                completion: None,
            })
            .unwrap();

        let writer_task = tokio::spawn(run_relay_mux_writer(
            "ws://relay-writer-priority-test/ws".to_owned(),
            writer_control_rx,
            writer_data_rx,
            writer_failed_tx,
            sender,
            active_clients,
            writer_queues.ordering(),
        ));

        let first_seen = timeout(Duration::from_secs(1), seen_rx.recv())
            .await
            .expect("control response should be sent first")
            .expect("server should report first frame");
        let second_seen = timeout(Duration::from_secs(1), seen_rx.recv())
            .await
            .expect("terminal output should still be sent after response")
            .expect("server should report second frame");
        assert_eq!(first_seen, response_client_id);
        assert_eq!(second_seen, output_client_id);
        assert!(writer_failed_rx.try_recv().is_err());

        drop(writer_queues);
        let _ = writer_task.await;
        server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_writer_preserves_same_client_order_across_lanes() {
        let primer_client_id = RelayClientId(921);
        let ordered_client_id = RelayClientId(922);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = mpsc::channel(4);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(message) = socket.next().await {
                let Ok(Message::Binary(raw)) = message else {
                    continue;
                };
                let RelayMuxEnvelope::DaemonFrame { client_id, frame } =
                    decode_binary_relay_mux_envelope(&raw).unwrap()
                else {
                    continue;
                };
                let RelayOpaqueFrame::Binary { data_base64 } = frame else {
                    continue;
                };
                let label =
                    String::from_utf8(general_purpose::STANDARD.decode(data_base64).unwrap())
                        .unwrap();
                if seen_tx.send((client_id, label)).await.is_err() {
                    break;
                }
            }
        });

        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (sender, _) = socket.split();
        let (writer_queues, writer_control_rx, writer_data_rx) = RelayMuxWriterQueues::new();
        let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, primer_client_id);
        mark_relay_client_active(&active_clients, ordered_client_id);

        // 中文注释：先发一个其他 client 的 control，让 writer 下一轮偏向 data lane。
        // 随后同一 client 的 control(order 0) 和 data(order 1) 同时待发；即使 data
        // 先被取出，也必须等 order 0 发完后才能写 socket。
        // 中文注释：这里验证 writer 自身的跨 lane 排序，直接塞队列可以避免
        // control enqueue 等待 writer 完成导致测试在启动 writer 前自我超时。
        writer_queues
            .control
            .try_send(RelayMuxWrite::Envelopes {
                kind: RelayOutKind::Response,
                client_id: Some(primer_client_id),
                order: Some(0),
                envelopes: vec![RelayMuxEnvelope::DaemonFrame {
                    client_id: primer_client_id,
                    frame: RelayOpaqueFrame::Binary {
                        data_base64: general_purpose::STANDARD.encode("primer"),
                    },
                }],
                completion: None,
            })
            .unwrap();
        writer_queues
            .control
            .try_send(RelayMuxWrite::Envelopes {
                kind: RelayOutKind::Response,
                client_id: Some(ordered_client_id),
                order: Some(0),
                envelopes: vec![RelayMuxEnvelope::DaemonFrame {
                    client_id: ordered_client_id,
                    frame: RelayOpaqueFrame::Binary {
                        data_base64: general_purpose::STANDARD.encode("response-before-data"),
                    },
                }],
                completion: None,
            })
            .unwrap();
        writer_queues
            .data
            .try_send(RelayMuxWrite::Envelopes {
                kind: RelayOutKind::PushOutput,
                client_id: Some(ordered_client_id),
                order: Some(1),
                envelopes: vec![RelayMuxEnvelope::DaemonFrame {
                    client_id: ordered_client_id,
                    frame: RelayOpaqueFrame::Binary {
                        data_base64: general_purpose::STANDARD.encode("data-after-response"),
                    },
                }],
                completion: None,
            })
            .unwrap();

        let writer_task = tokio::spawn(run_relay_mux_writer(
            "ws://relay-writer-order-test/ws".to_owned(),
            writer_control_rx,
            writer_data_rx,
            writer_failed_tx,
            sender,
            active_clients,
            writer_queues.ordering(),
        ));

        let mut frames = Vec::new();
        for _ in 0..3 {
            frames.push(
                timeout(Duration::from_secs(1), seen_rx.recv())
                    .await
                    .expect("writer should emit all ordered frames")
                    .expect("server should report frame"),
            );
        }
        assert_eq!(
            frames,
            vec![
                (primer_client_id, "primer".to_owned()),
                (ordered_client_id, "response-before-data".to_owned()),
                (ordered_client_id, "data-after-response".to_owned()),
            ]
        );
        assert!(writer_failed_rx.try_recv().is_err());

        drop(writer_queues);
        let _ = writer_task.await;
        server_task.abort();
    }

    #[test]
    fn relay_mux_push_slot_backpressures_only_data_lane() {
        let (writer_queues, _writer_control_rx, _writer_data_rx) = RelayMuxWriterQueues::new();
        let mut permits = Vec::new();

        for _ in 0..RELAY_MUX_DATA_QUEUE_CAPACITY {
            permits.push(
                try_reserve_relay_mux_push_slot(&writer_queues)
                    .unwrap()
                    .expect("data lane should still have capacity"),
            );
        }

        assert!(
            try_reserve_relay_mux_push_slot(&writer_queues)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            writer_queues.control.capacity(),
            RELAY_MUX_CONTROL_QUEUE_CAPACITY
        );

        permits.pop();
        assert!(
            try_reserve_relay_mux_push_slot(&writer_queues)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_enqueue_drops_inactive_client_before_writer_queue() {
        let client_id = RelayClientId(701);
        let (writer_queues, _writer_control_rx, mut writer_data_rx) = RelayMuxWriterQueues::new();
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, client_id);
        let sent = enqueue_relay_mux_envelopes(
            "ws://relay-direct-lock-test/ws",
            &writer_queues,
            &active_clients,
            Some(client_id),
            RelayOutKind::PushOutput,
            vec![RelayMuxEnvelope::DaemonFrame {
                client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "AQ==".to_owned(),
                },
            }],
        )
        .await
        .unwrap();
        assert!(sent > 0);
        let Some(RelayMuxWrite::Envelopes {
            client_id: queued_client,
            ..
        }) = writer_data_rx.recv().await
        else {
            panic!("expected queued daemon frame");
        };
        assert_eq!(queued_client, Some(client_id));

        mark_relay_client_inactive(&active_clients, client_id);
        let dropped = enqueue_relay_mux_envelopes(
            "ws://relay-direct-lock-test/ws",
            &writer_queues,
            &active_clients,
            Some(client_id),
            RelayOutKind::PushOutput,
            vec![RelayMuxEnvelope::DaemonFrame {
                client_id,
                frame: RelayOpaqueFrame::Binary {
                    data_base64: "Ag==".to_owned(),
                },
            }],
        )
        .await
        .unwrap();
        assert_eq!(dropped, 0);
        assert!(writer_data_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_response_enqueue_fails_fast_when_control_queue_is_full() {
        let client_id = RelayClientId(702);
        let (writer_queues, _writer_control_rx, _writer_data_rx) = RelayMuxWriterQueues::new();
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, client_id);

        for _ in 0..RELAY_MUX_CONTROL_QUEUE_CAPACITY {
            writer_queues
                .control
                .try_send(RelayMuxWrite::Raw {
                    kind: RelayOutKind::Pong,
                    message: Message::Pong(Vec::new().into()),
                })
                .expect("test should fill control queue exactly");
        }

        // 中文注释：relay 已经给 browser 回了 route_ready；daemon 侧如果因为旧输出把
        // hello/e2ee 卡在 control 队列里，前端只能等到握手超时。这里必须快速失败并重建 mux。
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            enqueue_relay_mux_envelopes(
                "ws://relay-control-full-test/ws",
                &writer_queues,
                &active_clients,
                Some(client_id),
                RelayOutKind::Response,
                vec![RelayMuxEnvelope::DaemonFrame {
                    client_id,
                    frame: RelayOpaqueFrame::Text {
                        data: "{}".to_owned(),
                    },
                }],
            ),
        )
        .await;

        assert!(
            matches!(result, Ok(Err(RelayConnectorError::SendFailed))),
            "response enqueue must fail fast instead of hanging behind a full control queue: {result:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_response_enqueue_times_out_when_writer_does_not_complete_control_write() {
        let client_id = RelayClientId(703);
        let (writer_queues, _writer_control_rx, _writer_data_rx) = RelayMuxWriterQueues::new();
        let active_clients = test_active_clients();
        mark_relay_client_active(&active_clients, client_id);

        // 中文注释：control 入队成功不等于已经写到 daemon->relay WebSocket。
        // writer 卡住时必须让 mux 重连，不能让 browser 停在 route_ready 后等待握手超时。
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            enqueue_relay_mux_envelopes(
                "ws://relay-control-stalled-test/ws",
                &writer_queues,
                &active_clients,
                Some(client_id),
                RelayOutKind::Response,
                vec![RelayMuxEnvelope::DaemonFrame {
                    client_id,
                    frame: RelayOpaqueFrame::Text {
                        data: "{}".to_owned(),
                    },
                }],
            ),
        )
        .await;

        assert!(
            matches!(result, Ok(Err(RelayConnectorError::SendTimeout))),
            "response enqueue must detect a stalled writer instead of reporting success: {result:?}",
        );
    }

    fn temp_state_path(name: &str) -> PathBuf {
        let counter = TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let state_dir = std::env::temp_dir().join(format!("tr-{}-{counter}", std::process::id()));
        // supervisor runtime 目录由 state parent 推导；并行测试必须隔离 parent，避免互相清理 orphan。
        // Unix socket 有较短路径长度限制，因此测试目录名必须保持紧凑。
        fs::create_dir_all(&state_dir).unwrap();
        state_dir.join(format!("{name}.json"))
    }

    #[test]
    fn parses_relay_base_url_and_builds_unified_ws_url() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();
        let url = base.daemon_mux_url(server_id);

        assert_eq!(url, "ws://127.0.0.1:8080/ws");
    }

    #[test]
    fn parses_wss_relay_base_url_and_preserves_secure_scheme() {
        let base = RelayBaseUrl::parse("wss://relay.example:443").unwrap();

        assert_eq!(
            base.daemon_mux_url(ServerId::new()),
            "wss://relay.example:443/ws"
        );
    }

    #[test]
    fn parses_wss_relay_base_path_and_builds_single_layer_ws_url() {
        let base = RelayBaseUrl::parse("wss://termd.yiln.de/ws").unwrap();

        assert_eq!(base.canonical_url(), "wss://termd.yiln.de/ws");
        assert_eq!(
            base.daemon_mux_url(ServerId::new()),
            "wss://termd.yiln.de/ws"
        );
    }

    #[test]
    fn relay_base_url_builds_unified_pairing_ws_url() {
        let base = RelayBaseUrl::parse("wss://termd.yiln.de/ws").unwrap();

        assert_eq!(base.client_url_template(), "wss://termd.yiln.de/ws");
        assert_eq!(
            base.client_url_template_with_auth(Some("relay secret")),
            "wss://termd.yiln.de/ws?relay_token=relay%20secret"
        );
    }

    #[test]
    fn relay_base_url_preserves_public_path_prefix() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("wss://relay.example/termd/ws/").unwrap();

        assert_eq!(base.canonical_url(), "wss://relay.example/termd/ws");
        assert_eq!(
            base.daemon_mux_url(server_id),
            "wss://relay.example/termd/ws"
        );
        assert_eq!(base.client_url_template(), "wss://relay.example/termd/ws");
    }

    #[test]
    fn relay_base_url_canonical_url_drops_trailing_slash_variants() {
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();

        assert_eq!(base.canonical_url(), "ws://127.0.0.1:8080");
    }

    #[test]
    fn unified_ws_url_can_carry_relay_auth_token_without_debug_leakage() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();
        let url = base.daemon_mux_url_with_auth(server_id, Some("relay-secret-1"));

        assert_eq!(url, "ws://127.0.0.1:8080/ws?relay_token=relay-secret-1");
        assert!(!format!("{base:?}").contains("relay-secret-1"));
    }

    #[test]
    fn relay_tls_kx_groups_exclude_hybrid_post_quantum_groups() {
        let names = relay_tls_kx_groups()
            .into_iter()
            .map(|group| format!("{:?}", group.name()))
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["X25519", "secp256r1", "secp384r1"]);
        assert!(!names.iter().any(|name| name.contains("MLKEM")));
    }

    #[test]
    fn parses_http_and_socks5_relay_proxy_urls() {
        let http = RelayProxyUrl::parse("http://127.0.0.1:3128").unwrap();
        assert_eq!(http.scheme(), RelayProxyScheme::Http);
        assert_eq!(http.authority(), "127.0.0.1:3128");

        let socks5 = RelayProxyUrl::parse("socks5://proxy.example:1080").unwrap();
        assert_eq!(socks5.scheme(), RelayProxyScheme::Socks5);
        assert_eq!(socks5.authority(), "proxy.example:1080");
    }

    #[test]
    fn relay_proxy_url_rejects_unsupported_or_ambiguous_values() {
        assert!(RelayProxyUrl::parse("https://proxy.example:443").is_err());
        assert!(RelayProxyUrl::parse("http://proxy.example").is_err());
        assert!(RelayProxyUrl::parse("socks5://user:pass@proxy.example:1080").is_err());
        assert!(RelayProxyUrl::parse("socks5h://proxy.example:1080").is_err());
        assert!(RelayProxyUrl::parse("http://proxy.example:3128/path").is_err());
    }

    #[test]
    fn http_connect_request_uses_target_authority_without_secret_url() {
        let request = http_connect_request("relay.example:443", "proxy.local:3128");

        assert_eq!(
            request,
            "CONNECT relay.example:443 HTTP/1.1\r\nHost: relay.example:443\r\nProxy-Connection: Keep-Alive\r\n\r\n"
        );
        assert!(!request.contains("relay_token"));
        assert!(!request.contains("proxy.local"));
    }

    #[test]
    fn socks5_connect_request_encodes_domain_target() {
        let request = socks5_connect_request("relay.example", 443).unwrap();

        assert_eq!(
            request,
            vec![
                0x05, 0x01, 0x00, // no-auth greeting
                0x05, 0x01, 0x00, 0x03, 13, b'r', b'e', b'l', b'a', b'y', b'.', b'e', b'x', b'a',
                b'm', b'p', b'l', b'e', 0x01, 0xbb,
            ]
        );
    }

    #[test]
    fn rejects_unsupported_relay_urls() {
        assert!(RelayBaseUrl::parse("http://127.0.0.1:8080").is_err());
        assert!(RelayBaseUrl::parse("ws://127.0.0.1:8080/path").is_err());
        assert!(RelayBaseUrl::parse("wss://termd.yiln.de/ws/server/client").is_err());
        assert!(RelayBaseUrl::parse("wss://termd.yiln.de/ws/server/daemon-mux").is_err());
        assert!(RelayBaseUrl::parse("wss://termd.yiln.de/ws?relay_token=secret").is_err());
        assert!(RelayBaseUrl::parse("wss://termd.yiln.de/ws#fragment").is_err());
    }

    #[test]
    fn relay_reconnect_policy_clamps_zero_and_grows_to_max() {
        let policy = RelayReconnectPolicy::from_config(RelayReconnectConfig {
            initial_delay_ms: 0,
            max_delay_ms: 5,
            heartbeat_interval_ms: 0,
        });

        assert_eq!(policy.first_retry_delay(), Duration::from_millis(1));
        assert_eq!(policy.heartbeat_interval(), Duration::from_millis(1));
        assert_eq!(
            policy.next_retry_delay(Duration::from_millis(1)),
            Duration::from_millis(2)
        );
        assert_eq!(
            policy.next_retry_delay(Duration::from_millis(4)),
            Duration::from_millis(5)
        );
        assert_eq!(
            policy.next_retry_delay(Duration::from_millis(5)),
            Duration::from_millis(5)
        );

        let inverted = RelayReconnectPolicy::from_config(RelayReconnectConfig {
            initial_delay_ms: 50,
            max_delay_ms: 10,
            heartbeat_interval_ms: 20,
        });

        assert_eq!(inverted.first_retry_delay(), Duration::from_millis(50));
        assert_eq!(inverted.heartbeat_interval(), Duration::from_millis(20));
        assert_eq!(
            inverted.next_retry_delay(Duration::from_millis(50)),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn relay_websocket_config_sets_transport_size_limits() {
        let config = relay_websocket_config();

        assert_eq!(config.max_frame_size, Some(RELAY_MAX_FRAME_SIZE));
        assert_eq!(config.max_message_size, Some(RELAY_MAX_MESSAGE_SIZE));
        assert!(RELAY_MAX_FRAME_SIZE <= RELAY_MAX_MESSAGE_SIZE);
    }

    #[test]
    fn relay_daemon_mux_uses_websocket_ping_only_for_transport_keepalive() {
        assert_eq!(
            RelayReconnectPolicy::default().heartbeat_interval(),
            Duration::from_secs(10)
        );
        assert!(relay_daemon_mux_idle_ping_enabled());
        // 中文注释：Ping 只用于穿透公网代理的空闲保活，不进入 mux 业务协议。
        // 是否断开只能由底层 WebSocket/TCP close/read/write error 暴露。
        assert_eq!(
            relay_send_deadline(RelayOutKind::Pong),
            Some(RELAY_PONG_DEADLINE)
        );
        assert_eq!(relay_send_deadline(RelayOutKind::FileTunnelBody), None);
    }

    #[test]
    fn relay_idle_ping_requires_full_outbound_idle_interval() {
        let start = Instant::now();
        let heartbeat_interval = Duration::from_millis(10);

        assert!(!relay_idle_ping_due(
            start + Duration::from_millis(9),
            start,
            start,
            heartbeat_interval
        ));
        assert!(relay_idle_ping_due(
            start + heartbeat_interval,
            start,
            start,
            heartbeat_interval
        ));
        assert!(!relay_idle_ping_due(
            start + Duration::from_millis(20),
            start + Duration::from_millis(15),
            start,
            heartbeat_interval
        ));
    }

    #[test]
    fn relay_traffic_ignores_empty_output_pushes() {
        let mut traffic = RelayTrafficCounters::default();

        traffic.record_out(RelayOutKind::PushOutput, 0, 0);

        assert!(!traffic.has_activity());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_supervisor_retries_after_close_and_keeps_control_alive_with_idle_ping() {
        let state = MockMuxState::default();
        let app = axum::Router::new()
            .route("/ws", get(mock_daemon_mux_ws))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let policy = RelayReconnectPolicy::from_config(RelayReconnectConfig {
            initial_delay_ms: 10,
            max_delay_ms: 20,
            heartbeat_interval_ms: 10,
        });
        let protocol = test_protocol("reconnect-supervisor");
        let connector = tokio::spawn(run_relay_mux_with_reconnect_base(
            base, None, None, policy, protocol,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.attempts.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert!(
            state.idle_pings.load(Ordering::SeqCst) >= 1,
            "daemon control 空闲时应发送 WebSocket Ping，避免公网代理清理静默主干"
        );
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            2,
            "首个连接故意失败后，稳定连接应只重拨一次并保持单条 daemon control"
        );

        connector.abort();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_control_open_data_creates_raw_daemon_data_pipe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let protocol = test_protocol("relay-control-data-pipe");
        let server_id = protocol.lock().await.server_id();
        let client_id = RelayClientId(77);
        let data_token = ProtoNonce("test-data-token".to_owned());

        let server = tokio::spawn(async move {
            let (control_tcp, _) = listener.accept().await.unwrap();
            let mut control_socket = tokio_tungstenite::accept_async(control_tcp).await.unwrap();
            complete_relay_route_prelude(&mut control_socket, server_id, RouteRole::DaemonControl)
                .await;
            let open_data = RelayControlEnvelope::OpenData {
                client_id,
                data_token: data_token.clone(),
            };
            control_socket
                .send(Message::Text(
                    serde_json::to_string(&open_data).unwrap().into(),
                ))
                .await
                .unwrap();

            let (data_tcp, _) = listener.accept().await.unwrap();
            let mut data_socket = tokio_tungstenite::accept_async(data_tcp).await.unwrap();
            let route_hello = read_route_hello_from_connector(&mut data_socket).await;
            assert_eq!(route_hello.server_id, server_id);
            assert_eq!(route_hello.role, RouteRole::DaemonData);
            assert!(
                route_hello.route_generation.is_some(),
                "daemon data route should inherit mux route_generation"
            );
            assert_eq!(route_hello.client_id, Some(client_id));
            assert_eq!(route_hello.data_token, Some(data_token));
            send_route_ready_to_connector(&mut data_socket, server_id, RouteRole::DaemonData).await;

            let initial = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let message = data_socket.next().await.unwrap().unwrap();
                    match message {
                        Message::Text(raw) => break raw.to_string(),
                        Message::Ping(payload) => {
                            data_socket.send(Message::Pong(payload)).await.unwrap();
                        }
                        Message::Pong(_) | Message::Frame(_) => continue,
                        Message::Binary(raw) => {
                            panic!("expected daemon initial JSON text, got binary {raw:?}")
                        }
                        Message::Close(frame) => panic!("data pipe closed early: {frame:?}"),
                    }
                }
            })
            .await
            .expect("daemon data pipe should send initial hello");
            let envelope: JsonEnvelope = serde_json::from_str(&initial).unwrap();
            assert_eq!(envelope.kind, MessageType::Hello);

            data_socket.close(None).await.unwrap();
            control_socket.close(None).await.unwrap();
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::time::timeout(
            Duration::from_secs(4),
            connect_relay_mux_base(base, None, protocol),
        )
        .await
        .expect("relay connector should finish after mock relay closes control");
        connector.unwrap();
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_idle_data_pipe_accepts_assignment_and_sends_initial_hello() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let protocol = test_protocol("relay-idle-data-assignment");
        let server_id = protocol.lock().await.server_id();
        let client_id = RelayClientId(5150);
        let data_token = ProtoNonce("idle-data-token".to_owned());
        let route_generation = ProtoNonce("idle-generation-assignment".to_owned());
        let expected_route_generation = route_generation.clone();

        let server = tokio::spawn(async move {
            let (data_tcp, _) = listener.accept().await.unwrap();
            let mut data_socket = tokio_tungstenite::accept_async(data_tcp).await.unwrap();
            let route_hello = read_route_hello_from_connector(&mut data_socket).await;
            assert_eq!(route_hello.server_id, server_id);
            assert_eq!(route_hello.role, RouteRole::DaemonData);
            assert_eq!(
                route_hello.route_generation,
                Some(expected_route_generation.clone())
            );
            assert_eq!(route_hello.client_id, None);
            assert_eq!(route_hello.data_token, None);
            send_route_ready_to_connector(&mut data_socket, server_id, RouteRole::DaemonData).await;

            let assign = RelayControlEnvelope::OpenData {
                client_id,
                data_token,
            };
            data_socket
                .send(Message::Text(
                    serde_json::to_string(&assign).unwrap().into(),
                ))
                .await
                .unwrap();

            let initial = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match data_socket.next().await.unwrap().unwrap() {
                        Message::Text(raw) => break raw.to_string(),
                        Message::Ping(payload) => {
                            data_socket.send(Message::Pong(payload)).await.unwrap();
                        }
                        Message::Pong(_) | Message::Frame(_) => continue,
                        Message::Binary(raw) => {
                            panic!("expected daemon initial JSON text, got binary {raw:?}")
                        }
                        Message::Close(frame) => panic!("idle data pipe closed early: {frame:?}"),
                    }
                }
            })
            .await
            .expect("assigned idle daemon data pipe should send initial hello");
            let envelope: JsonEnvelope = serde_json::from_str(&initial).unwrap();
            assert_eq!(envelope.kind, MessageType::Hello);
            // 中文注释：daemon 在 mock relay 关闭前可能已经因为测试结束主动断开；
            // 这里的 close 只负责收尾，BrokenPipe 不应让复用语义测试随机失败。
            let _ = data_socket.close(None).await;
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let task = tokio::spawn(run_relay_idle_data_connection(
            base,
            None,
            None,
            protocol,
            server_id,
            route_generation,
            1,
            event_tx,
        ));

        let ready = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("idle data ready event should arrive")
            .expect("idle data event channel should stay open");
        assert!(matches!(ready, RelayIdleDataEvent::Ready { task_id: 1 }));

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("idle data assignment event should arrive")
            .expect("idle data event channel should stay open");
        assert!(matches!(
            event,
            RelayIdleDataEvent::Assigned {
                task_id: 1,
                client_id: observed
            } if observed == client_id
        ));

        server.await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_idle_data_pipe_closes_socket_after_client_disconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let protocol = test_protocol("relay-idle-data-close-after-disconnect");
        let server_id = protocol.lock().await.server_id();
        let client_id = RelayClientId(6101);
        let route_generation = ProtoNonce("idle-generation-disconnect".to_owned());
        let expected_route_generation = route_generation.clone();

        let server = tokio::spawn(async move {
            let (data_tcp, _) = listener.accept().await.unwrap();
            let mut data_socket = tokio_tungstenite::accept_async(data_tcp).await.unwrap();
            let route_hello = read_route_hello_from_connector(&mut data_socket).await;
            assert_eq!(route_hello.server_id, server_id);
            assert_eq!(route_hello.role, RouteRole::DaemonData);
            assert_eq!(
                route_hello.route_generation,
                Some(expected_route_generation.clone())
            );
            assert_eq!(route_hello.client_id, None);
            assert_eq!(route_hello.data_token, None);
            send_route_ready_to_connector(&mut data_socket, server_id, RouteRole::DaemonData).await;

            let assign = RelayControlEnvelope::OpenData {
                client_id,
                data_token: ProtoNonce(format!("idle-close-token-{}", client_id.0)),
            };
            data_socket
                .send(Message::Text(
                    serde_json::to_string(&assign).unwrap().into(),
                ))
                .await
                .unwrap();

            let hello = read_json_envelope_from_connector(&mut data_socket).await;
            assert_eq!(hello.kind, MessageType::Hello);

            let disconnect = RelayControlEnvelope::ClientDisconnected { client_id };
            data_socket
                .send(Message::Ping(
                    encode_relay_data_control(&disconnect).unwrap(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match data_socket.next().await {
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = data_socket.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await
            .expect("daemon should close data pipe after client disconnect");
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let task = tokio::spawn(run_relay_idle_data_connection(
            base,
            None,
            None,
            protocol,
            server_id,
            route_generation,
            1,
            event_tx,
        ));

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RelayIdleDataEvent::Ready { task_id: 1 }
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RelayIdleDataEvent::Assigned {
                task_id: 1,
                client_id: observed
            } if observed == client_id
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RelayIdleDataEvent::Closed { task_id: 1 }
        ));

        server.await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_control_open_data_starts_data_pipes_concurrently() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let protocol = test_protocol("relay-control-concurrent-data-pipes");
        let server_id = protocol.lock().await.server_id();
        let first_client = RelayClientId(811);
        let second_client = RelayClientId(812);
        let first_token = ProtoNonce("first-data-token".to_owned());
        let second_token = ProtoNonce("second-data-token".to_owned());

        let server = tokio::spawn(async move {
            let (control_tcp, _) = listener.accept().await.unwrap();
            let mut control_socket = tokio_tungstenite::accept_async(control_tcp).await.unwrap();
            complete_relay_route_prelude(&mut control_socket, server_id, RouteRole::DaemonControl)
                .await;

            for (client_id, data_token) in [
                (first_client, first_token.clone()),
                (second_client, second_token.clone()),
            ] {
                let open_data = RelayControlEnvelope::OpenData {
                    client_id,
                    data_token,
                };
                control_socket
                    .send(Message::Text(
                        serde_json::to_string(&open_data).unwrap().into(),
                    ))
                    .await
                    .unwrap();
            }

            let mut seen = HashSet::new();
            tokio::time::timeout(Duration::from_millis(800), async {
                while seen.len() < 2 {
                    let (data_tcp, _) = listener.accept().await.unwrap();
                    let mut data_socket = tokio_tungstenite::accept_async(data_tcp).await.unwrap();
                    let route_hello = read_route_hello_from_connector(&mut data_socket).await;
                    assert_eq!(route_hello.server_id, server_id);
                    assert_eq!(route_hello.role, RouteRole::DaemonData);
                    let client_id = route_hello
                        .client_id
                        .expect("data route should name client");
                    seen.insert(client_id);
                    // 中文注释：故意不回第一个 route_ready；第二条 data pipe 仍必须能并发发出
                    // route_hello。否则一个慢握手会挡住后续快速切换的新 client。
                    if client_id == second_client {
                        send_route_ready_to_connector(
                            &mut data_socket,
                            server_id,
                            RouteRole::DaemonData,
                        )
                        .await;
                    }
                }
            })
            .await
            .expect("both daemon data pipes should connect without serial route_ready blocking");

            assert!(seen.contains(&first_client));
            assert!(seen.contains(&second_client));
            control_socket.close(None).await.unwrap();
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::time::timeout(
            Duration::from_secs(4),
            connect_relay_mux_base(base, None, protocol),
        )
        .await
        .expect("relay connector should finish after mock relay closes control");
        connector.unwrap();
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_control_client_disconnect_aborts_pending_data_task() {
        let client_id = RelayClientId(909);
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let mut data_tasks = RelayDataTaskMap::new();
        let task = tokio::spawn(async move {
            // 中文注释：模拟卡在 WebSocket connect 阶段的 data pipe；旧 client 断开时
            // control 线必须能主动 abort，而不是等连接超时自然结束。
            let _drop_notify = DropNotify(Some(dropped_tx));
            std::future::pending::<()>().await;
        });
        data_tasks.insert(client_id, task);

        handle_relay_control_envelope(
            RelayControlEnvelope::ClientDisconnected { client_id },
            RelayBaseUrl::parse("ws://127.0.0.1:1").unwrap(),
            None,
            None,
            test_protocol("relay-control-client-disconnect-aborts-data-task"),
            ServerId::new(),
            ProtoNonce("disconnect-route-generation".to_owned()),
            &mut data_tasks,
        )
        .await
        .unwrap();

        assert!(
            data_tasks.is_empty(),
            "client 断开后不能继续保留旧 data pipe 任务句柄"
        );
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("pending data pipe should be aborted promptly")
            .expect("drop notification should be delivered");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_route_ready_times_out_when_relay_never_acks() {
        let app = axum::Router::new().route("/ws", get(mock_route_ready_timeout_ws));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let protocol = test_protocol("relay-route-ready-timeout");
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            connect_relay_mux_base_once(base, None, None, protocol, RELAY_HEARTBEAT_INTERVAL),
        )
        .await
        .expect("relay connector should return before the outer test timeout");

        assert!(matches!(
            result,
            Err(RelayConnectorError::RouteReadyTimeout)
        ));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_replies_to_relay_websocket_ping_without_business_traffic() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let protocol = test_protocol("relay-control-pong");
        let server_id = protocol.lock().await.server_id();

        let server = tokio::spawn(async move {
            let mut socket = accept_relay_mux_pair(&listener, server_id).await;

            socket.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
            let pong = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    match socket.next().await.unwrap().unwrap() {
                        Message::Pong(payload) => break payload,
                        Message::Ping(payload) => {
                            socket.send(Message::Pong(payload)).await.unwrap();
                        }
                        Message::Text(raw) => panic!("unexpected relay mux text frame: {raw:?}"),
                        Message::Binary(raw) => {
                            panic!("unexpected relay mux binary frame: {raw:?}");
                        }
                        Message::Close(frame) => panic!("relay mux closed unexpectedly: {frame:?}"),
                        Message::Frame(_) => {}
                    }
                }
            })
            .await
            .expect("daemon mux should reply to relay control ping before heartbeat timeout");

            assert_eq!(pong, vec![1, 2, 3]);
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_once(
            base,
            None,
            None,
            protocol,
            RELAY_HEARTBEAT_INTERVAL,
        ));

        server.await.unwrap();
        connector.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_waits_for_transport_close_when_idle_ping_is_unanswered() {
        let protocol = test_protocol("relay-pong-real-close");
        let server_id = protocol.lock().await.server_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_raw_daemon_mux_ignores_ping_then_closes_after_pings(stream, server_id, 3).await;
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            connect_relay_mux_base_once(base, None, None, protocol, Duration::from_millis(10)),
        )
        .await
        .expect("daemon mux should return when the relay side really closes transport");

        assert!(
            result.is_ok(),
            "未收到 Pong 不能主动判死；只有真实 close/read error 才能结束连接"
        );
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_does_not_ack_timeout_when_relay_ping_still_arrives() {
        let protocol = test_protocol("relay-no-ack-timeout");
        let server_id = protocol.lock().await.server_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_raw_daemon_mux_sends_relay_ping_but_ignores_daemon_ping(stream, server_id).await;
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            connect_relay_mux_base_once(base, None, None, protocol, Duration::from_millis(10)),
        )
        .await;

        // 中文注释：旧实现会因为 daemon Ping 没收到 Pong 主动断开；新模型只把 Ping
        // 当保活，连接没有真实 close/read/write error 时应继续运行到测试外层超时。
        assert!(result.is_err());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_does_not_require_application_keepalive_ack() {
        let protocol = test_protocol("relay-mux-no-keepalive-ack");
        let server_id = protocol.lock().await.server_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_raw_daemon_mux_answers_ws_ping_but_ignores_mux_keepalive(stream, server_id)
                .await;
        });

        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            connect_relay_mux_base_once(base, None, None, protocol, Duration::from_millis(10)),
        )
        .await;

        // 中文注释：新模型不再发送需要 ACK 的 mux keepalive。测试超时说明连接仍在运行，
        // 而不是因为缺少应用层 ack 被 daemon 主动断开。
        assert!(result.is_err());
        server.abort();
    }

    #[derive(Clone, Default)]
    struct MockMuxState {
        attempts: Arc<AtomicUsize>,
        idle_pings: Arc<AtomicUsize>,
    }

    async fn mock_route_ready_timeout_ws(websocket: WebSocketUpgrade) -> impl IntoResponse {
        websocket.on_upgrade(move |mut socket| async move {
            let _ = read_axum_route_hello(&mut socket).await;
            tokio::time::sleep(Duration::from_secs(8)).await;
        })
    }

    async fn mock_daemon_mux_ws(
        websocket: WebSocketUpgrade,
        State(state): State<MockMuxState>,
    ) -> impl IntoResponse {
        websocket.on_upgrade(move |mut socket| async move {
            let attempt = state.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                // 首次连接立即关闭，用来证明 supervisor 会按退避重新拨号。
                return;
            }
            let Some(route_hello) = read_axum_route_hello(&mut socket).await else {
                return;
            };
            if !matches!(route_hello.role, RouteRole::DaemonControl) {
                return;
            }
            let route_ready = Envelope::new(
                MessageType::RouteReady,
                RouteReadyPayload {
                    server_id: route_hello.server_id,
                    role: route_hello.role,
                },
            );
            let raw = serde_json::to_string(&route_ready).unwrap();
            if socket.send(AxumMessage::Text(raw.into())).await.is_err() {
                return;
            }

            while let Some(message) = socket.next().await {
                match message {
                    Ok(AxumMessage::Binary(raw)) => {
                        if let Ok(RelayMuxEnvelope::Keepalive { nonce }) =
                            decode_binary_relay_mux_envelope(&raw)
                        {
                            let ack = RelayMuxEnvelope::KeepaliveAck { nonce };
                            let raw = encode_binary_relay_mux_envelope(&ack).unwrap();
                            if socket.send(AxumMessage::Binary(raw.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(AxumMessage::Ping(payload)) => {
                        state.idle_pings.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.send(AxumMessage::Pong(payload)).await;
                    }
                    Ok(AxumMessage::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
    }

    async fn handle_raw_daemon_mux_ignores_ping_then_closes_after_pings(
        mut stream: TcpStream,
        server_id: ServerId,
        ping_count: usize,
    ) {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).await.is_err() {
                return;
            }
            request.push(byte[0]);
        }
        let request_text = String::from_utf8_lossy(&request);
        let Some(key) = request_text.lines().find_map(|line| {
            line.strip_prefix("Sec-WebSocket-Key:")
                .map(|value| value.trim())
        }) else {
            return;
        };
        let accept_key =
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept_key}\r\n\
             \r\n"
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }

        let Some((opcode, _payload)) = read_raw_ws_frame(&mut stream).await else {
            return;
        };
        if opcode != 0x1 && opcode != 0x2 {
            return;
        }
        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id,
                role: RouteRole::DaemonMux,
            },
        );
        let raw = serde_json::to_string(&route_ready).unwrap();
        if write_raw_ws_frame(&mut stream, 0x1, raw.as_bytes())
            .await
            .is_err()
        {
            return;
        }

        let mut ping_seen = 0;
        while let Some((opcode, payload)) = read_raw_ws_frame(&mut stream).await {
            match opcode {
                // 中文注释：故意忽略 WebSocket Ping，模拟 NAT/代理半开后控制帧无回包。
                0x9 => {
                    ping_seen += 1;
                    if ping_seen >= ping_count {
                        let _ = write_raw_ws_frame(&mut stream, 0x8, &[]).await;
                        return;
                    }
                }
                0x2 => {
                    if let Ok(RelayMuxEnvelope::Keepalive { nonce }) =
                        decode_binary_relay_mux_envelope(&payload)
                    {
                        let ack = RelayMuxEnvelope::KeepaliveAck { nonce };
                        if let Ok(raw) = encode_binary_relay_mux_envelope(&ack) {
                            if write_raw_ws_frame(&mut stream, 0x2, &raw).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                0x8 => break,
                _ => {}
            }
        }
    }

    async fn handle_raw_daemon_mux_sends_relay_ping_but_ignores_daemon_ping(
        mut stream: TcpStream,
        server_id: ServerId,
    ) {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).await.is_err() {
                return;
            }
            request.push(byte[0]);
        }
        let request_text = String::from_utf8_lossy(&request);
        let Some(key) = request_text.lines().find_map(|line| {
            line.strip_prefix("Sec-WebSocket-Key:")
                .map(|value| value.trim())
        }) else {
            return;
        };
        let accept_key =
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept_key}\r\n\
             \r\n"
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }

        let Some((opcode, _payload)) = read_raw_ws_frame(&mut stream).await else {
            return;
        };
        if opcode != 0x1 && opcode != 0x2 {
            return;
        }
        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id,
                role: RouteRole::DaemonMux,
            },
        );
        let raw = serde_json::to_string(&route_ready).unwrap();
        if write_raw_ws_frame(&mut stream, 0x1, raw.as_bytes())
            .await
            .is_err()
        {
            return;
        }

        let mut relay_ping = tokio::time::interval(Duration::from_millis(5));
        relay_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut relay_ping_nonce = 0_u64;
        loop {
            tokio::select! {
                _ = relay_ping.tick() => {
                    relay_ping_nonce = relay_ping_nonce.wrapping_add(1);
                    // 中文注释：持续给 daemon 发 Ping，模拟 relay->daemon 方向仍可达。
                    // daemon 如果把这些入站 Ping 当作出站可达证明，就会永远不重连。
                    if write_raw_ws_frame(&mut stream, 0x9, &relay_ping_nonce.to_be_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                frame = read_raw_ws_frame(&mut stream) => {
                    let Some((opcode, payload)) = frame else {
                        return;
                    };
                    match opcode {
                        // 中文注释：故意不回 daemon 的 Ping，模拟 daemon->relay 方向半断。
                        0x9 => {}
                        // daemon 对 relay Ping 的 Pong 会证明入站方向可达，但不能证明出站方向。
                        0xA => {}
                        0x2 => {
                            if let Ok(RelayMuxEnvelope::Keepalive { nonce }) =
                                decode_binary_relay_mux_envelope(&payload)
                            {
                                let ack = RelayMuxEnvelope::KeepaliveAck { nonce };
                                if let Ok(raw) = encode_binary_relay_mux_envelope(&ack) {
                                    if write_raw_ws_frame(&mut stream, 0x2, &raw).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        0x8 => return,
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_raw_daemon_mux_answers_ws_ping_but_ignores_mux_keepalive(
        mut stream: TcpStream,
        server_id: ServerId,
    ) {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).await.is_err() {
                return;
            }
            request.push(byte[0]);
        }
        let request_text = String::from_utf8_lossy(&request);
        let Some(key) = request_text.lines().find_map(|line| {
            line.strip_prefix("Sec-WebSocket-Key:")
                .map(|value| value.trim())
        }) else {
            return;
        };
        let accept_key =
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept_key}\r\n\
             \r\n"
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }

        let Some((opcode, _payload)) = read_raw_ws_frame(&mut stream).await else {
            return;
        };
        if opcode != 0x1 && opcode != 0x2 {
            return;
        }
        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id,
                role: RouteRole::DaemonMux,
            },
        );
        let raw = serde_json::to_string(&route_ready).unwrap();
        if write_raw_ws_frame(&mut stream, 0x1, raw.as_bytes())
            .await
            .is_err()
        {
            return;
        }
        while let Some((opcode, payload)) = read_raw_ws_frame(&mut stream).await {
            match opcode {
                // 中文注释：WebSocket 控制帧正常响应；如果旧 daemon 仍发 mux keepalive，
                // 这里故意不 ack。新 daemon 不依赖这条应用层 ack。
                0x9 => {
                    let _ = write_raw_ws_frame(&mut stream, 0xA, &payload).await;
                }
                0x2 => {}
                0x8 => break,
                _ => {}
            }
        }
    }

    async fn read_raw_ws_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).await.ok()?;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut len = u64::from(header[1] & 0x7f);
        if len == 126 {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended).await.ok()?;
            len = u64::from(u16::from_be_bytes(extended));
        } else if len == 127 {
            let mut extended = [0_u8; 8];
            stream.read_exact(&mut extended).await.ok()?;
            len = u64::from_be_bytes(extended);
        }
        if len > 1024 * 1024 {
            return None;
        }
        let mut mask = [0_u8; 4];
        if masked {
            stream.read_exact(&mut mask).await.ok()?;
        }
        let mut payload = vec![0_u8; len as usize];
        stream.read_exact(&mut payload).await.ok()?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % mask.len()];
            }
        }
        Some((opcode, payload))
    }

    async fn write_raw_ws_frame(
        stream: &mut TcpStream,
        opcode: u8,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(0x80 | (opcode & 0x0f));
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        stream.write_all(&frame).await
    }

    async fn read_axum_route_hello(
        socket: &mut axum::extract::ws::WebSocket,
    ) -> Option<RouteHelloPayload> {
        loop {
            let message = socket.next().await?.ok()?;
            match message {
                AxumMessage::Text(raw) => {
                    let envelope: JsonEnvelope = serde_json::from_str(raw.as_str()).ok()?;
                    assert_eq!(envelope.kind, MessageType::RouteHello);
                    return decode_payload(envelope.payload).ok();
                }
                AxumMessage::Binary(raw) => {
                    let envelope: JsonEnvelope = serde_json::from_slice(&raw).ok()?;
                    assert_eq!(envelope.kind, MessageType::RouteHello);
                    return decode_payload(envelope.payload).ok();
                }
                AxumMessage::Ping(payload) => {
                    let _ = socket.send(AxumMessage::Pong(payload)).await;
                }
                AxumMessage::Pong(_) => {}
                AxumMessage::Close(_) => return None,
            }
        }
    }

    #[test]
    fn decodes_text_and_binary_mux_frames_as_json_envelopes() {
        let envelope = Envelope::new(
            MessageType::Ping,
            serde_json::to_value(PingPayload {
                nonce: termd_proto::Nonce("n".to_owned()),
                timestamp_ms: termd_proto::UnixTimestampMillis(1),
            })
            .unwrap(),
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        let decoded_text =
            json_envelope_from_mux_frame(RelayOpaqueFrame::Text { data: raw.clone() }).unwrap();
        let decoded_binary = json_envelope_from_mux_frame(RelayOpaqueFrame::Binary {
            data_base64: general_purpose::STANDARD.encode(raw.as_bytes()),
        })
        .unwrap();

        assert_eq!(decoded_text.kind, MessageType::Ping);
        assert_eq!(decoded_binary.kind, MessageType::Ping);
    }

    #[test]
    fn mux_binary_client_frame_stays_binary_until_protocol_connection() {
        let raw = b"TD2E encrypted packet".to_vec();
        let message = wire_message_from_mux_frame(RelayOpaqueFrame::Binary {
            data_base64: general_purpose::STANDARD.encode(&raw),
        })
        .unwrap();

        assert_eq!(message, ProtocolWireMessage::Binary(raw));
    }

    #[tokio::test]
    async fn mux_keepalive_is_ignored_by_daemon_connector() {
        let protocol = test_protocol("mux-keepalive-ignore");
        let mut connections = HashMap::new();
        let active_clients = test_active_clients();

        let keepalive = handle_mux_envelope(
            RelayMuxEnvelope::Keepalive { nonce: 42 },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();
        let keepalive_ack = handle_mux_envelope(
            RelayMuxEnvelope::KeepaliveAck { nonce: 42 },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();

        assert!(keepalive.is_empty());
        assert!(keepalive_ack.is_empty());
    }

    #[tokio::test]
    async fn mux_client_connection_can_complete_pairing_on_independent_protocol_connection() {
        let protocol = test_protocol("mux-pairing");
        let client_id = RelayClientId(42);
        let mut connections = HashMap::new();
        let active_clients = test_active_clients();

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected { client_id },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();
        let hello = daemon_frame_to_json(initial[0].clone());
        let key_exchange = daemon_frame_to_json(initial[1].clone());
        assert_eq!(hello.kind, MessageType::Hello);
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);

        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .await
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let device_id = termd_proto::DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&server_key_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            server_key_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();

        let device_key_exchange = envelope_value(
            MessageType::E2eeKeyExchange,
            termd_proto::E2eeKeyExchangePayload::new(
                server_key_exchange.server_id,
                device_id,
                device_keypair.public_key_wire(),
                termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                current_unix_timestamp_millis(),
            ),
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();
        assert!(handshake_responses.is_empty());

        let pair_request = envelope_value(
            MessageType::PairRequest,
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key: termd_proto::PublicKey("ed25519-v1:test-device".to_owned()),
                token: termd_proto::PairingToken(token),
                nonce: termd_proto::Nonce("relay-pair-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let encrypted = envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&pair_request).unwrap(),
        )
        .unwrap();
        let pair_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(encrypted),
            },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();

        let outer = daemon_frame_to_json(pair_responses[0].clone());
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        let inner: JsonEnvelope = device_e2ee.decrypt_json_payload(&frame).unwrap();
        let accepted: termd_proto::PairAcceptPayload = decode_payload(inner.payload).unwrap();

        assert_eq!(inner.kind, MessageType::PairAccept);
        assert_eq!(accepted.device_id, device_id);
        assert_eq!(accepted.server_id, server_key_exchange.server_id);
    }

    #[tokio::test]
    async fn invalid_mux_client_frame_closes_only_that_client_connection() {
        let protocol = test_protocol("invalid-mux-frame");
        let bad_client_id = RelayClientId(1);
        let good_client_id = RelayClientId(2);
        let mut connections = HashMap::new();
        let active_clients = test_active_clients();

        handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: bad_client_id,
            },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();
        let bad_result = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id: bad_client_id,
                frame: RelayOpaqueFrame::Text {
                    data: "not-json".to_owned(),
                },
            },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await;

        assert!(bad_result.unwrap().is_empty());
        assert!(!connections.contains_key(&bad_client_id));

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: good_client_id,
            },
            &protocol,
            &mut connections,
            &active_clients,
        )
        .await
        .unwrap();
        assert_eq!(
            daemon_frame_to_json(initial[0].clone()).kind,
            MessageType::Hello
        );

        let key_exchange = daemon_frame_to_json(initial[1].clone());
        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .await
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let pair_response = complete_pairing_via_mux(
            &protocol,
            &mut connections,
            good_client_id,
            server_key_exchange,
            token,
            &active_clients,
        )
        .await;

        assert_eq!(pair_response.kind, MessageType::PairAccept);
        assert!(connections.contains_key(&good_client_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_pushes_session_output_without_client_pull_frame() {
        let protocol = test_protocol("mux-output-push");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_once(
            base,
            None,
            None,
            protocol.clone(),
            RELAY_HEARTBEAT_INTERVAL,
        ));
        let client_id = RelayClientId(77);
        let expected_server_id = protocol.lock().await.server_id();
        let mut relay_socket = accept_relay_mux_pair(&listener, expected_server_id).await;

        send_mux_to_connector(
            &mut relay_socket,
            RelayMuxEnvelope::ClientConnected { client_id },
        )
        .await;
        let hello = read_daemon_frame_from_connector(&mut relay_socket, client_id).await;
        assert_eq!(hello.kind, MessageType::Hello);
        let key_exchange = read_daemon_frame_from_connector(&mut relay_socket, client_id).await;
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);

        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .await
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let device_id = termd_proto::DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&server_key_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            server_key_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();

        send_mux_client_json(
            &mut relay_socket,
            client_id,
            envelope_value(
                MessageType::E2eeKeyExchange,
                termd_proto::E2eeKeyExchangePayload::new(
                    server_key_exchange.server_id,
                    device_id,
                    device_keypair.public_key_wire(),
                    termd_proto::Nonce("relay-push-e2ee-nonce".to_owned()),
                    current_unix_timestamp_millis(),
                ),
            )
            .unwrap(),
        )
        .await;
        send_encrypted_mux_client_json(
            &mut relay_socket,
            client_id,
            &mut device_e2ee,
            envelope_value(
                MessageType::PairRequest,
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: termd_proto::PublicKey(
                        "ed25519-v1:relay-push-test-device".to_owned(),
                    ),
                    token: termd_proto::PairingToken(token),
                    nonce: termd_proto::Nonce("relay-push-pair-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        )
        .await;
        let pair_accept =
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee).await;
        assert_eq!(pair_accept.kind, MessageType::PairAccept);

        send_encrypted_mux_client_json(
            &mut relay_socket,
            client_id,
            &mut device_e2ee,
            envelope_value(
                MessageType::SessionCreate,
                termd_proto::SessionCreatePayload {
                    command: vec![
                        "sh".to_owned(),
                        "-lc".to_owned(),
                        "sleep 0.15; printf relay-pushed-output".to_owned(),
                    ],
                    size: termd_proto::TerminalSize::default(),
                },
            )
            .unwrap(),
        )
        .await;
        let created =
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee).await;
        assert_eq!(created.kind, MessageType::SessionCreated);
        let created_payload: termd_proto::SessionCreatedPayload =
            decode_payload(created.payload).unwrap();

        // relay client 不再发送 ping 或任何业务帧；daemon mux 必须像直连 WebSocket 一样主动推送。
        let mut pushed_output = Vec::new();
        let push_deadline = Instant::now() + Duration::from_secs(2);
        while !pushed_output
            .windows(b"relay-pushed-output".len())
            .any(|window| window == b"relay-pushed-output")
        {
            let remaining = push_deadline.saturating_duration_since(Instant::now());
            let pushed = tokio::time::timeout(
                remaining,
                read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee),
            )
            .await
            .expect("relay mux should push PTY output without client pull frames");
            if pushed.kind != MessageType::SessionData {
                continue;
            }
            let payload: termd_proto::SessionDataPayload = decode_payload(pushed.payload).unwrap();
            assert_eq!(payload.session_id, created_payload.session_id);
            pushed_output.extend(
                general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap(),
            );
        }

        connector.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_processes_new_client_while_output_writer_is_backpressured() {
        let protocol = test_protocol("mux-backpressure-new-client");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_once(
            base,
            None,
            None,
            protocol.clone(),
            RELAY_HEARTBEAT_INTERVAL,
        ));
        let slow_client_id = RelayClientId(701);
        let fresh_client_id = RelayClientId(702);
        let expected_server_id = protocol.lock().await.server_id();
        let mut relay_socket = accept_relay_mux_pair(&listener, expected_server_id).await;

        send_mux_to_connector(
            &mut relay_socket,
            RelayMuxEnvelope::ClientConnected {
                client_id: slow_client_id,
            },
        )
        .await;
        let slow_hello = read_daemon_frame_from_connector(&mut relay_socket, slow_client_id).await;
        assert_eq!(slow_hello.kind, MessageType::Hello);
        let slow_key_exchange =
            read_daemon_frame_from_connector(&mut relay_socket, slow_client_id).await;
        assert_eq!(slow_key_exchange.kind, MessageType::E2eeKeyExchange);
        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(slow_key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .await
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let (slow_device_id, mut slow_e2ee) = open_mux_e2ee(
            &mut relay_socket,
            slow_client_id,
            server_key_exchange,
            "relay-backpressure-e2ee-nonce",
        )
        .await;
        send_encrypted_mux_client_json(
            &mut relay_socket,
            slow_client_id,
            &mut slow_e2ee,
            envelope_value(
                MessageType::PairRequest,
                termd_proto::PairRequestPayload {
                    device_id: slow_device_id,
                    device_public_key: termd_proto::PublicKey(
                        "ed25519-v1:relay-backpressure-device".to_owned(),
                    ),
                    token: termd_proto::PairingToken(token),
                    nonce: termd_proto::Nonce("relay-backpressure-pair-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        )
        .await;
        let pair_accept =
            read_encrypted_daemon_frame(&mut relay_socket, slow_client_id, &mut slow_e2ee).await;
        assert_eq!(pair_accept.kind, MessageType::PairAccept);

        send_encrypted_mux_client_json(
            &mut relay_socket,
            slow_client_id,
            &mut slow_e2ee,
            envelope_value(
                MessageType::SessionCreate,
                termd_proto::SessionCreatePayload {
                    command: vec![
                        "sh".to_owned(),
                        "-lc".to_owned(),
                        "for i in $(seq 1 4096); do printf 'relay-backpressure-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' \"$i\"; done".to_owned(),
                    ],
                    size: termd_proto::TerminalSize::default(),
                },
            )
            .unwrap(),
        )
        .await;
        let created =
            read_encrypted_daemon_frame(&mut relay_socket, slow_client_id, &mut slow_e2ee).await;
        assert_eq!(created.kind, MessageType::SessionCreated);

        // 先读到第一批 output，证明 slow client 已经触发大量推送；随后故意不再
        // 消费它的剩余 output，模拟公网 relay/web 侧慢写。
        let first_output =
            read_encrypted_daemon_frame(&mut relay_socket, slow_client_id, &mut slow_e2ee).await;
        assert_eq!(first_output.kind, MessageType::SessionData);

        send_mux_to_connector(
            &mut relay_socket,
            RelayMuxEnvelope::ClientConnected {
                client_id: fresh_client_id,
            },
        )
        .await;

        let fresh_hello = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let RelayMuxEnvelope::DaemonFrame { client_id, frame } =
                    read_mux_from_connector(&mut relay_socket).await
                else {
                    continue;
                };
                if client_id != fresh_client_id {
                    continue;
                }
                let envelope = json_envelope_from_mux_frame(frame).unwrap();
                if envelope.kind == MessageType::Hello {
                    break envelope;
                }
            }
        })
        .await
        .expect("新 client 的 hello 不能被旧 client 的 terminal output 卡住");
        assert_eq!(fresh_hello.kind, MessageType::Hello);
        let fresh_key_exchange =
            read_daemon_frame_for_connector(&mut relay_socket, fresh_client_id).await;
        assert_eq!(fresh_key_exchange.kind, MessageType::E2eeKeyExchange);
        let fresh_server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(fresh_key_exchange.payload).unwrap();
        let mut fresh_e2ee = open_mux_e2ee_for_device(
            &mut relay_socket,
            fresh_client_id,
            slow_device_id,
            fresh_server_key_exchange,
            "relay-backpressure-fresh-e2ee-nonce",
        )
        .await;
        let auth_challenge = tokio::time::timeout(
            Duration::from_secs(1),
            read_encrypted_daemon_frame_for_connector(
                &mut relay_socket,
                fresh_client_id,
                &mut fresh_e2ee,
            ),
        )
        .await
        .expect("新 client 的 auth.challenge 不能被旧 client 的 terminal output 卡住");
        assert_eq!(auth_challenge.kind, MessageType::AuthChallenge);

        connector.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_pushes_cwd_updates_without_client_pull_frame() {
        let workspace_root = std::env::temp_dir().join(format!(
            "termd-relay-cwd-root-{}-{}",
            std::process::id(),
            TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let file_root = workspace_root.join("project");
        fs::create_dir_all(&file_root).unwrap();
        fs::write(file_root.join("alpha.txt"), b"alpha\n").unwrap();

        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("mux-cwd-push-state"));
        // cwd 变化推送测试不能读取共享 `/tmp`，否则会和并行测试清理临时文件产生竞态。
        config.default_working_directory = Some(workspace_root.clone());
        let protocol = default_protocol(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_once(
            base,
            None,
            None,
            protocol.clone(),
            RELAY_HEARTBEAT_INTERVAL,
        ));
        let client_id = RelayClientId(78);
        let expected_server_id = protocol.lock().await.server_id();
        let mut relay_socket = accept_relay_mux_pair(&listener, expected_server_id).await;

        send_mux_to_connector(
            &mut relay_socket,
            RelayMuxEnvelope::ClientConnected { client_id },
        )
        .await;
        let hello = read_daemon_frame_from_connector(&mut relay_socket, client_id).await;
        assert_eq!(hello.kind, MessageType::Hello);
        let key_exchange = read_daemon_frame_from_connector(&mut relay_socket, client_id).await;
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);

        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .await
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let (device_id, mut device_e2ee) = open_mux_e2ee(
            &mut relay_socket,
            client_id,
            server_key_exchange.clone(),
            "relay-cwd-push-e2ee-nonce",
        )
        .await;

        send_encrypted_mux_client_json(
            &mut relay_socket,
            client_id,
            &mut device_e2ee,
            envelope_value(
                MessageType::PairRequest,
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: termd_proto::PublicKey(
                        "ed25519-v1:relay-cwd-push-test-device".to_owned(),
                    ),
                    token: termd_proto::PairingToken(token),
                    nonce: termd_proto::Nonce("relay-cwd-push-pair-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        )
        .await;
        let pair_accept =
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee).await;
        assert_eq!(pair_accept.kind, MessageType::PairAccept);

        send_encrypted_mux_client_json(
            &mut relay_socket,
            client_id,
            &mut device_e2ee,
            envelope_value(
                MessageType::SessionCreate,
                termd_proto::SessionCreatePayload {
                    command: vec![
                        "sh".to_owned(),
                        "-lc".to_owned(),
                        format!(
                            "sleep 0.15; cd {}; printf cwd-moved; sleep 2",
                            file_root.to_string_lossy()
                        ),
                    ],
                    size: termd_proto::TerminalSize::default(),
                },
            )
            .unwrap(),
        )
        .await;
        let created =
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee).await;
        assert_eq!(created.kind, MessageType::SessionCreated);
        let created_payload: termd_proto::SessionCreatedPayload =
            decode_payload(created.payload).unwrap();

        send_encrypted_mux_client_json(
            &mut relay_socket,
            client_id,
            &mut device_e2ee,
            envelope_value(
                MessageType::SessionFiles,
                termd_proto::SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        )
        .await;
        let initial_files =
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee).await;
        assert_eq!(initial_files.kind, MessageType::SessionFilesResult);

        // 中文注释：新模型下 relay 只会收到 cwd 变化轻事件；真正文件树内容要由
        // client 收到事件后主动再拉一次 `session.files`。
        let pushed = tokio::time::timeout(
            Duration::from_secs(5),
            read_session_cwd_changed_with_path(
                &mut relay_socket,
                client_id,
                &mut device_e2ee,
                &file_root.to_string_lossy(),
            ),
        )
        .await
        .expect("relay mux should push cwd updates without client pull frames");
        assert_eq!(pushed.session_id, created_payload.session_id);

        fs::remove_dir_all(workspace_root).ok();

        connector.abort();
    }

    async fn complete_pairing_via_mux(
        protocol: &SharedDaemonProtocol,
        connections: &mut HashMap<RelayClientId, ProtocolConnection>,
        client_id: RelayClientId,
        server_key_exchange: termd_proto::E2eeKeyExchangePayload,
        token: String,
        active_clients: &RelayActiveClients,
    ) -> JsonEnvelope {
        let device_id = termd_proto::DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&server_key_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            server_key_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();
        let device_key_exchange = envelope_value(
            MessageType::E2eeKeyExchange,
            termd_proto::E2eeKeyExchangePayload::new(
                server_key_exchange.server_id,
                device_id,
                device_keypair.public_key_wire(),
                termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                current_unix_timestamp_millis(),
            ),
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            protocol,
            connections,
            active_clients,
        )
        .await
        .unwrap();
        assert!(handshake_responses.is_empty());

        let pair_request = envelope_value(
            MessageType::PairRequest,
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key: termd_proto::PublicKey("ed25519-v1:test-device".to_owned()),
                token: termd_proto::PairingToken(token),
                nonce: termd_proto::Nonce("relay-pair-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let encrypted = envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&pair_request).unwrap(),
        )
        .unwrap();
        let pair_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(encrypted),
            },
            protocol,
            connections,
            active_clients,
        )
        .await
        .unwrap();

        let outer = daemon_frame_to_json(pair_responses[0].clone());
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    fn json_to_mux_text(envelope: JsonEnvelope) -> RelayOpaqueFrame {
        RelayOpaqueFrame::Text {
            data: serde_json::to_string(&envelope).unwrap(),
        }
    }

    async fn send_mux_to_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        envelope: RelayMuxEnvelope,
    ) {
        let raw = serde_json::to_string(&envelope).unwrap();
        socket.send(Message::Text(raw.into())).await.unwrap();
    }

    async fn accept_relay_mux_pair(
        listener: &tokio::net::TcpListener,
        expected_server_id: ServerId,
    ) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
        let (control_tcp, _) = listener.accept().await.unwrap();
        let mut control_socket = tokio_tungstenite::accept_async(control_tcp).await.unwrap();
        complete_relay_route_prelude(
            &mut control_socket,
            expected_server_id,
            RouteRole::DaemonMux,
        )
        .await;
        control_socket
    }

    async fn complete_relay_route_prelude(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_server_id: ServerId,
        expected_role: RouteRole,
    ) {
        let route_hello = tokio::time::timeout(
            Duration::from_secs(1),
            read_route_hello_from_connector(socket),
        )
        .await
        .expect("connector should send route_hello before relay mux envelopes");
        assert_eq!(route_hello.server_id, expected_server_id);
        assert_eq!(route_hello.role, expected_role);
        assert_eq!(
            route_hello.protocol_version,
            ProtocolVersion(PROTOCOL_PACKET_VERSION)
        );

        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id: expected_server_id,
                role: expected_role,
            },
        );
        socket
            .send(Message::Text(
                serde_json::to_string(&route_ready).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    async fn send_route_ready_to_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        server_id: ServerId,
        role: RouteRole,
    ) {
        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload { server_id, role },
        );
        socket
            .send(Message::Text(
                serde_json::to_string(&route_ready).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    async fn read_json_envelope_from_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> JsonEnvelope {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            match message {
                Message::Text(raw) => {
                    if let Ok(envelope) = serde_json::from_str(raw.as_str()) {
                        return envelope;
                    }
                    continue;
                }
                Message::Binary(raw) => {
                    if let Ok(envelope) = serde_json::from_slice(&raw) {
                        return envelope;
                    }
                    continue;
                }
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await.unwrap();
                }
                Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(frame) => panic!("relay mux closed unexpectedly: {frame:?}"),
            }
        }
    }

    async fn read_route_hello_from_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> RouteHelloPayload {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            match message {
                Message::Text(raw) => {
                    let envelope: JsonEnvelope = serde_json::from_str(raw.as_str()).unwrap();
                    assert_eq!(envelope.kind, MessageType::RouteHello);
                    return decode_payload(envelope.payload).unwrap();
                }
                Message::Binary(raw) => {
                    let envelope: JsonEnvelope = serde_json::from_slice(&raw).unwrap();
                    assert_eq!(envelope.kind, MessageType::RouteHello);
                    return decode_payload(envelope.payload).unwrap();
                }
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await.unwrap();
                }
                Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(frame) => panic!("relay mux closed unexpectedly: {frame:?}"),
            }
        }
    }

    async fn send_mux_client_json(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        client_id: RelayClientId,
        envelope: JsonEnvelope,
    ) {
        send_mux_to_connector(
            socket,
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(envelope),
            },
        )
        .await;
    }

    async fn send_encrypted_mux_client_json(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
        inner: JsonEnvelope,
    ) {
        let encrypted = envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&inner).unwrap(),
        )
        .unwrap();
        send_mux_client_json(socket, client_id, encrypted).await;
    }

    async fn open_mux_e2ee(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        client_id: RelayClientId,
        daemon_exchange: termd_proto::E2eeKeyExchangePayload,
        nonce: &str,
    ) -> (termd_proto::DeviceId, E2eeSession) {
        let device_id = termd_proto::DeviceId::new();
        let device_e2ee =
            open_mux_e2ee_for_device(socket, client_id, device_id, daemon_exchange, nonce).await;
        (device_id, device_e2ee)
    }

    async fn open_mux_e2ee_for_device(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        client_id: RelayClientId,
        device_id: termd_proto::DeviceId,
        daemon_exchange: termd_proto::E2eeKeyExchangePayload,
        nonce: &str,
    ) -> E2eeSession {
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&daemon_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            daemon_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();

        send_mux_client_json(
            socket,
            client_id,
            envelope_value(
                MessageType::E2eeKeyExchange,
                termd_proto::E2eeKeyExchangePayload::new(
                    daemon_exchange.server_id,
                    device_id,
                    device_keypair.public_key_wire(),
                    termd_proto::Nonce(nonce.to_owned()),
                    current_unix_timestamp_millis(),
                ),
            )
            .unwrap(),
        )
        .await;

        device_e2ee
    }

    async fn read_mux_from_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> RelayMuxEnvelope {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            match message {
                Message::Text(raw) => return serde_json::from_str(raw.as_str()).unwrap(),
                Message::Binary(raw) => return decode_binary_relay_mux_envelope(&raw).unwrap(),
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await.unwrap();
                }
                Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(frame) => panic!("relay mux closed unexpectedly: {frame:?}"),
            }
        }
    }

    async fn read_daemon_frame_from_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
    ) -> JsonEnvelope {
        let RelayMuxEnvelope::DaemonFrame { client_id, frame } =
            read_mux_from_connector(socket).await
        else {
            panic!("expected daemon_frame");
        };
        assert_eq!(client_id, expected_client_id);
        json_envelope_from_mux_frame(frame).unwrap()
    }

    async fn read_daemon_frame_for_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
    ) -> JsonEnvelope {
        loop {
            let RelayMuxEnvelope::DaemonFrame { client_id, frame } =
                read_mux_from_connector(socket).await
            else {
                continue;
            };
            if client_id != expected_client_id {
                // 中文注释：backpressure 测试允许旧 client 的 output 和新 client 握手帧交错；
                // 只要新 client 能在时限内出现，就证明 relay 没被旧输出卡死。
                continue;
            }
            return json_envelope_from_mux_frame(frame).unwrap();
        }
    }

    async fn read_encrypted_daemon_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
    ) -> JsonEnvelope {
        let outer = read_daemon_frame_from_connector(socket, expected_client_id).await;
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    async fn read_encrypted_daemon_frame_for_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
    ) -> JsonEnvelope {
        let outer = read_daemon_frame_for_connector(socket, expected_client_id).await;
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    async fn read_session_cwd_changed_with_path(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
        expected_path: &str,
    ) -> termd_proto::SessionCwdChangedPayload {
        loop {
            let inner = read_encrypted_daemon_frame(socket, expected_client_id, device_e2ee).await;
            if inner.kind != MessageType::SessionCwdChanged {
                continue;
            }
            let payload: termd_proto::SessionCwdChangedPayload =
                decode_payload(inner.payload).unwrap();
            if payload.cwd == expected_path {
                return payload;
            }
        }
    }

    fn daemon_frame_to_json(envelope: RelayMuxEnvelope) -> JsonEnvelope {
        let RelayMuxEnvelope::DaemonFrame { frame, .. } = envelope else {
            panic!("expected daemon_frame");
        };
        json_envelope_from_mux_frame(frame).unwrap()
    }
}
