use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, ErrorPayload, MessageType, RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tracing::{debug, warn};

const DATA_CHANNEL_CAPACITY: usize = 1024;
// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(2);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const WEBSOCKET_MAX_FRAME_SIZE: usize = 1024 * 1024;
const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

type ConnectionId = u64;

#[derive(Debug, Clone)]
struct FrameSender {
    control: mpsc::UnboundedSender<RelayOutbound>,
    data: mpsc::Sender<RelayOutbound>,
}

impl FrameSender {
    fn channel(
        data_capacity: usize,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<RelayOutbound>,
        mpsc::Receiver<RelayOutbound>,
    ) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (data_tx, data_rx) = mpsc::channel(data_capacity);
        (
            Self {
                control: control_tx,
                data: data_tx,
            },
            control_rx,
            data_rx,
        )
    }

    fn try_send(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), mpsc::error::TrySendError<RelayOutbound>> {
        self.data.try_send(outbound)
    }

    fn try_send_control(
        &self,
        outbound: RelayOutbound,
    ) -> Result<(), mpsc::error::SendError<RelayOutbound>> {
        // 生命周期控制消息不能被普通业务队列挤掉；否则 daemon 会继续保留 stale client/watchers。
        self.control.send(outbound)
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
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedRelayOutbound {
    Frame(OpaqueFrame),
    Close,
    Drop,
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
    // daemon mux 是所有 relay client 共享的主干连接；主动 Ping/Pong 和大输出、断连控制帧共用
    // 同一条 WebSocket/TCP 流，10s deadline 会在高输出或批量断连时误杀主干，反而造成全局卡顿。
    // 浏览器 client 仍用 heartbeat 快速清理断线；daemon mux 交给 idle timeout 和写失败检测。
    role == ConnectionRole::Client
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

    fn register(
        &self,
        server_id: ServerId,
        role: ConnectionRole,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        self.inner.register(server_id, role, sender)
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
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
}

impl RelayRoom {
    fn close_clients(&mut self) {
        for (_, client) in self.clients.drain() {
            // daemon mux 已不可用时，client 必须尽快收到 close，避免继续等待业务响应直到超时。
            let _ = client.sender.try_send_control(RelayOutbound::Close);
        }
    }
}

#[derive(Debug, Clone)]
struct ConnectionEndpoint {
    id: ConnectionId,
    sender: FrameSender,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConnectionRegistration {
    server_id: ServerId,
    role: ConnectionRole,
    id: ConnectionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardReport {
    pub attempted: usize,
    pub delivered: usize,
    pub dropped: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
enum RelayError {
    #[error("daemon mux is not connected for server_id")]
    DaemonMuxOffline,
    #[error("relay state mutex poisoned")]
    Poisoned,
}

impl RelayError {
    fn route_error_code(&self) -> &'static str {
        match self {
            Self::DaemonMuxOffline => "relay_daemon_offline",
            Self::Poisoned => "relay_state_unavailable",
        }
    }

    fn route_error_message(&self) -> &'static str {
        match self {
            Self::DaemonMuxOffline => {
                "relay daemon mux is not connected; retry after daemon reconnects"
            }
            Self::Poisoned => "relay state is temporarily unavailable",
        }
    }
}

#[derive(Debug, Error)]
enum RelayMuxFrameError {
    #[error("relay mux frame binary payload is not valid base64")]
    InvalidBase64(#[source] base64::DecodeError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoutePrelude {
    server_id: ServerId,
    route_role: RouteRole,
    connection_role: ConnectionRole,
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
        server_id: ServerId,
        role: ConnectionRole,
        sender: FrameSender,
    ) -> Result<ConnectionRegistration, RelayError> {
        let id = self.next_connection_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut rooms = self.rooms.lock().map_err(|_| RelayError::Poisoned)?;

        match role {
            ConnectionRole::DaemonMux => {
                let room = rooms.entry(server_id).or_default();
                if let Some(stale_mux) = room.daemon_mux.replace(ConnectionEndpoint { id, sender })
                {
                    warn!(
                        server_id = %server_id.0,
                        stale_connection_id = stale_mux.id,
                        new_connection_id = id,
                        "replacing stale relay daemon mux"
                    );
                    // 新 mux 已经到达时，旧 mux 多半是半断连接；关闭旧 mux 和旧 clients，
                    // 让浏览器按统一重连路径重新完成 E2EE 握手，避免复用旧 client_id。
                    let _ = stale_mux.sender.try_send_control(RelayOutbound::Close);
                    room.close_clients();
                }
            }
            ConnectionRole::Client => {
                let room = rooms
                    .get_mut(&server_id)
                    .ok_or(RelayError::DaemonMuxOffline)?;
                if room.daemon_mux.is_none() {
                    return Err(RelayError::DaemonMuxOffline);
                }
                room.clients.insert(id, ConnectionEndpoint { id, sender });
            }
        }

        Ok(ConnectionRegistration {
            server_id,
            role,
            id,
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
                    room.daemon_mux = None;
                    room.close_clients();
                }
            }
            ConnectionRole::Client => {
                room.clients.remove(&registration.id);
                if let Some(daemon_mux) = room.daemon_mux.as_ref() {
                    notify_daemon_mux_client_disconnected(daemon_mux, registration.id);
                }
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
            ConnectionRole::DaemonMux => self.forward_mux_to_client(registration.server_id, frame),
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

        let Some(daemon_mux) = room.daemon_mux.as_ref() else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        match daemon_mux.sender.try_send(RelayOutbound::MuxClientFrame {
            client_id: RelayClientId(registration.id),
            frame,
        }) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(error) => {
                warn!(
                    server_id = %registration.server_id.0,
                    connection_id = daemon_mux.id,
                    %error,
                    "dropping offline relay daemon mux"
                );
                room.daemon_mux = None;
                room.close_clients();
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn forward_mux_to_client(&self, server_id: ServerId, frame: OpaqueFrame) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon mux forward");
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        let Some(room) = rooms.get_mut(&server_id) else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };
        let OpaqueFrame::Text(raw) = frame else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 1,
            };
        };
        let envelope = match serde_json::from_str::<RelayMuxEnvelope>(&raw) {
            Ok(envelope) => envelope,
            Err(error) => {
                warn!(server_id = %server_id.0, %error, "rejecting invalid relay mux envelope");
                return ForwardReport {
                    attempted: 0,
                    delivered: 0,
                    dropped: 1,
                };
            }
        };
        let RelayMuxEnvelope::DaemonFrame { client_id, frame } = envelope else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 1,
            };
        };
        let Some(client) = room.clients.get(&client_id.0) else {
            return ForwardReport {
                attempted: 1,
                delivered: 0,
                dropped: 1,
            };
        };
        let target_client_id = client.id;
        let target_sender = client.sender.clone();
        let frame = match opaque_frame_from_mux(frame) {
            Ok(frame) => frame,
            Err(error) => {
                warn!(server_id = %server_id.0, %error, "rejecting invalid relay mux frame");
                return ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                };
            }
        };

        match target_sender.try_send(RelayOutbound::Frame(frame)) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(error) => {
                warn!(
                    server_id = %server_id.0,
                    connection_id = target_client_id,
                    %error,
                    "dropping slow relay mux client"
                );
                room.clients.remove(&client_id.0);
                if let Some(daemon_mux) = room.daemon_mux.as_ref() {
                    notify_daemon_mux_client_disconnected(daemon_mux, client_id.0);
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

fn notify_daemon_mux_client_disconnected(daemon_mux: &ConnectionEndpoint, client_id: ConnectionId) {
    let envelope = RelayMuxEnvelope::ClientDisconnected {
        client_id: RelayClientId(client_id),
    };
    let _ = daemon_mux
        .sender
        .try_send_control(RelayOutbound::Frame(mux_envelope_frame(envelope)));
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
    let (tx, mut control_rx, mut data_rx) = FrameSender::channel(DATA_CHANNEL_CAPACITY);
    let registration = match state.register(server_id, role, tx) {
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

    if role == ConnectionRole::Client {
        notify_mux_client_connected(&state, &registration);
    }

    debug!(
        server_id = %server_id.0,
        ?role,
        connection_id = registration.id,
        "relay websocket registered"
    );

    let (mut sender, mut receiver) = socket.split();
    let mut idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
    let mut heartbeat = tokio::time::interval_at(
        Instant::now() + WEBSOCKET_HEARTBEAT_INTERVAL,
        WEBSOCKET_HEARTBEAT_INTERVAL,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let heartbeat_enabled = websocket_heartbeat_enabled(role);
    let mut pending_pong_deadline: Option<Instant> = None;
    let mut heartbeat_debug = WebSocketHeartbeatDebug::new(Instant::now());

    loop {
        let pending_pong_deadline_snapshot = pending_pong_deadline;
        // control frame 优先级高于业务转发，避免慢 client 或大输出把 Pong 消费延迟到超时之后。
        tokio::select! {
            biased;

            _ = tokio::time::sleep_until(idle_deadline) => {
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
            control = control_rx.recv() => {
                let Some(outbound) = control else {
                    break;
                };
                if !send_relay_outbound(
                    &state,
                    &mut sender,
                    server_id,
                    role,
                    registration.id,
                    outbound,
                    &mut heartbeat_debug,
                    &mut idle_deadline,
                )
                .await
                {
                    break;
                }
            }
            inbound = receiver.next() => {
                let Some(inbound) = inbound else {
                    break;
                };
                let inbound = match inbound {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(
                            server_id = %server_id.0,
                            ?role,
                            connection_id = registration.id,
                            %error,
                            heartbeat_debug = ?heartbeat_debug.pending_snapshot(),
                            "relay websocket receive failed"
                        );
                        break;
                    }
                };
                idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
                heartbeat_debug.record_inbound(
                    websocket_message_kind(&inbound),
                    websocket_message_bytes(&inbound),
                );
                let is_pong = matches!(inbound, Message::Pong(_));

                if !handle_inbound_message(
                    &state,
                    &registration,
                    &mut sender,
                    &mut heartbeat_debug,
                    inbound,
                )
                .await
                {
                    break;
                }
                if is_pong {
                    pending_pong_deadline = None;
                    heartbeat_debug.note_pong_received();
                }
            }
            _ = heartbeat.tick(), if heartbeat_enabled => {
                if pending_pong_deadline.is_none() {
                    if send_message_with_deadline(
                        &mut sender,
                        Message::Ping(Vec::new()),
                        WEBSOCKET_SEND_DEADLINE,
                        "relay websocket ping",
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    heartbeat_debug.note_ping_sent();
                    pending_pong_deadline = Some(Instant::now() + WEBSOCKET_PONG_DEADLINE);
                }
            }
            outbound = data_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };

                if !send_relay_outbound(
                    &state,
                    &mut sender,
                    server_id,
                    role,
                    registration.id,
                    outbound,
                    &mut heartbeat_debug,
                    &mut idle_deadline,
                )
                .await
                {
                    break;
                }
            }
        }
    }

    state.unregister(&registration);
    debug!(
        server_id = %server_id.0,
        ?role,
        connection_id = registration.id,
        "relay websocket unregistered"
    );
}

async fn send_relay_outbound(
    state: &RelayState,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    outbound: RelayOutbound,
    heartbeat_debug: &mut WebSocketHeartbeatDebug,
    idle_deadline: &mut Instant,
) -> bool {
    match prepare_relay_outbound(state, server_id, role, connection_id, outbound) {
        PreparedRelayOutbound::Frame(frame) => {
            send_relay_opaque_frame(
                sender,
                server_id,
                role,
                connection_id,
                frame,
                heartbeat_debug,
                idle_deadline,
            )
            .await
        }
        PreparedRelayOutbound::Close => {
            let _ = send_message_with_deadline(
                sender,
                Message::Close(None),
                WEBSOCKET_SEND_DEADLINE,
                "relay websocket close",
            )
            .await;
            false
        }
        PreparedRelayOutbound::Drop => true,
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
        RelayOutbound::Close => PreparedRelayOutbound::Close,
    }
}

async fn send_relay_opaque_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    server_id: ServerId,
    role: ConnectionRole,
    connection_id: ConnectionId,
    frame: OpaqueFrame,
    heartbeat_debug: &mut WebSocketHeartbeatDebug,
    idle_deadline: &mut Instant,
) -> bool {
    let frame_kind = frame.kind();
    let frame_len = frame.len();
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
            "relay websocket send failed"
        );
        return false;
    }
    heartbeat_debug.record_outbound(frame_kind, frame_len);
    *idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
    true
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
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    heartbeat_debug: &mut WebSocketHeartbeatDebug,
    message: Message,
) -> bool {
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
                return false;
            }
            forward_opaque(state, registration, OpaqueFrame::Text(text));
            true
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
                return false;
            }
            forward_opaque(state, registration, OpaqueFrame::Binary(bytes));
            true
        }
        Message::Ping(payload) => {
            let pong_bytes = payload.len();
            send_message_with_deadline(
                sender,
                Message::Pong(payload),
                WEBSOCKET_PONG_DEADLINE,
                "relay websocket pong",
            )
            .await
            .map(|_| {
                heartbeat_debug.record_outbound("pong", pong_bytes);
            })
            .is_ok()
        }
        Message::Pong(_) => true,
        Message::Close(_) => false,
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

fn forward_opaque(state: &RelayState, registration: &ConnectionRegistration, frame: OpaqueFrame) {
    let frame_kind = frame.kind();
    let frame_len = frame.len();
    let report = state.forward_from(registration, frame);

    debug!(
        server_id = %registration.server_id.0,
        ?registration.role,
        connection_id = registration.id,
        frame_kind,
        frame_len,
        attempted = report.attempted,
        delivered = report.delivered,
        dropped = report.dropped,
        "relay forwarded opaque frame"
    );
}

fn notify_mux_client_connected(state: &RelayState, registration: &ConnectionRegistration) {
    let Ok(rooms) = state.inner.rooms.lock() else {
        warn!("relay registry mutex poisoned during mux connect notify");
        return;
    };
    let Some(room) = rooms.get(&registration.server_id) else {
        return;
    };
    let Some(daemon_mux) = room.daemon_mux.as_ref() else {
        return;
    };
    let envelope = RelayMuxEnvelope::ClientConnected {
        client_id: RelayClientId(registration.id),
    };
    let _ = daemon_mux
        .sender
        .try_send_control(RelayOutbound::Frame(mux_envelope_frame(envelope)));
}

fn mux_envelope_frame(envelope: RelayMuxEnvelope) -> OpaqueFrame {
    let raw = serde_json::to_string(&envelope).expect("relay mux envelope should serialize");
    OpaqueFrame::Text(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::router;
    use termd_proto::{Nonce, PROTOCOL_PACKET_VERSION, ProtocolVersion, UnixTimestampMillis};
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    fn server_id(value: u128) -> ServerId {
        ServerId(uuid::Uuid::from_u128(value))
    }

    struct TestReceiver {
        control: mpsc::UnboundedReceiver<RelayOutbound>,
        data: mpsc::Receiver<RelayOutbound>,
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

    fn channel_with_data_capacity(data_capacity: usize) -> (FrameSender, TestReceiver) {
        let (sender, control, data) = FrameSender::channel(data_capacity);
        (sender, TestReceiver { control, data })
    }

    fn client_route_hello(server_id: ServerId) -> Envelope<RouteHelloPayload> {
        Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role: RouteRole::Client,
                protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                nonce: Nonce("test-route".to_owned()),
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        )
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
    fn websocket_heartbeat_is_not_active_for_daemon_mux_trunk() {
        assert!(!websocket_heartbeat_enabled(ConnectionRole::DaemonMux));
        assert!(websocket_heartbeat_enabled(ConnectionRole::Client));
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
        let next = timeout(Duration::from_secs(4), socket.next())
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
        notify_mux_client_connected(&state, &client);

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
    fn client_disconnect_bypasses_full_daemon_mux_data_queue() {
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
        let raw = match outbound {
            RelayOutbound::Frame(OpaqueFrame::Text(raw)) => raw,
            RelayOutbound::MuxClientFrame { client_id, frame } => {
                let envelope = RelayMuxEnvelope::ClientFrame {
                    client_id,
                    frame: frame.into(),
                };
                serde_json::to_string(&envelope).expect("mux envelope should encode")
            }
            other => {
                panic!("expected mux text envelope, got {other:?}");
            }
        };
        serde_json::from_str(&raw).expect("mux envelope should decode")
    }
}
