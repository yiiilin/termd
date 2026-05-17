use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, MessageType, RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame, RouteHelloPayload,
    RouteReadyPayload, RouteRole, ServerId,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tracing::{debug, warn};

const CHANNEL_CAPACITY: usize = 1024;
// relay 只关闭当前 WebSocket transport；不会解释或终止 E2EE 内部的 daemon session。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(2);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(2);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(2);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const WEBSOCKET_MAX_FRAME_SIZE: usize = 1024 * 1024;
const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

type ConnectionId = u64;
type FrameSender = mpsc::Sender<RelayOutbound>;

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
    Close,
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
            let _ = client.sender.try_send(RelayOutbound::Close);
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
    #[error("daemon mux already connected for server_id")]
    DuplicateDaemonMux,
    #[error("daemon mux is not connected for server_id")]
    DaemonMuxOffline,
    #[error("relay state mutex poisoned")]
    Poisoned,
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
                if room.daemon_mux.is_some() {
                    return Err(RelayError::DuplicateDaemonMux);
                }
                room.daemon_mux = Some(ConnectionEndpoint { id, sender });
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

        let envelope = RelayMuxEnvelope::ClientFrame {
            client_id: RelayClientId(registration.id),
            frame: frame.into(),
        };
        match daemon_mux
            .sender
            .try_send(RelayOutbound::Frame(mux_envelope_frame(envelope)))
        {
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
        .try_send(RelayOutbound::Frame(mux_envelope_frame(envelope)));
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
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    let registration = match state.register(server_id, role, tx) {
        Ok(registration) => registration,
        Err(error) => {
            warn!(server_id = %server_id.0, ?role, %error, "rejecting relay websocket");
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

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(idle_deadline) => {
                warn!(
                    server_id = %server_id.0,
                    ?role,
                    connection_id = registration.id,
                    "relay websocket idle timeout"
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
                        warn!(server_id = %server_id.0, ?role, connection_id = registration.id, %error, "relay websocket receive failed");
                        break;
                    }
                };
                idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;

                if !handle_inbound_message(&state, &registration, &mut sender, inbound).await {
                    break;
                }
            }
            outbound = rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };

                match outbound {
                    RelayOutbound::Frame(frame) => {
                        let frame_kind = frame.kind();
                        let frame_len = frame.len();
                        if send_message_with_deadline(
                            &mut sender,
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
                                connection_id = registration.id,
                                frame_kind,
                                frame_len,
                                "relay websocket send failed"
                            );
                            break;
                        }
                        idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
                    }
                    RelayOutbound::Close => {
                        let _ = send_message_with_deadline(
                            &mut sender,
                            Message::Close(None),
                            WEBSOCKET_SEND_DEADLINE,
                            "relay websocket close",
                        )
                        .await;
                        break;
                    }
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

async fn handle_inbound_message(
    state: &RelayState,
    registration: &ConnectionRegistration,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
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
        Message::Ping(payload) => send_message_with_deadline(
            sender,
            Message::Pong(payload),
            WEBSOCKET_PONG_DEADLINE,
            "relay websocket pong",
        )
        .await
        .is_ok(),
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
        .try_send(RelayOutbound::Frame(mux_envelope_frame(envelope)));
}

fn mux_envelope_frame(envelope: RelayMuxEnvelope) -> OpaqueFrame {
    let raw = serde_json::to_string(&envelope).expect("relay mux envelope should serialize");
    OpaqueFrame::Text(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::router;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    fn server_id(value: u128) -> ServerId {
        ServerId(uuid::Uuid::from_u128(value))
    }

    fn channel() -> (FrameSender, mpsc::Receiver<RelayOutbound>) {
        mpsc::channel(CHANNEL_CAPACITY)
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
    fn room_rejects_duplicate_daemon_mux() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (first_tx, _first_rx) = channel();
        let (second_tx, _second_rx) = channel();

        state
            .register(server_id, ConnectionRole::DaemonMux, first_tx)
            .unwrap();
        let error = state
            .register(server_id, ConnectionRole::DaemonMux, second_tx)
            .unwrap_err();

        assert_eq!(error, RelayError::DuplicateDaemonMux);
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
        let (client_tx, _client_rx) = mpsc::channel(1);

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
        let RelayOutbound::Frame(OpaqueFrame::Text(raw)) = outbound else {
            panic!("expected mux text envelope");
        };
        serde_json::from_str(&raw).expect("mux envelope should decode")
    }
}
