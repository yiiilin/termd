use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket};
use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame, ServerId};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const CHANNEL_CAPACITY: usize = 256;

type ConnectionId = u64;
type FrameSender = mpsc::Sender<OpaqueFrame>;

/// relay 只区分连接方向，不表达 controller/viewer 或任何控制权角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRole {
    Daemon,
    DaemonMux,
    Client,
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
    daemon: Option<ConnectionEndpoint>,
    daemon_mux: Option<ConnectionEndpoint>,
    clients: HashMap<ConnectionId, ConnectionEndpoint>,
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
    #[error("daemon already connected for server_id")]
    DuplicateDaemon,
    #[error("daemon mux already connected for server_id")]
    DuplicateDaemonMux,
    #[error("daemon is not connected for server_id")]
    DaemonOffline,
    #[error("relay state mutex poisoned")]
    Poisoned,
}

#[derive(Debug, Error)]
enum RelayMuxFrameError {
    #[error("relay mux frame binary payload is not valid base64")]
    InvalidBase64(#[source] base64::DecodeError),
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
            ConnectionRole::Daemon => {
                let room = rooms.entry(server_id).or_default();
                if room.daemon.is_some() {
                    return Err(RelayError::DuplicateDaemon);
                }
                room.daemon = Some(ConnectionEndpoint { id, sender });
            }
            ConnectionRole::DaemonMux => {
                let room = rooms.entry(server_id).or_default();
                if room.daemon_mux.is_some() {
                    return Err(RelayError::DuplicateDaemonMux);
                }
                room.daemon_mux = Some(ConnectionEndpoint { id, sender });
            }
            ConnectionRole::Client => {
                let room = rooms.get_mut(&server_id).ok_or(RelayError::DaemonOffline)?;
                if room.daemon.is_none() && room.daemon_mux.is_none() {
                    return Err(RelayError::DaemonOffline);
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
            ConnectionRole::Daemon => {
                if room
                    .daemon
                    .as_ref()
                    .is_some_and(|daemon| daemon.id == registration.id)
                {
                    room.daemon = None;
                }
            }
            ConnectionRole::DaemonMux => {
                if room
                    .daemon_mux
                    .as_ref()
                    .is_some_and(|daemon| daemon.id == registration.id)
                {
                    room.daemon_mux = None;
                }
            }
            ConnectionRole::Client => {
                room.clients.remove(&registration.id);
                if let Some(daemon_mux) = room.daemon_mux.as_ref() {
                    let envelope = RelayMuxEnvelope::ClientDisconnected {
                        client_id: RelayClientId(registration.id),
                    };
                    let _ = daemon_mux.sender.try_send(mux_envelope_frame(envelope));
                }
            }
        }

        if room.daemon.is_none() && room.daemon_mux.is_none() && room.clients.is_empty() {
            rooms.remove(&registration.server_id);
        }
    }

    fn forward_from(
        &self,
        registration: &ConnectionRegistration,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        match registration.role {
            ConnectionRole::Daemon => self.forward_to_clients(registration.server_id, frame),
            ConnectionRole::DaemonMux => self.forward_mux_to_client(registration.server_id, frame),
            ConnectionRole::Client => self.forward_client_to_mux_daemon(registration, frame),
        }
    }

    fn forward_to_clients(&self, server_id: ServerId, frame: OpaqueFrame) -> ForwardReport {
        let Ok(mut rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon fanout");
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

        let attempted = room.clients.len();
        let mut delivered = 0;
        let mut dropped_ids = Vec::new();

        for (client_id, client) in &room.clients {
            match client.sender.try_send(frame.clone()) {
                Ok(()) => delivered += 1,
                Err(error) => {
                    warn!(
                        server_id = %server_id.0,
                        connection_id = *client_id,
                        frame_kind = frame.kind(),
                        frame_len = frame.len(),
                        %error,
                        "dropping slow relay client"
                    );
                    dropped_ids.push(*client_id);
                }
            }
        }

        for client_id in &dropped_ids {
            room.clients.remove(client_id);
        }

        if room.daemon.is_none() && room.daemon_mux.is_none() && room.clients.is_empty() {
            rooms.remove(&server_id);
        }

        ForwardReport {
            attempted,
            delivered,
            dropped: dropped_ids.len(),
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
            return self.forward_to_daemon_locked(room, registration.server_id, frame);
        };

        let envelope = RelayMuxEnvelope::ClientFrame {
            client_id: RelayClientId(registration.id),
            frame: frame.into(),
        };
        match daemon_mux.sender.try_send(mux_envelope_frame(envelope)) {
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
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn forward_to_daemon_locked(
        &self,
        room: &mut RelayRoom,
        server_id: ServerId,
        frame: OpaqueFrame,
    ) -> ForwardReport {
        let Some(daemon) = room.daemon.as_ref() else {
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        match daemon.sender.try_send(frame.clone()) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(error) => {
                warn!(
                    server_id = %server_id.0,
                    connection_id = daemon.id,
                    frame_kind = frame.kind(),
                    frame_len = frame.len(),
                    %error,
                    "dropping offline relay daemon"
                );
                room.daemon = None;
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }

    fn forward_mux_to_client(&self, server_id: ServerId, frame: OpaqueFrame) -> ForwardReport {
        let Ok(rooms) = self.rooms.lock() else {
            warn!("relay registry mutex poisoned during daemon mux forward");
            return ForwardReport {
                attempted: 0,
                delivered: 0,
                dropped: 0,
            };
        };

        let Some(room) = rooms.get(&server_id) else {
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

        match client.sender.try_send(frame) {
            Ok(()) => ForwardReport {
                attempted: 1,
                delivered: 1,
                dropped: 0,
            },
            Err(error) => {
                warn!(
                    server_id = %server_id.0,
                    connection_id = client.id,
                    %error,
                    "dropping slow relay mux client"
                );
                ForwardReport {
                    attempted: 1,
                    delivered: 0,
                    dropped: 1,
                }
            }
        }
    }
}

pub async fn handle_socket(
    socket: WebSocket,
    state: RelayState,
    server_id: ServerId,
    role: ConnectionRole,
) {
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    let registration = match state.register(server_id, role, tx) {
        Ok(registration) => registration,
        Err(error) => {
            warn!(server_id = %server_id.0, ?role, %error, "rejecting relay websocket");
            return;
        }
    };

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

    loop {
        tokio::select! {
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

                if !handle_inbound_message(&state, &registration, &mut sender, inbound).await {
                    break;
                }
            }
            outbound = rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };

                let frame_kind = outbound.kind();
                let frame_len = outbound.len();
                if sender.send(outbound.into()).await.is_err() {
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

async fn handle_inbound_message(
    state: &RelayState,
    registration: &ConnectionRegistration,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    match message {
        Message::Text(text) => {
            forward_opaque(state, registration, OpaqueFrame::Text(text));
            true
        }
        Message::Binary(bytes) => {
            forward_opaque(state, registration, OpaqueFrame::Binary(bytes));
            true
        }
        Message::Ping(payload) => sender.send(Message::Pong(payload)).await.is_ok(),
        Message::Pong(_) => true,
        Message::Close(_) => false,
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
    let _ = daemon_mux.sender.try_send(mux_envelope_frame(envelope));
}

fn mux_envelope_frame(envelope: RelayMuxEnvelope) -> OpaqueFrame {
    let raw = serde_json::to_string(&envelope).expect("relay mux envelope should serialize");
    OpaqueFrame::Text(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    fn server_id(value: u128) -> ServerId {
        ServerId(uuid::Uuid::from_u128(value))
    }

    fn channel() -> (FrameSender, mpsc::Receiver<OpaqueFrame>) {
        mpsc::channel(CHANNEL_CAPACITY)
    }

    #[test]
    fn room_registers_one_daemon_and_many_clients() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (daemon_tx, _daemon_rx) = channel();
        let (client_a_tx, _client_a_rx) = channel();
        let (client_b_tx, _client_b_rx) = channel();

        state
            .register(server_id, ConnectionRole::Daemon, daemon_tx)
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
    fn room_rejects_duplicate_daemon() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (first_tx, _first_rx) = channel();
        let (second_tx, _second_rx) = channel();

        state
            .register(server_id, ConnectionRole::Daemon, first_tx)
            .unwrap();
        let error = state
            .register(server_id, ConnectionRole::Daemon, second_tx)
            .unwrap_err();

        assert_eq!(error, RelayError::DuplicateDaemon);
    }

    #[test]
    fn room_rejects_client_when_daemon_is_offline() {
        let state = RelayState::default();
        let (client_tx, _client_rx) = channel();

        let error = state
            .register(server_id(1), ConnectionRole::Client, client_tx)
            .unwrap_err();

        assert_eq!(error, RelayError::DaemonOffline);
    }

    #[test]
    fn daemon_fanout_sends_text_and_binary_to_all_clients() {
        let state = RelayState::default();
        let server_id = server_id(1);
        let (daemon_tx, _daemon_rx) = channel();
        let (client_a_tx, mut client_a_rx) = channel();
        let (client_b_tx, mut client_b_rx) = channel();

        let daemon = state
            .register(server_id, ConnectionRole::Daemon, daemon_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_a_tx)
            .unwrap();
        state
            .register(server_id, ConnectionRole::Client, client_b_tx)
            .unwrap();

        let text_report = state.forward_from(&daemon, OpaqueFrame::Text("{not-json".to_owned()));
        let binary_report = state.forward_from(&daemon, OpaqueFrame::Binary(vec![1, 2, 3]));

        assert_eq!(text_report.delivered, 2);
        assert_eq!(binary_report.delivered, 2);
        assert_eq!(
            client_a_rx.try_recv().unwrap(),
            OpaqueFrame::Text("{not-json".to_owned())
        );
        assert_eq!(
            client_a_rx.try_recv().unwrap(),
            OpaqueFrame::Binary(vec![1, 2, 3])
        );
        assert_eq!(
            client_b_rx.try_recv().unwrap(),
            OpaqueFrame::Text("{not-json".to_owned())
        );
        assert_eq!(
            client_b_rx.try_recv().unwrap(),
            OpaqueFrame::Binary(vec![1, 2, 3])
        );
    }

    #[test]
    fn client_frame_goes_only_to_matching_daemon() {
        let state = RelayState::default();
        let server_a = server_id(1);
        let server_b = server_id(2);
        let (daemon_a_tx, mut daemon_a_rx) = channel();
        let (daemon_b_tx, mut daemon_b_rx) = channel();
        let (client_a_tx, _client_a_rx) = channel();

        state
            .register(server_a, ConnectionRole::Daemon, daemon_a_tx)
            .unwrap();
        state
            .register(server_b, ConnectionRole::Daemon, daemon_b_tx)
            .unwrap();
        let client_a = state
            .register(server_a, ConnectionRole::Client, client_a_tx)
            .unwrap();

        let report = state.forward_from(&client_a, OpaqueFrame::Text("opaque".to_owned()));

        assert_eq!(report.delivered, 1);
        assert_eq!(
            daemon_a_rx.try_recv().unwrap(),
            OpaqueFrame::Text("opaque".to_owned())
        );
        assert_eq!(daemon_b_rx.try_recv().unwrap_err(), TryRecvError::Empty);
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
            OpaqueFrame::Binary(vec![1, 2, 3, 4])
        );
        assert_eq!(client_b_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn disconnect_cleans_room_without_affecting_other_server_id() {
        let state = RelayState::default();
        let server_a = server_id(1);
        let server_b = server_id(2);
        let (daemon_a_tx, _daemon_a_rx) = channel();
        let (daemon_b_tx, _daemon_b_rx) = channel();

        let daemon_a = state
            .register(server_a, ConnectionRole::Daemon, daemon_a_tx)
            .unwrap();
        state
            .register(server_b, ConnectionRole::Daemon, daemon_b_tx)
            .unwrap();

        state.unregister(&daemon_a);

        assert_eq!(state.room_count(), 1);
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

    fn decode_mux(frame: OpaqueFrame) -> RelayMuxEnvelope {
        let OpaqueFrame::Text(raw) = frame else {
            panic!("expected mux text envelope");
        };
        serde_json::from_str(&raw).expect("mux envelope should decode")
    }
}
