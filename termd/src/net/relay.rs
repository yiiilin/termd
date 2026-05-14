//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope as ProtoEnvelope, MessageType as ProtoMessageType, Nonce as ProtoNonce,
    ProtocolVersion as ProtoProtocolVersion, RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame,
    RouteHelloPayload as ProtoRouteHelloPayload, RouteReadyPayload as ProtoRouteReadyPayload,
    RouteRole as ProtoRouteRole, ServerId, SessionId,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::RelayReconnectConfig;

use super::protocol::{JsonEnvelope, ProtocolConnection, ProtocolError};
use super::server::SharedDaemonProtocol;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;
const MIN_RELAY_RETRY_DELAY_MS: u64 = 1;
const MIN_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 1;

type RelayWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type RelaySender = futures_util::stream::SplitSink<RelayWs, Message>;
type RelayReceiver = futures_util::stream::SplitStream<RelayWs>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayPushEvent {
    Output {
        client_id: RelayClientId,
        session_id: SessionId,
    },
    FileTree {
        client_id: RelayClientId,
        session_id: SessionId,
    },
    Resize {
        client_id: RelayClientId,
        session_id: SessionId,
    },
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

    pub fn heartbeat_interval(self) -> Duration {
        self.heartbeat_interval
    }

    pub fn next_retry_delay(self, current: Duration) -> Duration {
        current
            .checked_mul(2)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
            .max(self.initial_delay)
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

#[derive(Debug, Error)]
pub enum RelayConnectorError {
    #[error("unsupported relay URL; expected ws://host:port or wss://host:port")]
    UnsupportedUrl,
    #[error("failed to connect relay daemon mux websocket")]
    ConnectFailed,
    #[error("relay websocket receive failed")]
    ReceiveFailed,
    #[error("relay websocket send failed")]
    SendFailed,
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
    connect_relay_mux_base_with_heartbeat(
        base,
        auth_token,
        RelayReconnectPolicy::default().heartbeat_interval(),
        protocol,
    )
    .await
}

pub async fn run_relay_mux_with_reconnect(
    relay_url: &str,
    auth_token: Option<&str>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    run_relay_mux_with_reconnect_base(base, auth_token, policy, protocol).await
}

pub async fn run_relay_mux_with_reconnect_base(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let mut retry_delay = policy.first_retry_delay();

    loop {
        let result = connect_relay_mux_base_with_heartbeat(
            base.clone(),
            auth_token,
            policy.heartbeat_interval(),
            protocol.clone(),
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
        retry_delay = policy.next_retry_delay(retry_delay);
    }
}

async fn connect_relay_mux_base_with_heartbeat(
    base: RelayBaseUrl,
    auth_token: Option<&str>,
    heartbeat_interval: Duration,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let server_id = {
        protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .server_id()
    };
    let url = base.daemon_mux_url_with_auth(server_id, auth_token);
    let (socket, _) = connect_async(url)
        .await
        .map_err(|_| RelayConnectorError::ConnectFailed)?;
    let (mut sender, mut receiver) = socket.split();
    send_route_hello(&mut sender, server_id).await?;
    read_route_ready(&mut sender, &mut receiver, server_id).await?;

    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();
    let (push_event_tx, mut push_event_rx) = mpsc::unbounded_channel::<RelayPushEvent>();
    let mut watched_output_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_file_tree_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_resize_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watcher_tasks = HashMap::<RelayClientId, Vec<JoinHandle<()>>>::new();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result = loop {
        tokio::select! {
            inbound = receiver.next() => {
                let Some(message) = inbound else {
                    break Ok(());
                };
                let message = match message {
                    Ok(message) => message,
                    Err(_) => break Err(RelayConnectorError::ReceiveFailed),
                };

                match message {
                    Message::Text(raw) => {
                        let envelope: RelayMuxEnvelope = match serde_json::from_str(raw.as_str()) {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        let client_id = relay_envelope_client_id(&envelope);
                        let responses = match handle_mux_envelope(envelope, &protocol, &mut connections) {
                            Ok(responses) => responses,
                            Err(error) => break Err(error),
                        };
                        if let Err(error) = send_mux_envelopes(&mut sender, responses).await {
                            break Err(error);
                        }
                        sync_relay_watchers_for_client(
                            client_id,
                            &connections,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_file_tree_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        );
                    }
                    Message::Binary(raw) => {
                        let envelope: RelayMuxEnvelope = match serde_json::from_slice(&raw) {
                            Ok(envelope) => envelope,
                            Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                        };
                        let client_id = relay_envelope_client_id(&envelope);
                        let responses = match handle_mux_envelope(envelope, &protocol, &mut connections) {
                            Ok(responses) => responses,
                            Err(error) => break Err(error),
                        };
                        if let Err(error) = send_mux_envelopes(&mut sender, responses).await {
                            break Err(error);
                        }
                        sync_relay_watchers_for_client(
                            client_id,
                            &connections,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_file_tree_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        );
                    }
                    Message::Ping(payload) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break Err(RelayConnectorError::SendFailed);
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break Ok(()),
                    Message::Frame(_) => {}
                }
            }
            _ = heartbeat.tick() => {
                // 心跳只使用 WebSocket control frame，不进入 termd 的 JSON envelope / E2EE 状态机。
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break Err(RelayConnectorError::SendFailed);
                }
            }
            maybe_event = push_event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break Err(RelayConnectorError::SendFailed);
                };
                let (client_id, responses) = {
                    let (client_id, session_id) = match event {
                        RelayPushEvent::Output { client_id, session_id } => (client_id, session_id),
                        RelayPushEvent::FileTree { client_id, session_id } => (client_id, session_id),
                        RelayPushEvent::Resize { client_id, session_id } => (client_id, session_id),
                    };
                    let Some(connection) = connections.get_mut(&client_id) else {
                        continue;
                    };
                    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                    let responses = match event {
                        RelayPushEvent::Output { .. } => connection.read_session_output(
                            &mut protocol,
                            session_id,
                            OUTPUT_FLUSH_MAX_BYTES_PER_SESSION,
                        ),
                        RelayPushEvent::FileTree { .. } => {
                            connection.read_session_file_tree_update(&mut protocol, session_id)
                        }
                        RelayPushEvent::Resize { .. } => {
                            connection.read_session_resize_update(&mut protocol, session_id)
                        }
                    };
                    (client_id, responses)
                };
                let responses = client_envelopes(client_id, responses)?;
                if let Err(error) = send_mux_envelopes(&mut sender, responses).await {
                    break Err(error);
                }
            }
        }
    };

    abort_relay_watcher_tasks(watcher_tasks);
    close_relay_connections(protocol, connections);
    result
}

async fn send_route_hello(
    sender: &mut RelaySender,
    server_id: ServerId,
) -> Result<(), RelayConnectorError> {
    let envelope = ProtoEnvelope::new(
        ProtoMessageType::RouteHello,
        ProtoRouteHelloPayload {
            server_id,
            role: ProtoRouteRole::DaemonMux,
            protocol_version: ProtoProtocolVersion::default(),
            nonce: relay_route_nonce(),
            timestamp_ms: current_unix_timestamp_millis(),
        },
    );
    let raw = serde_json::to_string(&envelope).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    sender
        .send(Message::Text(raw.into()))
        .await
        .map_err(|_| RelayConnectorError::SendFailed)
}

async fn read_route_ready(
    sender: &mut RelaySender,
    receiver: &mut RelayReceiver,
    expected_server_id: ServerId,
) -> Result<(), RelayConnectorError> {
    loop {
        let Some(message) = receiver.next().await else {
            return Err(RelayConnectorError::ReceiveFailed);
        };
        let message = message.map_err(|_| RelayConnectorError::ReceiveFailed)?;

        match message {
            Message::Text(raw) => {
                let envelope: JsonEnvelope = serde_json::from_str(raw.as_str())
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                validate_route_ready(envelope, expected_server_id)?;
                return Ok(());
            }
            Message::Binary(raw) => {
                let envelope: JsonEnvelope = serde_json::from_slice(&raw)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                validate_route_ready(envelope, expected_server_id)?;
                return Ok(());
            }
            Message::Ping(payload) => {
                if sender.send(Message::Pong(payload)).await.is_err() {
                    return Err(RelayConnectorError::SendFailed);
                }
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Err(RelayConnectorError::ReceiveFailed),
        }
    }
}

fn validate_route_ready(
    envelope: JsonEnvelope,
    expected_server_id: ServerId,
) -> Result<(), RelayConnectorError> {
    if envelope.kind != ProtoMessageType::RouteReady {
        return Err(RelayConnectorError::InvalidEnvelope);
    }
    let payload: ProtoRouteReadyPayload = serde_json::from_value(envelope.payload)
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    if payload.server_id != expected_server_id || payload.role != ProtoRouteRole::DaemonMux {
        return Err(RelayConnectorError::InvalidEnvelope);
    }
    Ok(())
}

fn relay_route_nonce() -> ProtoNonce {
    ProtoNonce(format!("relay-route-{}", ServerId::new().0))
}

fn handle_mux_envelope(
    envelope: RelayMuxEnvelope,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    match envelope {
        RelayMuxEnvelope::ClientConnected { client_id } => {
            let (connection, initial_messages) = {
                let protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                protocol.start_connection()
            };
            connections.insert(client_id, connection);
            debug!(
                client_id = client_id.0,
                "relay client connected to daemon mux"
            );
            client_envelopes(client_id, initial_messages)
        }
        RelayMuxEnvelope::ClientDisconnected { client_id } => {
            if let Some(mut connection) = connections.remove(&client_id) {
                let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
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

            let frame = match json_envelope_from_mux_frame(frame) {
                Ok(frame) => frame,
                Err(error) => {
                    // relay client 是非可信输入源；坏业务 frame 只能影响该 client，不能杀掉
                    // daemon outbound connector 或 direct daemon。
                    close_client_connection(protocol, connections, client_id);
                    warn!(
                        client_id = client_id.0,
                        %error,
                        "closed relay client after invalid daemon protocol frame"
                    );
                    return Ok(Vec::new());
                }
            };
            let responses = {
                let connection = connections
                    .get_mut(&client_id)
                    .expect("connection existence checked before frame parsing");
                let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                let mut responses = connection.handle_wire_envelope(&mut protocol, frame);
                responses.extend(
                    connection
                        .read_attached_outputs(&mut protocol, OUTPUT_FLUSH_MAX_BYTES_PER_SESSION),
                );
                responses
            };
            client_envelopes(client_id, responses)
        }
        RelayMuxEnvelope::DaemonFrame { .. } => Err(RelayConnectorError::InvalidEnvelope),
    }
}

fn relay_envelope_client_id(envelope: &RelayMuxEnvelope) -> Option<RelayClientId> {
    match envelope {
        RelayMuxEnvelope::ClientConnected { client_id }
        | RelayMuxEnvelope::ClientDisconnected { client_id }
        | RelayMuxEnvelope::ClientFrame { client_id, .. } => Some(*client_id),
        RelayMuxEnvelope::DaemonFrame { .. } => None,
    }
}

fn sync_relay_watchers_for_client(
    client_id: Option<RelayClientId>,
    connections: &HashMap<RelayClientId, ProtocolConnection>,
    protocol: &SharedDaemonProtocol,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_file_tree_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    push_event_tx: &mpsc::UnboundedSender<RelayPushEvent>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) {
    let Some(client_id) = client_id else {
        return;
    };
    let Some(connection) = connections.get(&client_id) else {
        remove_relay_watchers_for_client(
            client_id,
            watched_output_sessions,
            watched_file_tree_sessions,
            watched_resize_sessions,
            watcher_tasks,
        );
        return;
    };

    let (output_signals, file_tree_signals, resize_signals) = {
        let protocol = protocol.lock().expect("daemon protocol mutex poisoned");
        (
            connection.attached_output_signals(&protocol),
            connection.attached_file_tree_signals(&protocol),
            connection.attached_resize_signals(&protocol),
        )
    };

    for (session_id, mut signal) in output_signals {
        let watched = watched_output_sessions.entry(client_id).or_default();
        if !watched.insert(session_id) {
            continue;
        }

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
                        .send(RelayPushEvent::Output {
                            client_id,
                            session_id,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
    }

    for (session_id, mut signal) in file_tree_signals {
        let watched = watched_file_tree_sessions.entry(client_id).or_default();
        if !watched.insert(session_id) {
            continue;
        }

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
                        .send(RelayPushEvent::FileTree {
                            client_id,
                            session_id,
                        })
                        .is_err()
                    {
                        break;
                    }
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
                        .is_err()
                    {
                        break;
                    }
                }
            }));
    }
}

fn remove_relay_watchers_for_client(
    client_id: RelayClientId,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_file_tree_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) {
    watched_output_sessions.remove(&client_id);
    watched_file_tree_sessions.remove(&client_id);
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

async fn send_mux_envelopes(
    sender: &mut RelaySender,
    envelopes: Vec<RelayMuxEnvelope>,
) -> Result<(), RelayConnectorError> {
    for envelope in envelopes {
        let raw =
            serde_json::to_string(&envelope).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
        sender
            .send(Message::Text(raw.into()))
            .await
            .map_err(|_| RelayConnectorError::SendFailed)?;
    }
    Ok(())
}

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

fn close_relay_connections(
    protocol: SharedDaemonProtocol,
    connections: HashMap<RelayClientId, ProtocolConnection>,
) {
    if connections.is_empty() {
        return;
    }

    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
    for (_client_id, mut connection) in connections {
        connection.close(&mut protocol);
    }
}

fn close_client_connection(
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    client_id: RelayClientId,
) {
    if let Some(mut connection) = connections.remove(&client_id) {
        let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
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
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use termd_proto::{
        Envelope, MessageType, PingPayload, ProtocolVersion, RouteHelloPayload, RouteReadyPayload,
        RouteRole,
    };

    static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_protocol(name: &str) -> SharedDaemonProtocol {
        default_protocol(DaemonConfig::default_for_state_path(temp_state_path(name)))
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
        assert_eq!(
            inverted.next_retry_delay(Duration::from_millis(50)),
            Duration::from_millis(50)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_supervisor_retries_after_close_and_sends_heartbeat() {
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
            base, None, policy, protocol,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.attempts.load(Ordering::SeqCst) >= 2
                    && state.heartbeat_pings.load(Ordering::SeqCst) >= 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        connector.abort();
        server.abort();
    }

    #[derive(Clone, Default)]
    struct MockMuxState {
        attempts: Arc<AtomicUsize>,
        heartbeat_pings: Arc<AtomicUsize>,
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
            if route_hello.role != RouteRole::DaemonMux {
                return;
            }
            let route_ready = Envelope::new(
                MessageType::RouteReady,
                RouteReadyPayload {
                    server_id: route_hello.server_id,
                    role: RouteRole::DaemonMux,
                },
            );
            let raw = serde_json::to_string(&route_ready).unwrap();
            if socket.send(AxumMessage::Text(raw.into())).await.is_err() {
                return;
            }

            while let Some(message) = socket.next().await {
                match message {
                    Ok(AxumMessage::Ping(payload)) => {
                        state.heartbeat_pings.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.send(AxumMessage::Pong(payload)).await;
                        break;
                    }
                    Ok(AxumMessage::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
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
    fn mux_client_connection_can_complete_pairing_on_independent_protocol_connection() {
        let protocol = test_protocol("mux-pairing");
        let client_id = RelayClientId(42);
        let mut connections = HashMap::new();

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected { client_id },
            &protocol,
            &mut connections,
        )
        .unwrap();
        let hello = daemon_frame_to_json(initial[0].clone());
        let key_exchange = daemon_frame_to_json(initial[1].clone());
        assert_eq!(hello.kind, MessageType::Hello);
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);

        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
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
            termd_proto::E2eeKeyExchangePayload {
                server_id: server_key_exchange.server_id,
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            &protocol,
            &mut connections,
        )
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
        )
        .unwrap();

        let outer = daemon_frame_to_json(pair_responses[0].clone());
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        let inner: JsonEnvelope = device_e2ee.decrypt_json_payload(&frame).unwrap();
        let accepted: termd_proto::PairAcceptPayload = decode_payload(inner.payload).unwrap();

        assert_eq!(inner.kind, MessageType::PairAccept);
        assert_eq!(accepted.device_id, device_id);
        assert_eq!(accepted.server_id, server_key_exchange.server_id);
    }

    #[test]
    fn invalid_mux_client_frame_closes_only_that_client_connection() {
        let protocol = test_protocol("invalid-mux-frame");
        let bad_client_id = RelayClientId(1);
        let good_client_id = RelayClientId(2);
        let mut connections = HashMap::new();

        handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: bad_client_id,
            },
            &protocol,
            &mut connections,
        )
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
        );

        assert!(bad_result.unwrap().is_empty());
        assert!(!connections.contains_key(&bad_client_id));

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: good_client_id,
            },
            &protocol,
            &mut connections,
        )
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
            .expect("daemon protocol mutex poisoned")
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
        );

        assert_eq!(pair_response.kind, MessageType::PairAccept);
        assert!(connections.contains_key(&good_client_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_pushes_session_output_without_client_poll_frame() {
        let protocol = test_protocol("mux-output-push");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_with_heartbeat(
            base,
            None,
            Duration::from_secs(60),
            protocol.clone(),
        ));
        let (tcp, _) = listener.accept().await.unwrap();
        let mut relay_socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let client_id = RelayClientId(77);
        let expected_server_id = protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .server_id();
        complete_relay_route_prelude(&mut relay_socket, expected_server_id).await;

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
            .expect("daemon protocol mutex poisoned")
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
                termd_proto::E2eeKeyExchangePayload {
                    server_id: server_key_exchange.server_id,
                    device_id,
                    public_key: device_keypair.public_key_wire(),
                    nonce: termd_proto::Nonce("relay-push-e2ee-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
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
        let pushed = tokio::time::timeout(
            Duration::from_secs(2),
            read_encrypted_daemon_frame(&mut relay_socket, client_id, &mut device_e2ee),
        )
        .await
        .expect("relay mux should push PTY output without client polling");
        assert_eq!(pushed.kind, MessageType::SessionData);
        let payload: termd_proto::SessionDataPayload = decode_payload(pushed.payload).unwrap();
        assert_eq!(payload.session_id, created_payload.session_id);
        let output = general_purpose::STANDARD
            .decode(payload.data_base64)
            .unwrap();
        assert_eq!(output, b"relay-pushed-output");

        connector.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_pushes_file_tree_updates_without_client_poll_frame() {
        let file_root = std::env::temp_dir().join(format!(
            "termd-relay-file-tree-root-{}-{}",
            std::process::id(),
            TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&file_root).unwrap();
        fs::write(file_root.join("alpha.txt"), b"alpha\n").unwrap();

        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("mux-file-tree-push-state"));
        // 文件树推送测试不能读取共享 `/tmp`，否则会和并行测试清理临时文件产生竞态。
        config.default_working_directory = Some(file_root.clone());
        let protocol = default_protocol(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = RelayBaseUrl::parse(&format!("ws://{addr}")).unwrap();
        let connector = tokio::spawn(connect_relay_mux_base_with_heartbeat(
            base,
            None,
            Duration::from_secs(60),
            protocol.clone(),
        ));
        let (tcp, _) = listener.accept().await.unwrap();
        let mut relay_socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let client_id = RelayClientId(78);
        let expected_server_id = protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .server_id();
        complete_relay_route_prelude(&mut relay_socket, expected_server_id).await;

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
            .expect("daemon protocol mutex poisoned")
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let (device_id, mut device_e2ee) = open_mux_e2ee(
            &mut relay_socket,
            client_id,
            server_key_exchange.clone(),
            "relay-file-tree-e2ee-nonce",
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
                        "ed25519-v1:relay-file-tree-test-device".to_owned(),
                    ),
                    token: termd_proto::PairingToken(token),
                    nonce: termd_proto::Nonce("relay-file-tree-pair-nonce".to_owned()),
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
                    command: vec!["sh".to_owned(), "-lc".to_owned(), "sleep 2".to_owned()],
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

        let mut direct_protocol = protocol.lock().expect("daemon protocol mutex poisoned");
        let direct_token = direct_protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let (mut direct_connection, direct_initial) = direct_protocol.start_connection();
        let direct_server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(direct_initial[1].payload.clone()).unwrap();
        let direct_device_id = termd_proto::DeviceId::new();
        let direct_keypair = E2eeKeyPair::generate();
        let direct_server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&direct_server_key_exchange.public_key)
                .unwrap();
        let direct_context = E2eeSessionContext::new(
            direct_server_key_exchange.server_id,
            direct_device_id,
            direct_server_e2ee_key,
            direct_keypair.public_key(),
        );
        let mut direct_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &direct_keypair,
            direct_server_e2ee_key,
            direct_context,
        )
        .unwrap();
        direct_connection.handle_wire_envelope(
            &mut direct_protocol,
            envelope_value(
                MessageType::E2eeKeyExchange,
                termd_proto::E2eeKeyExchangePayload {
                    server_id: direct_server_key_exchange.server_id,
                    device_id: direct_device_id,
                    public_key: direct_keypair.public_key_wire(),
                    nonce: termd_proto::Nonce("direct-file-tree-e2ee-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        );
        let pair_responses = direct_connection.handle_wire_envelope(
            &mut direct_protocol,
            encrypted_outer(
                &mut direct_e2ee,
                envelope_value(
                    MessageType::PairRequest,
                    termd_proto::PairRequestPayload {
                        device_id: direct_device_id,
                        device_public_key: termd_proto::PublicKey(
                            "ed25519-v1:direct-file-tree-device".to_owned(),
                        ),
                        token: termd_proto::PairingToken(direct_token),
                        nonce: termd_proto::Nonce("direct-file-tree-pair-nonce".to_owned()),
                        timestamp_ms: current_unix_timestamp_millis(),
                    },
                )
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_protocol_response(&mut direct_e2ee, pair_responses[0].clone()).kind,
            MessageType::PairAccept
        );
        let attach_responses = direct_connection.handle_wire_envelope(
            &mut direct_protocol,
            encrypted_outer(
                &mut direct_e2ee,
                envelope_value(
                    MessageType::SessionAttach,
                    termd_proto::SessionAttachPayload {
                        session_id: created_payload.session_id,
                    },
                )
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_protocol_response(&mut direct_e2ee, attach_responses[0].clone()).kind,
            MessageType::SessionAttached
        );
        let files_responses = direct_connection.handle_wire_envelope(
            &mut direct_protocol,
            encrypted_outer(
                &mut direct_e2ee,
                envelope_value(
                    MessageType::SessionFiles,
                    termd_proto::SessionFilesPayload {
                        session_id: created_payload.session_id,
                        path: Some(file_root.to_string_lossy().to_string()),
                    },
                )
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_protocol_response(&mut direct_e2ee, files_responses[0].clone()).kind,
            MessageType::SessionFilesResult
        );
        drop(direct_protocol);

        // 文件树状态变化来自 direct daemon 连接；当前 relay client 不发送任何帧也必须收到同步结果。
        let pushed = tokio::time::timeout(
            Duration::from_secs(2),
            read_session_files_result_with_path(
                &mut relay_socket,
                client_id,
                &mut device_e2ee,
                &file_root.to_string_lossy(),
            ),
        )
        .await
        .expect("relay mux should push file tree updates without client polling");
        assert_eq!(pushed.session_id, created_payload.session_id);

        fs::remove_dir_all(file_root).ok();

        connector.abort();
    }

    fn complete_pairing_via_mux(
        protocol: &SharedDaemonProtocol,
        connections: &mut HashMap<RelayClientId, ProtocolConnection>,
        client_id: RelayClientId,
        server_key_exchange: termd_proto::E2eeKeyExchangePayload,
        token: String,
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
            termd_proto::E2eeKeyExchangePayload {
                server_id: server_key_exchange.server_id,
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            protocol,
            connections,
        )
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
        )
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

    async fn complete_relay_route_prelude(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_server_id: ServerId,
    ) {
        let route_hello = tokio::time::timeout(
            Duration::from_secs(1),
            read_route_hello_from_connector(socket),
        )
        .await
        .expect("connector should send route_hello before relay mux envelopes");
        assert_eq!(route_hello.server_id, expected_server_id);
        assert_eq!(route_hello.role, RouteRole::DaemonMux);
        assert_eq!(route_hello.protocol_version, ProtocolVersion::default());

        let route_ready = Envelope::new(
            MessageType::RouteReady,
            RouteReadyPayload {
                server_id: expected_server_id,
                role: RouteRole::DaemonMux,
            },
        );
        socket
            .send(Message::Text(
                serde_json::to_string(&route_ready).unwrap().into(),
            ))
            .await
            .unwrap();
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

    fn encrypted_outer(device_e2ee: &mut E2eeSession, inner: JsonEnvelope) -> JsonEnvelope {
        envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&inner).unwrap(),
        )
        .unwrap()
    }

    fn decrypt_protocol_response(
        device_e2ee: &mut E2eeSession,
        outer: JsonEnvelope,
    ) -> JsonEnvelope {
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    async fn open_mux_e2ee(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        client_id: RelayClientId,
        daemon_exchange: termd_proto::E2eeKeyExchangePayload,
        nonce: &str,
    ) -> (termd_proto::DeviceId, E2eeSession) {
        let device_id = termd_proto::DeviceId::new();
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
                termd_proto::E2eeKeyExchangePayload {
                    server_id: daemon_exchange.server_id,
                    device_id,
                    public_key: device_keypair.public_key_wire(),
                    nonce: termd_proto::Nonce(nonce.to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        )
        .await;

        (device_id, device_e2ee)
    }

    async fn read_mux_from_connector(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> RelayMuxEnvelope {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            match message {
                Message::Text(raw) => return serde_json::from_str(raw.as_str()).unwrap(),
                Message::Binary(raw) => return serde_json::from_slice(&raw).unwrap(),
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

    async fn read_encrypted_daemon_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
    ) -> JsonEnvelope {
        let outer = read_daemon_frame_from_connector(socket, expected_client_id).await;
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    async fn read_session_files_result_with_path(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        expected_client_id: RelayClientId,
        device_e2ee: &mut E2eeSession,
        expected_path: &str,
    ) -> termd_proto::SessionFilesResultPayload {
        loop {
            let inner = read_encrypted_daemon_frame(socket, expected_client_id, device_e2ee).await;
            if inner.kind != MessageType::SessionFilesResult {
                continue;
            }
            let payload: termd_proto::SessionFilesResultPayload =
                decode_payload(inner.payload).unwrap();
            if payload.path == expected_path {
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
