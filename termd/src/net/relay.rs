//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame, ServerId, SessionId};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::config::RelayReconnectConfig;

use super::protocol::{JsonEnvelope, ProtocolConnection, ProtocolError};
use super::server::SharedDaemonProtocol;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;
const MIN_RELAY_RETRY_DELAY_MS: u64 = 1;
const MIN_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 1;

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

        let authority = match rest.split_once('/') {
            Some((authority, "")) => authority,
            Some(_) => return Err(RelayConnectorError::UnsupportedUrl),
            None => rest,
        };
        validate_authority(authority)?;
        Ok(Self {
            scheme,
            authority: authority.to_owned(),
        })
    }

    /// 返回去掉尾随斜杠后的 canonical endpoint 形式，便于配置层做去重。
    pub fn canonical_url(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority)
    }

    pub fn daemon_mux_url(&self, server_id: ServerId) -> String {
        format!(
            "{}://{}/ws/{}/daemon-mux",
            self.scheme.as_str(),
            self.authority,
            server_id.0
        )
    }

    pub fn daemon_mux_url_with_auth(
        &self,
        server_id: ServerId,
        auth_token: Option<&str>,
    ) -> String {
        let base = self.daemon_mux_url(server_id);
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
    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();
    let (push_event_tx, mut push_event_rx) = mpsc::unbounded_channel::<RelayPushEvent>();
    let mut watched_output_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_file_tree_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
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
            watcher_tasks,
        );
        return;
    };

    let (output_signals, file_tree_signals) = {
        let protocol = protocol.lock().expect("daemon protocol mutex poisoned");
        (
            connection.attached_output_signals(&protocol),
            connection.attached_file_tree_signals(&protocol),
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
}

fn remove_relay_watchers_for_client(
    client_id: RelayClientId,
    watched_output_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watched_file_tree_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
    watcher_tasks: &mut HashMap<RelayClientId, Vec<JoinHandle<()>>>,
) {
    watched_output_sessions.remove(&client_id);
    watched_file_tree_sessions.remove(&client_id);
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
    sender: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
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
        let Some((host, port)) = after_bracket.split_once("]:") else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        return Ok(());
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Err(RelayConnectorError::UnsupportedUrl);
    };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(RelayConnectorError::UnsupportedUrl);
    }
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use termd_proto::{Envelope, MessageType, PingPayload};

    fn test_protocol(name: &str) -> SharedDaemonProtocol {
        default_protocol(DaemonConfig::default_for_state_path(
            std::env::temp_dir().join(format!(
                "termd-relay-test-{}-{}-{name}.json",
                std::process::id(),
                current_unix_timestamp_millis().0
            )),
        ))
    }

    #[test]
    fn parses_relay_base_url_and_builds_daemon_mux_url() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();
        let url = base.daemon_mux_url(server_id);

        assert_eq!(
            url,
            format!("ws://127.0.0.1:8080/ws/{}/daemon-mux", server_id.0)
        );
    }

    #[test]
    fn parses_wss_relay_base_url_and_preserves_secure_scheme() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("wss://relay.example:443").unwrap();

        assert_eq!(
            base.daemon_mux_url(server_id),
            format!("wss://relay.example:443/ws/{}/daemon-mux", server_id.0)
        );
    }

    #[test]
    fn relay_base_url_canonical_url_drops_trailing_slash_variants() {
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();

        assert_eq!(base.canonical_url(), "ws://127.0.0.1:8080");
    }

    #[test]
    fn daemon_mux_url_can_carry_relay_auth_token_without_debug_leakage() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();
        let url = base.daemon_mux_url_with_auth(server_id, Some("relay-secret-1"));

        assert_eq!(
            url,
            format!(
                "ws://127.0.0.1:8080/ws/{}/daemon-mux?relay_token=relay-secret-1",
                server_id.0
            )
        );
        assert!(!format!("{base:?}").contains("relay-secret-1"));
    }

    #[test]
    fn rejects_unsupported_relay_urls() {
        assert!(RelayBaseUrl::parse("http://127.0.0.1:8080").is_err());
        assert!(RelayBaseUrl::parse("ws://127.0.0.1:8080/path").is_err());
        assert!(RelayBaseUrl::parse("ws://127.0.0.1").is_err());
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
            .route("/ws/:server_id/daemon-mux", get(mock_daemon_mux_ws))
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
        let protocol = test_protocol("mux-file-tree-push");
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
        let files_responses = direct_connection.handle_wire_envelope(
            &mut direct_protocol,
            encrypted_outer(
                &mut direct_e2ee,
                envelope_value(
                    MessageType::SessionFiles,
                    termd_proto::SessionFilesPayload {
                        session_id: created_payload.session_id,
                        path: Some("/tmp".to_owned()),
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
                "/tmp",
            ),
        )
        .await
        .expect("relay mux should push file tree updates without client polling");
        assert_eq!(pushed.session_id, created_payload.session_id);

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
