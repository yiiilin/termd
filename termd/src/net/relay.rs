//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use rustls::{ClientConfig, RootCertStore};
use termd_proto::{
    Envelope as ProtoEnvelope, MessageType as ProtoMessageType, Nonce as ProtoNonce,
    PROTOCOL_PACKET_VERSION, ProtocolVersion as ProtoProtocolVersion, RelayClientId,
    RelayMuxEnvelope, RelayOpaqueFrame, RouteHelloPayload as ProtoRouteHelloPayload,
    RouteReadyPayload as ProtoRouteReadyPayload, RouteRole as ProtoRouteRole, ServerId, SessionId,
    decode_binary_relay_mux_envelope, encode_binary_relay_mux_envelope,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::{
    Connector,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tracing::{debug, info, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::RelayReconnectConfig;

use super::protocol::{
    JsonEnvelope, ProtocolConnection, ProtocolConnectionDebugTraffic, ProtocolError,
    ProtocolWireMessage,
};
use super::server::SharedDaemonProtocol;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;
const MIN_RELAY_RETRY_DELAY_MS: u64 = 1;
const MIN_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 1;
// relay mux transport 失败只会断开当前 relay 连接并触发重连，不关闭持久 session/supervisor。
// 公网 relay 往往还隔着 TLS 和反向代理，2s 级 deadline 容易把短暂抖动误判成断线。
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_ROUTE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_SEND_DEADLINE: Duration = Duration::from_secs(10);
const RELAY_PONG_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
const RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const RELAY_RECONNECT_STABLE_RESET_AFTER: Duration = Duration::from_secs(60);
const RELAY_MAX_FRAME_SIZE: usize = 1024 * 1024;
const RELAY_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const RELAY_TRAFFIC_LOG_INTERVAL: Duration = Duration::from_secs(1);
const RELAY_SEND_SLOW_LOG_THRESHOLD: Duration = Duration::from_millis(50);
const RELAY_SEND_DEBUG_LOG_THRESHOLD: Duration = Duration::from_millis(10);
const RELAY_SEND_DEBUG_BATCH_ENVELOPES: usize = 8;
const RELAY_SEND_DEBUG_BATCH_BYTES: usize = 32 * 1024;
const RELAY_SEND_INFO_BATCH_ENVELOPES: usize = 20;
const RELAY_SEND_INFO_BATCH_BYTES: usize = 256 * 1024;
const RELAY_MUX_CONTROL_QUEUE_CAPACITY: usize = 256;
const RELAY_MUX_OUTPUT_QUEUE_CAPACITY: usize = 256;
const RELAY_MUX_WRITE_OUTCOME_QUEUE_CAPACITY: usize = 256;
const RELAY_PUSH_EVENT_QUEUE_CAPACITY: usize = 2048;
const RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK: usize = 4;
const RELAY_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK: usize = 256 * 1024;
const RELAY_PUSH_DRAIN_MAX_ELAPSED: Duration = Duration::from_millis(2);

type RelayWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
type RelaySender = futures_util::stream::SplitSink<RelayWs, Message>;
type RelayReceiver = futures_util::stream::SplitStream<RelayWs>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Default)]
struct RelayPushEventQueue {
    pending: VecDeque<RelayPushEvent>,
    pending_set: HashSet<RelayPushEvent>,
    inflight: HashSet<RelayPushEvent>,
    dirty_inflight: HashSet<RelayPushEvent>,
}

impl RelayPushEventQueue {
    fn enqueue(&mut self, event: RelayPushEvent) {
        if self.pending_set.contains(&event) {
            return;
        }
        if self.inflight.contains(&event) {
            // 中文注释：relay output 发送期间的重复 watch 信号只标脏。
            // 发送完成后最多补一次，避免一个慢 relay/client 把 daemon 事件队列刷爆。
            self.dirty_inflight.insert(event);
            return;
        }
        self.pending_set.insert(event);
        self.pending.push_back(event);
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn pop_front_for_inflight(&mut self) -> Option<RelayPushEvent> {
        let event = self.pending.pop_front()?;
        self.pending_set.remove(&event);
        self.inflight.insert(event);
        Some(event)
    }

    fn peek_front(&self) -> Option<RelayPushEvent> {
        self.pending.front().copied()
    }

    fn finish_inflight_after_send(&mut self, events: &[RelayPushEvent]) {
        for event in events {
            let should_requeue = self.dirty_inflight.remove(event);
            self.inflight.remove(event);
            if should_requeue {
                self.enqueue(*event);
            }
        }
    }
}

#[derive(Debug)]
enum RelayMuxWrite {
    Envelopes {
        kind: RelayOutKind,
        envelopes: Vec<RelayMuxEnvelope>,
        push_events: Vec<RelayPushEvent>,
    },
    Raw {
        kind: RelayOutKind,
        message: Message,
        bytes: usize,
    },
}

#[derive(Debug)]
enum RelayMuxWriteOutcome {
    Sent {
        channel: RelayMuxChannel,
        kind: RelayOutKind,
        envelopes: usize,
        bytes: usize,
        push_events: Vec<RelayPushEvent>,
    },
    Failed {
        channel: RelayMuxChannel,
        error: RelayConnectorError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayMuxChannel {
    Control,
    Data,
}

impl RelayMuxChannel {
    fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Data => "data",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct RelayTrafficBucket {
    calls: u64,
    envelopes: u64,
    bytes: u64,
}

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
    out_push_file_tree: RelayTrafficBucket,
    out_push_resize: RelayTrafficBucket,
    out_idle_ping: RelayTrafficBucket,
    out_pong: RelayTrafficBucket,
    send_errors: u64,
}

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
            RelayOutKind::PushOutput => self.out_push_output.record(envelopes, bytes),
            RelayOutKind::PushFileTree => self.out_push_file_tree.record(envelopes, bytes),
            RelayOutKind::PushResize => self.out_push_resize.record(envelopes, bytes),
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
            || !self.out_push_file_tree.is_empty()
            || !self.out_push_resize.is_empty()
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

#[derive(Debug, Clone, Copy)]
struct RelayHeartbeatDebug {
    last_inbound_at: Instant,
    last_outbound_at: Instant,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
}

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
    PushOutput,
    PushFileTree,
    PushResize,
    IdlePing,
    Pong,
}

impl RelayOutKind {
    fn is_payload_batch(self) -> bool {
        // 空业务 batch 不会写 WebSocket；忽略它，避免零 credit 时把 watcher 空转记成输出流量。
        !matches!(self, Self::Pong | Self::IdlePing)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::PushOutput => "push_output",
            Self::PushFileTree => "push_file_tree",
            Self::PushResize => "push_resize",
            Self::IdlePing => "idle_ping",
            Self::Pong => "pong",
        }
    }
}

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

fn relay_daemon_mux_idle_ping_enabled() -> bool {
    // daemon 是 relay mux 主干连接的 owner；空闲时由 daemon 主动发标准 WebSocket Ping。
    // relay 不解析业务心跳，连接断开后才裁定 daemon 离线。
    true
}

fn relay_daemon_mux_inbound_idle_timeout_enabled() -> bool {
    // daemon->relay 是一条长期主干连接：空闲时可能只有 daemon 发出的 WebSocket Ping，
    // relay/Pong 不进入业务数据流。这里不能因为“没有业务入站帧”主动断开，否则会让健康
    // 主干每 120s 自杀一次，并在 Web 侧表现成 relay 离线或操作超时。
    false
}

#[derive(Debug, Default, Clone, Copy)]
struct RelayWatcherCounts {
    output: usize,
    file_tree: usize,
    resize: usize,
}

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
    connect_relay_mux_base_once(
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
        let result = connect_relay_mux_base_once(
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
    // 中文注释：control/data 两条 WebSocket 必须带同一个公开 route generation。
    // relay 只用它做连接代际绑定，防止旧 data 通道把上一个 daemon mux 的输出投给新 client。
    let route_generation = relay_route_nonce();
    let (control_sender, mut control_receiver) = connect_relay_mux_socket(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonMux,
        route_generation.clone(),
    )
    .await?;
    let (data_sender, data_receiver) = connect_relay_mux_socket(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonMuxData,
        route_generation,
    )
    .await?;
    let (write_raw_control_tx, write_raw_control_rx) =
        mpsc::channel::<RelayMuxWrite>(RELAY_MUX_CONTROL_QUEUE_CAPACITY);
    let (write_client_tx, write_client_rx) =
        mpsc::channel::<RelayMuxWrite>(RELAY_MUX_OUTPUT_QUEUE_CAPACITY);
    let (write_outcome_tx, mut write_outcome_rx) =
        mpsc::channel::<RelayMuxWriteOutcome>(RELAY_MUX_WRITE_OUTCOME_QUEUE_CAPACITY);
    // 中文注释：control/data 使用两条物理 WebSocket。
    // 仅拆队列还不够：单条 WebSocket 正在 flush 大 output frame 时无法抢占，
    // fresh client hello、输入响应和断开通知仍会排在旧输出后面。
    let control_writer_task = tokio::spawn(run_relay_mux_writer(
        relay_endpoint.clone(),
        RelayMuxChannel::Control,
        control_sender,
        write_raw_control_rx,
        write_outcome_tx.clone(),
    ));
    let data_task = tokio::spawn(run_relay_mux_data_task(
        relay_endpoint.clone(),
        data_sender,
        data_receiver,
        write_client_rx,
        write_outcome_tx,
        heartbeat_interval,
    ));

    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();
    let (push_event_tx, mut push_event_rx) =
        mpsc::channel::<RelayPushEvent>(RELAY_PUSH_EVENT_QUEUE_CAPACITY);
    let mut push_drain_wake_pending = false;
    let mut watched_output_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_file_tree_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watched_resize_sessions = HashMap::<RelayClientId, HashSet<SessionId>>::new();
    let mut watcher_tasks = HashMap::<RelayClientId, Vec<JoinHandle<()>>>::new();
    let mut pending_push_events = RelayPushEventQueue::default();
    let mut idle_deadline = Instant::now() + RELAY_IDLE_TIMEOUT;
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let idle_ping_enabled = relay_daemon_mux_idle_ping_enabled();
    let mut last_control_activity = Instant::now();
    let mut last_idle_ping_sent_at = Instant::now();
    let mut idle_ping_nonce: u64 = 0;
    let mut traffic = RelayTrafficCounters::default();
    let mut last_traffic_log = Instant::now();
    let mut heartbeat_debug = RelayHeartbeatDebug::new(Instant::now());

    let result = loop {
        // relay control frame 必须先于业务输出处理，避免大量 PTY 输出让 Ping/Pong 被调度延迟。
        // 中文注释：writer outcome 可能在大输出期间形成热循环。它只能推动输出续传，
        // 不能排在 relay control 入站帧前面，否则 attach/input/disconnect 会被饿住。
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
                    last_control_activity = Instant::now();
                    traffic.record_in(&message);
                    heartbeat_debug
                        .record_inbound(relay_message_kind(&message), relay_message_bytes(&message));

                    match message {
                        Message::Text(raw) => {
                            let envelope: RelayMuxEnvelope = match serde_json::from_str(raw.as_str()) {
                                Ok(envelope) => envelope,
                                Err(_) => break Err(RelayConnectorError::InvalidEnvelope),
                            };
                            let client_id = relay_envelope_client_id(&envelope);
                            let responses = match handle_mux_envelope(envelope, &protocol, &mut connections).await {
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
                                }
                            }
                            if let Err(error) = enqueue_relay_mux_response(
                                &write_raw_control_tx,
                                &write_client_tx,
                                RelayOutKind::Response,
                                responses,
                            ) {
                                traffic.record_send_error();
                                break Err(error);
                            }
                            let initial_output_sessions = sync_relay_watchers_for_client(
                                client_id,
                                &connections,
                                &protocol,
                                &mut watched_output_sessions,
                                &mut watched_file_tree_sessions,
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
                            maybe_log_relay_traffic(
                                &relay_endpoint,
                                &mut traffic,
                                &mut last_traffic_log,
                                &mut connections,
                                relay_watcher_counts(
                                    &watched_output_sessions,
                                    &watched_file_tree_sessions,
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
                            let responses = match handle_mux_envelope(envelope, &protocol, &mut connections).await {
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
                                }
                            }
                            if let Err(error) = enqueue_relay_mux_response(
                                &write_raw_control_tx,
                                &write_client_tx,
                                RelayOutKind::Response,
                                responses,
                            ) {
                                traffic.record_send_error();
                                break Err(error);
                            }
                            let initial_output_sessions = sync_relay_watchers_for_client(
                                client_id,
                                &connections,
                                &protocol,
                                &mut watched_output_sessions,
                                &mut watched_file_tree_sessions,
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
                            maybe_log_relay_traffic(
                                &relay_endpoint,
                                &mut traffic,
                                &mut last_traffic_log,
                                &mut connections,
                                relay_watcher_counts(
                                    &watched_output_sessions,
                                    &watched_file_tree_sessions,
                                    &watched_resize_sessions,
                                ),
                                false,
                            );
                        }
                        Message::Ping(payload) => {
                            let pong_bytes = payload.len();
                            if let Err(error) = enqueue_relay_mux_control_message(
                                &write_raw_control_tx,
                                RelayOutKind::Pong,
                                Message::Pong(payload),
                                pong_bytes,
                            )
                            {
                                traffic.record_send_error();
                                break Err(error);
                            }
                            maybe_log_relay_traffic(
                                &relay_endpoint,
                                &mut traffic,
                                &mut last_traffic_log,
                                &mut connections,
                                relay_watcher_counts(
                                    &watched_output_sessions,
                                    &watched_file_tree_sessions,
                                    &watched_resize_sessions,
                                ),
                                false,
                            );
                        }
                        Message::Pong(_) => {
                            maybe_log_relay_traffic(
                                &relay_endpoint,
                                &mut traffic,
                                &mut last_traffic_log,
                                &mut connections,
                                relay_watcher_counts(
                                    &watched_output_sessions,
                                    &watched_file_tree_sessions,
                                    &watched_resize_sessions,
                                ),
                                false,
                            );
                        }
                        Message::Close(_) => break Ok(()),
                        Message::Frame(_) => {}
                    }
                }
                write_outcome = write_outcome_rx.recv() => {
                    let Some(write_outcome) = write_outcome else {
                        break Err(RelayConnectorError::SendFailed);
                    };
                    match write_outcome {
                        RelayMuxWriteOutcome::Sent { channel, kind, envelopes, bytes, push_events } => {
                            if channel == RelayMuxChannel::Control {
                                last_control_activity = Instant::now();
                                heartbeat_debug.record_outbound(kind.label(), bytes);
                            }
                            traffic.record_out(kind, envelopes, bytes);
                            pending_push_events.finish_inflight_after_send(&push_events);
                            queue_relay_push_drain_wakeup(
                                &pending_push_events,
                                &push_event_tx,
                                &mut push_drain_wake_pending,
                            );
                            maybe_log_relay_traffic(
                                &relay_endpoint,
                                &mut traffic,
                                &mut last_traffic_log,
                                &mut connections,
                                relay_watcher_counts(
                                    &watched_output_sessions,
                                    &watched_file_tree_sessions,
                                    &watched_resize_sessions,
                                ),
                                false,
                            );
                        }
                        RelayMuxWriteOutcome::Failed { channel, error } => {
                            warn!(
                                relay = %relay_endpoint,
                                channel = channel.label(),
                                %error,
                                "relay mux writer failed"
                            );
                            traffic.record_send_error();
                            break Err(error);
                        }
                    }
                }
                _ = heartbeat.tick(), if idle_ping_enabled => {
                    let now = Instant::now();
                    if last_control_activity.elapsed() >= heartbeat_interval
                        && now.duration_since(last_idle_ping_sent_at) >= heartbeat_interval
                    {
                        idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
                        let payload = idle_ping_nonce.to_be_bytes().to_vec();
                        let payload_len = payload.len();
                        match enqueue_relay_mux_control_message(
                            &write_raw_control_tx,
                            RelayOutKind::IdlePing,
                            Message::Ping(payload),
                            payload_len,
                        )
                        {
                            Ok(()) => {
                                last_idle_ping_sent_at = Instant::now();
                                // heartbeat 使用标准 WebSocket Ping，仅用于保活 daemon -> relay 长连接；
                                // relay 不解析它，也不把它纳入业务协议。
                            }
                            Err(error) => {
                                traffic.record_send_error();
                                break Err(error);
                            }
                        }
                    }
                }
        maybe_event = push_event_rx.recv() => {
                    let Some(event) = maybe_event else {
                        break Err(RelayConnectorError::SendFailed);
                    };
                    idle_deadline = Instant::now() + RELAY_IDLE_TIMEOUT;
                    push_drain_wake_pending = false;
                    pending_push_events.enqueue(event);
                    if let Err(error) = drain_relay_push_events(
                        &relay_endpoint,
                        server_id,
                        &protocol,
                        &mut connections,
                        &mut pending_push_events,
                        &write_client_tx,
                        &push_event_tx,
                        &mut push_drain_wake_pending,
                    )
                    .await
                    {
                        traffic.record_send_error();
                        break Err(error);
                    }
                    maybe_log_relay_traffic(
                        &relay_endpoint,
                        &mut traffic,
                        &mut last_traffic_log,
                        &mut connections,
                        relay_watcher_counts(
                            &watched_output_sessions,
                            &watched_file_tree_sessions,
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
            &watched_file_tree_sessions,
            &watched_resize_sessions,
        ),
        true,
    );

    abort_relay_watcher_tasks(watcher_tasks);
    control_writer_task.abort();
    data_task.abort();
    close_relay_connections(protocol, connections).await;
    result
}

fn relay_watcher_counts(
    watched_output_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
    watched_file_tree_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
    watched_resize_sessions: &HashMap<RelayClientId, HashSet<SessionId>>,
) -> RelayWatcherCounts {
    RelayWatcherCounts {
        output: watched_output_sessions.values().map(HashSet::len).sum(),
        file_tree: watched_file_tree_sessions.values().map(HashSet::len).sum(),
        resize: watched_resize_sessions.values().map(HashSet::len).sum(),
    }
}

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

fn relay_protocol_debug_traffic(
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
) -> ProtocolConnectionDebugTraffic {
    let mut traffic = ProtocolConnectionDebugTraffic::default();
    for connection in connections.values_mut() {
        traffic.merge(connection.take_debug_traffic());
    }
    traffic
}

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
    if relay_traffic_should_promote_to_info(
        traffic,
        &protocol_traffic,
        flow.zero_credit_terminal_streams,
    ) {
        info_relay_traffic(relay_endpoint, traffic, &protocol_traffic, watchers, flow);
    } else {
        debug_relay_traffic(relay_endpoint, traffic, &protocol_traffic, watchers, flow);
    }
    *traffic = RelayTrafficCounters::default();
    *last_logged_at = Instant::now();
}

fn relay_traffic_should_promote_to_info(
    traffic: &RelayTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    zero_credit_terminal_streams: usize,
) -> bool {
    // relay 是所有公网 client 共享的单条 daemon mux；只把异常放大、背压或发送错误提升到 info。
    traffic.send_errors > 0
        || traffic.out_push_output.calls > 1_000
        || traffic.out_response.calls > 1_000
        || protocol_traffic.inbound_flow_packets > 200
        || protocol_traffic.method_count_exceeds(20)
        || protocol_traffic.inbound_stream_chunks > 100
        || protocol_traffic.outbound_stream_chunks > 100
        || (zero_credit_terminal_streams > 0
            && (traffic.out_push_output.calls > 0 || traffic.out_response.calls > 0))
}

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
        watchers_file_tree = watchers.file_tree,
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

fn debug_relay_traffic(
    relay_endpoint: &str,
    traffic: &RelayTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    watchers: RelayWatcherCounts,
    flow: RelayMuxDebugSnapshot,
) {
    debug!(
        relay = relay_endpoint,
        ?traffic,
        ?protocol_traffic,
        watchers_output = watchers.output,
        watchers_file_tree = watchers.file_tree,
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

async fn connect_relay_mux_socket(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: ProtoNonce,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    let (socket, _) = connect_relay_websocket(url, proxy).await?;
    let (mut sender, mut receiver) = socket.split();
    send_route_hello(&mut sender, server_id, role, route_generation).await?;
    read_route_ready(&mut sender, &mut receiver, server_id, role).await?;
    Ok((sender, receiver))
}

async fn send_route_hello(
    sender: &mut RelaySender,
    server_id: ServerId,
    role: ProtoRouteRole,
    route_generation: ProtoNonce,
) -> Result<(), RelayConnectorError> {
    let envelope = ProtoEnvelope::new(
        ProtoMessageType::RouteHello,
        ProtoRouteHelloPayload {
            server_id,
            role,
            protocol_version: ProtoProtocolVersion(PROTOCOL_PACKET_VERSION),
            nonce: relay_route_nonce(),
            route_generation: Some(route_generation),
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

async fn handle_mux_envelope(
    envelope: RelayMuxEnvelope,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    match envelope {
        RelayMuxEnvelope::Keepalive { .. } | RelayMuxEnvelope::KeepaliveAck { .. } => {
            // 新模型下 mux keepalive 已退出业务协议；daemon 只用标准 WebSocket Ping 保活 relay 主干。
            Ok(Vec::new())
        }
        RelayMuxEnvelope::ClientConnected { client_id } => {
            let (connection, initial_messages) = {
                let protocol = protocol.lock().await;
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

            let frame = match wire_message_from_mux_frame(frame) {
                Ok(frame) => frame,
                Err(error) => {
                    // relay client 是非可信输入源；坏业务 frame 只能影响该 client，不能杀掉
                    // daemon outbound connector 或 direct daemon。
                    close_client_connection(protocol, connections, client_id).await;
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
                let mut protocol = protocol.lock().await;
                connection.handle_wire_message(&mut protocol, frame)
            };
            client_wire_messages(client_id, responses)
        }
        RelayMuxEnvelope::DaemonFrame { .. } => Err(RelayConnectorError::InvalidEnvelope),
    }
}

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
    watched_file_tree_sessions: &mut HashMap<RelayClientId, HashSet<SessionId>>,
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
            watched_file_tree_sessions,
            watched_resize_sessions,
            watcher_tasks,
        );
        return initial_output_sessions;
    };

    let (output_signals, file_tree_signals, resize_signals) = {
        let protocol = protocol.lock().await;
        (
            connection.attached_output_signals(&protocol),
            connection.attached_file_tree_signals(&protocol),
            connection.attached_resize_signals(&protocol),
        )
    };
    let desired_output_sessions: HashSet<_> = output_signals
        .iter()
        .map(|(session_id, _)| *session_id)
        .collect();
    let desired_file_tree_sessions: HashSet<_> = file_tree_signals
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
    let current_file_tree = watched_file_tree_sessions
        .get(&client_id)
        .cloned()
        .unwrap_or_default();
    let current_resize = watched_resize_sessions
        .get(&client_id)
        .cloned()
        .unwrap_or_default();
    if !current_output.is_subset(&desired_output_sessions)
        || !current_file_tree.is_subset(&desired_file_tree_sessions)
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
            watched_file_tree_sessions,
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
                    if push_event_tx
                        .send(RelayPushEvent::Output {
                            client_id,
                            session_id,
                        })
                        .await
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
                        .await
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
                        .await
                        .is_err()
                    {
                        break;
                    }
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
    }
}

fn queue_relay_deferred_output_wakeups(
    client_id: RelayClientId,
    connection: &mut ProtocolConnection,
    pending_push_events: &mut RelayPushEventQueue,
) {
    for session_id in connection.take_deferred_output_wakeups() {
        // 中文注释：flow ACK 只补 credit 并唤醒输出 drain；真正读取 PTY、组包和加密
        // 放到 push 队列里做，避免 relay client 的 ACK 风暴占住 daemon 全局协议锁。
        pending_push_events.enqueue(RelayPushEvent::Output {
            client_id,
            session_id,
        });
    }
}

fn enqueue_relay_mux_response(
    write_raw_control_tx: &mpsc::Sender<RelayMuxWrite>,
    write_client_tx: &mpsc::Sender<RelayMuxWrite>,
    kind: RelayOutKind,
    envelopes: Vec<RelayMuxEnvelope>,
) -> Result<(), RelayConnectorError> {
    if envelopes.is_empty() {
        return Ok(());
    }
    let contains_e2ee_payload = relay_mux_envelopes_contain_e2ee_payload(&envelopes);
    let write = RelayMuxWrite::Envelopes {
        kind,
        envelopes,
        push_events: Vec::new(),
    };
    if contains_e2ee_payload {
        // 中文注释：EncryptedFrame/binary payload 承载同一条 client E2EE sequence。
        // 它们必须和 terminal output 一起走 data FIFO，不能被 control socket 插队。
        write_client_tx
            .try_send(write)
            .map_err(|_| RelayConnectorError::SendFailed)
    } else {
        // 中文注释：hello / e2ee_key_exchange 还没有 E2EE sequence，可以继续走 control。
        // 这样新 client 的首包不会被其它 client 的大 terminal output 队列挡住。
        write_raw_control_tx
            .try_send(write)
            .map_err(|_| RelayConnectorError::SendFailed)
    }
}

fn relay_mux_envelopes_contain_e2ee_payload(envelopes: &[RelayMuxEnvelope]) -> bool {
    envelopes.iter().any(relay_mux_envelope_is_e2ee_payload)
}

fn relay_mux_envelope_is_e2ee_payload(envelope: &RelayMuxEnvelope) -> bool {
    match envelope {
        RelayMuxEnvelope::DaemonFrame {
            frame: RelayOpaqueFrame::Binary { .. },
            ..
        } => true,
        RelayMuxEnvelope::DaemonFrame {
            frame: RelayOpaqueFrame::Text { data },
            ..
        } => serde_json::from_str::<JsonEnvelope>(data)
            .is_ok_and(|envelope| envelope.kind == ProtoMessageType::EncryptedFrame),
        RelayMuxEnvelope::Keepalive { .. }
        | RelayMuxEnvelope::KeepaliveAck { .. }
        | RelayMuxEnvelope::ClientConnected { .. }
        | RelayMuxEnvelope::ClientDisconnected { .. }
        | RelayMuxEnvelope::ClientFrame { .. } => false,
    }
}

fn enqueue_relay_mux_control_message(
    write_control_tx: &mpsc::Sender<RelayMuxWrite>,
    kind: RelayOutKind,
    message: Message,
    bytes: usize,
) -> Result<(), RelayConnectorError> {
    match write_control_tx.try_send(RelayMuxWrite::Raw {
        kind,
        message,
        bytes,
    }) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) if kind == RelayOutKind::IdlePing => {
            // 中文注释：daemon idle Ping 只用于保活；control 队列满时说明连接不空闲，
            // 丢弃本次 Ping，不能把背压误判成 relay mux transport failure。
            Ok(())
        }
        Err(_) => Err(RelayConnectorError::SendFailed),
    }
}

async fn drain_relay_push_events(
    relay_endpoint: &str,
    server_id: ServerId,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    pending_push_events: &mut RelayPushEventQueue,
    write_client_tx: &mpsc::Sender<RelayMuxWrite>,
    push_event_tx: &mpsc::Sender<RelayPushEvent>,
    push_drain_wake_pending: &mut bool,
) -> Result<(), RelayConnectorError> {
    let started_at = Instant::now();
    let mut drained_events = 0_usize;
    let mut enqueued_bytes = 0_usize;
    while pending_push_events.has_pending() {
        let Some(event) = pending_push_events.pop_front_for_inflight() else {
            break;
        };
        let permit = match write_client_tx.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                pending_push_events.finish_inflight_after_send(&[event]);
                pending_push_events.enqueue(event);
                break;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                pending_push_events.finish_inflight_after_send(&[event]);
                pending_push_events.enqueue(event);
                return Err(RelayConnectorError::SendFailed);
            }
        };
        let (client_id, session_id, kind) = relay_push_event_parts(event);
        let Some(connection) = connections.get_mut(&client_id) else {
            pending_push_events.finish_inflight_after_send(&[event]);
            continue;
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
                    let messages = connection.collect_session_output_messages(
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
            RelayOutKind::PushFileTree => {
                let messages = {
                    let mut protocol = protocol.lock().await;
                    connection.read_session_file_tree_update_messages(&mut protocol, session_id)
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
            RelayOutKind::Response | RelayOutKind::IdlePing | RelayOutKind::Pong => Vec::new(),
        };
        queue_relay_deferred_output_wakeups(client_id, connection, pending_push_events);
        let response_count = responses.len();
        if response_count == 0 {
            pending_push_events.finish_inflight_after_send(&[event]);
            drained_events = drained_events.saturating_add(1);
            if relay_push_drain_budget_exhausted(drained_events, enqueued_bytes, started_at) {
                queue_relay_push_drain_wakeup(
                    pending_push_events,
                    push_event_tx,
                    push_drain_wake_pending,
                );
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
        permit.send(RelayMuxWrite::Envelopes {
            kind,
            envelopes,
            push_events: vec![event],
        });
        drained_events = drained_events.saturating_add(1);
        enqueued_bytes = enqueued_bytes.saturating_add(batch_bytes);
        if relay_push_drain_budget_exhausted(drained_events, enqueued_bytes, started_at) {
            // 中文注释：这里主动让 relay mux 主循环回到 select!，让输入、attach、disconnect
            // 和 Ping/Pong 有机会插队处理，避免多个大输出 session 把 control 面饿住。
            queue_relay_push_drain_wakeup(
                pending_push_events,
                push_event_tx,
                push_drain_wake_pending,
            );
            break;
        }
    }
    Ok(())
}

fn queue_relay_push_drain_wakeup(
    pending_push_events: &RelayPushEventQueue,
    push_event_tx: &mpsc::Sender<RelayPushEvent>,
    push_drain_wake_pending: &mut bool,
) {
    if *push_drain_wake_pending || !pending_push_events.has_pending() {
        return;
    }
    let Some(event) = pending_push_events.peek_front() else {
        return;
    };
    if push_event_tx.try_send(event).is_ok() {
        *push_drain_wake_pending = true;
    }
}

fn relay_push_drain_budget_exhausted(
    drained_events: usize,
    enqueued_bytes: usize,
    started_at: Instant,
) -> bool {
    drained_events >= RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK
        || enqueued_bytes >= RELAY_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK
        || started_at.elapsed() >= RELAY_PUSH_DRAIN_MAX_ELAPSED
}

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
        RelayPushEvent::FileTree {
            client_id,
            session_id,
        } => (client_id, session_id, RelayOutKind::PushFileTree),
        RelayPushEvent::Resize {
            client_id,
            session_id,
        } => (client_id, session_id, RelayOutKind::PushResize),
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

async fn run_relay_mux_writer(
    relay_endpoint: String,
    channel: RelayMuxChannel,
    mut sender: RelaySender,
    mut rx: mpsc::Receiver<RelayMuxWrite>,
    outcome_tx: mpsc::Sender<RelayMuxWriteOutcome>,
) {
    while let Some(write) = rx.recv().await {
        if !send_relay_mux_write(&relay_endpoint, channel, &mut sender, write, &outcome_tx).await {
            break;
        }
    }
}

async fn run_relay_mux_data_task(
    relay_endpoint: String,
    mut sender: RelaySender,
    mut receiver: RelayReceiver,
    mut rx: mpsc::Receiver<RelayMuxWrite>,
    outcome_tx: mpsc::Sender<RelayMuxWriteOutcome>,
    heartbeat_interval: Duration,
) {
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_data_activity = Instant::now();
    let mut last_idle_ping_sent_at = Instant::now();
    let mut idle_ping_nonce: u64 = 0;
    let mut prefer_inbound_once = false;

    loop {
        if prefer_inbound_once {
            tokio::select! {
                biased;

                inbound = receiver.next() => {
                    prefer_inbound_once = false;
                    if !handle_relay_mux_data_inbound(&relay_endpoint, &mut sender, inbound, &outcome_tx, &mut last_data_activity).await {
                        break;
                    }
                }
                write = rx.recv() => {
                    prefer_inbound_once = true;
                    let Some(write) = write else {
                        break;
                    };
                    if !send_relay_mux_write(
                        &relay_endpoint,
                        RelayMuxChannel::Data,
                        &mut sender,
                        write,
                        &outcome_tx,
                    )
                    .await
                    {
                        break;
                    }
                    last_data_activity = Instant::now();
                }
                _ = heartbeat.tick() => {
                    let now = Instant::now();
                    if last_data_activity.elapsed() >= heartbeat_interval
                        && now.duration_since(last_idle_ping_sent_at) >= heartbeat_interval
                    {
                        idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
                        let payload = idle_ping_nonce.to_be_bytes().to_vec();
                        let bytes = payload.len();
                        let write = RelayMuxWrite::Raw {
                            kind: RelayOutKind::IdlePing,
                            message: Message::Ping(payload),
                            bytes,
                        };
                        if !send_relay_mux_write(
                            &relay_endpoint,
                            RelayMuxChannel::Data,
                            &mut sender,
                            write,
                            &outcome_tx,
                        )
                        .await
                        {
                            break;
                        }
                        last_idle_ping_sent_at = Instant::now();
                        last_data_activity = Instant::now();
                    }
                }
            }
            continue;
        }

        tokio::select! {
            biased;

            write = rx.recv() => {
                let Some(write) = write else {
                    break;
                };
                if !send_relay_mux_write(
                    &relay_endpoint,
                    RelayMuxChannel::Data,
                    &mut sender,
                    write,
                    &outcome_tx,
                )
                .await
                {
                    break;
                }
                last_data_activity = Instant::now();
                prefer_inbound_once = true;
            }
            inbound = receiver.next() => {
                prefer_inbound_once = false;
                if !handle_relay_mux_data_inbound(&relay_endpoint, &mut sender, inbound, &outcome_tx, &mut last_data_activity).await {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                let now = Instant::now();
                if last_data_activity.elapsed() >= heartbeat_interval
                    && now.duration_since(last_idle_ping_sent_at) >= heartbeat_interval
                {
                    idle_ping_nonce = idle_ping_nonce.wrapping_add(1);
                    let payload = idle_ping_nonce.to_be_bytes().to_vec();
                    let bytes = payload.len();
                    let write = RelayMuxWrite::Raw {
                        kind: RelayOutKind::IdlePing,
                        message: Message::Ping(payload),
                        bytes,
                    };
                    if !send_relay_mux_write(
                        &relay_endpoint,
                        RelayMuxChannel::Data,
                        &mut sender,
                        write,
                        &outcome_tx,
                    )
                    .await
                    {
                        break;
                    }
                    last_idle_ping_sent_at = Instant::now();
                    last_data_activity = Instant::now();
                }
            }
        }
    }
}

async fn send_relay_mux_write(
    relay_endpoint: &str,
    channel: RelayMuxChannel,
    sender: &mut RelaySender,
    write: RelayMuxWrite,
    outcome_tx: &mpsc::Sender<RelayMuxWriteOutcome>,
) -> bool {
    let outcome = match write {
        RelayMuxWrite::Envelopes {
            kind,
            envelopes,
            push_events,
        } => {
            let envelope_count = envelopes.len();
            match send_mux_envelopes_logged(relay_endpoint, sender, envelopes, kind.label()).await {
                Ok(bytes) => RelayMuxWriteOutcome::Sent {
                    channel,
                    kind,
                    envelopes: envelope_count,
                    bytes,
                    push_events,
                },
                Err(error) => RelayMuxWriteOutcome::Failed { channel, error },
            }
        }
        RelayMuxWrite::Raw {
            kind,
            message,
            bytes,
        } => {
            match send_relay_message_with_deadline(sender, message, relay_send_deadline(kind)).await
            {
                Ok(()) => RelayMuxWriteOutcome::Sent {
                    channel,
                    kind,
                    envelopes: 0,
                    bytes,
                    push_events: Vec::new(),
                },
                Err(error) => RelayMuxWriteOutcome::Failed { channel, error },
            }
        }
    };
    let should_continue = matches!(outcome, RelayMuxWriteOutcome::Sent { .. });
    if outcome_tx.send(outcome).await.is_err() {
        return false;
    }
    should_continue
}

async fn handle_relay_mux_data_inbound(
    relay_endpoint: &str,
    sender: &mut RelaySender,
    inbound: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    outcome_tx: &mpsc::Sender<RelayMuxWriteOutcome>,
    last_data_activity: &mut Instant,
) -> bool {
    let Some(message) = inbound else {
        let _ = outcome_tx.try_send(RelayMuxWriteOutcome::Failed {
            channel: RelayMuxChannel::Data,
            error: RelayConnectorError::ReceiveFailed,
        });
        return false;
    };
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            warn!(
                relay = %relay_endpoint,
                %error,
                "relay data mux receive failed"
            );
            let _ = outcome_tx.try_send(RelayMuxWriteOutcome::Failed {
                channel: RelayMuxChannel::Data,
                error: RelayConnectorError::ReceiveFailed,
            });
            return false;
        }
    };
    *last_data_activity = Instant::now();
    match message {
        Message::Ping(payload) => {
            let payload_len = payload.len();
            let write = RelayMuxWrite::Raw {
                kind: RelayOutKind::Pong,
                message: Message::Pong(payload),
                bytes: payload_len,
            };
            if !send_relay_mux_write(
                relay_endpoint,
                RelayMuxChannel::Data,
                sender,
                write,
                outcome_tx,
            )
            .await
            {
                return false;
            };
            *last_data_activity = Instant::now();
        }
        Message::Pong(_) | Message::Frame(_) => {}
        Message::Close(_) => {
            let _ = outcome_tx.try_send(RelayMuxWriteOutcome::Failed {
                channel: RelayMuxChannel::Data,
                error: RelayConnectorError::ReceiveFailed,
            });
            return false;
        }
        Message::Text(_) | Message::Binary(_) => {
            warn!(
                relay = %relay_endpoint,
                kind = relay_message_kind(&message),
                bytes = relay_message_bytes(&message),
                "relay data mux received unexpected business frame"
            );
            let _ = outcome_tx.try_send(RelayMuxWriteOutcome::Failed {
                channel: RelayMuxChannel::Data,
                error: RelayConnectorError::InvalidEnvelope,
            });
            return false;
        }
    }
    true
}

fn relay_send_deadline(kind: RelayOutKind) -> Duration {
    match kind {
        RelayOutKind::Pong => RELAY_PONG_DEADLINE,
        _ => RELAY_SEND_DEADLINE,
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

async fn send_mux_envelopes_logged(
    relay_endpoint: &str,
    sender: &mut RelaySender,
    envelopes: Vec<RelayMuxEnvelope>,
    label: &'static str,
) -> Result<usize, RelayConnectorError> {
    let envelope_count = envelopes.len();
    let mut bytes = 0_usize;
    let started_at = Instant::now();
    for envelope in envelopes {
        let raw = encode_binary_relay_mux_envelope(&envelope)
            .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
        bytes = bytes.saturating_add(raw.len());
        send_relay_message_with_deadline(sender, Message::Binary(raw.into()), RELAY_SEND_DEADLINE)
            .await?;
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

async fn send_relay_message_with_deadline(
    sender: &mut RelaySender,
    message: Message,
    deadline: Duration,
) -> Result<(), RelayConnectorError> {
    match timeout(deadline, sender.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(RelayConnectorError::SendFailed),
        Err(_) => Err(RelayConnectorError::SendTimeout),
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
            RELAY_CONNECT_TIMEOUT,
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
    let stream = timeout(RELAY_CONNECT_TIMEOUT, connect_proxy_tunnel(proxy, &target))
        .await
        .map_err(|_| RelayConnectorError::ConnectTimeout)?
        .map_err(|_| RelayConnectorError::ConnectFailed)?;

    timeout(
        RELAY_CONNECT_TIMEOUT,
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

        assert_eq!(queue.pop_front_for_inflight(), Some(event));
        assert_eq!(queue.pop_front_for_inflight(), None);
    }

    #[test]
    fn relay_push_queue_requeues_dirty_inflight_once_after_send() {
        let client_id = RelayClientId(1);
        let session_id = SessionId::new();
        let event = RelayPushEvent::Output {
            client_id,
            session_id,
        };
        let mut queue = RelayPushEventQueue::default();

        queue.enqueue(event);
        assert_eq!(queue.pop_front_for_inflight(), Some(event));
        queue.enqueue(event);
        queue.enqueue(event);
        queue.finish_inflight_after_send(&[event]);

        assert_eq!(queue.pop_front_for_inflight(), Some(event));
        assert_eq!(queue.pop_front_for_inflight(), None);
    }

    #[test]
    fn relay_push_wakeup_peeks_without_draining_after_writer_outcome() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();
        let (tx, mut rx) = mpsc::channel(1);
        let mut wake_pending = false;

        queue.enqueue(event);
        assert_eq!(queue.pop_front_for_inflight(), Some(event));
        queue.enqueue(event);
        queue.finish_inflight_after_send(&[event]);
        queue_relay_push_drain_wakeup(&queue, &tx, &mut wake_pending);

        // 中文注释：relay writer outcome 只投递一次普通 push 事件；不能同步继续
        // drain，否则 output writer 会形成自激循环，把 control 入站帧压在后面。
        assert_eq!(queue.peek_front(), Some(event));
        assert_eq!(rx.try_recv(), Ok(event));
        assert!(wake_pending);
    }

    #[test]
    fn relay_push_queue_peek_front_does_not_dequeue() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();

        queue.enqueue(event);

        assert_eq!(queue.peek_front(), Some(event));
        assert_eq!(queue.pop_front_for_inflight(), Some(event));
    }

    #[test]
    fn relay_push_drain_budget_limits_hot_loop() {
        assert!(!relay_push_drain_budget_exhausted(1, 1024, Instant::now()));
        assert!(relay_push_drain_budget_exhausted(
            RELAY_PUSH_DRAIN_MAX_EVENTS_PER_TICK,
            0,
            Instant::now()
        ));
        assert!(relay_push_drain_budget_exhausted(
            1,
            RELAY_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK,
            Instant::now()
        ));
        assert!(relay_push_drain_budget_exhausted(
            1,
            0,
            Instant::now() - RELAY_PUSH_DRAIN_MAX_ELAPSED
        ));
    }

    #[test]
    fn relay_push_drain_wakeup_is_queued_once_while_pending() {
        let event = RelayPushEvent::Output {
            client_id: RelayClientId(1),
            session_id: SessionId::new(),
        };
        let mut queue = RelayPushEventQueue::default();
        queue.enqueue(event);
        let (tx, mut rx) = mpsc::channel(2);
        let mut pending = false;

        queue_relay_push_drain_wakeup(&queue, &tx, &mut pending);
        queue_relay_push_drain_wakeup(&queue, &tx, &mut pending);

        assert!(pending);
        assert_eq!(rx.try_recv().unwrap(), event);
        assert!(rx.try_recv().is_err());
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

    #[test]
    fn relay_mux_client_frame_queue_preserves_e2ee_sequence_order_across_kinds() {
        let client_id = RelayClientId(7);
        let (control_tx, mut control_rx) = mpsc::channel(4);
        let (client_tx, mut client_rx) = mpsc::channel(4);
        let seq3 = RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "Aw==".to_owned(),
            },
        };
        let seq4 = RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "BA==".to_owned(),
            },
        };

        client_tx
            .try_send(RelayMuxWrite::Envelopes {
                kind: RelayOutKind::PushOutput,
                envelopes: vec![seq3.clone()],
                push_events: Vec::new(),
            })
            .unwrap();
        enqueue_relay_mux_response(
            &control_tx,
            &client_tx,
            RelayOutKind::Response,
            vec![seq4.clone()],
        )
        .unwrap();

        // 中文注释：同一 client 的 DaemonFrame 承载同一条 E2EE sequence。
        // output 和 response 必须在 relay data 队列里保持加密后的 FIFO 顺序。
        assert!(control_rx.try_recv().is_err());
        let first = client_rx.try_recv().unwrap();
        let second = client_rx.try_recv().unwrap();
        match first {
            RelayMuxWrite::Envelopes {
                kind, envelopes, ..
            } => {
                assert_eq!(kind, RelayOutKind::PushOutput);
                assert_eq!(envelopes, vec![seq3]);
            }
            other => panic!("expected first client frame batch, got {other:?}"),
        }
        match second {
            RelayMuxWrite::Envelopes {
                kind, envelopes, ..
            } => {
                assert_eq!(kind, RelayOutKind::Response);
                assert_eq!(envelopes, vec![seq4]);
            }
            other => panic!("expected second client frame batch, got {other:?}"),
        }
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
    fn relay_daemon_mux_uses_websocket_ping_without_pong_deadline() {
        assert_eq!(
            RelayReconnectPolicy::default().heartbeat_interval(),
            Duration::from_secs(5)
        );
        assert!(relay_daemon_mux_idle_ping_enabled());
    }

    #[test]
    fn relay_traffic_ignores_empty_output_pushes() {
        let mut traffic = RelayTrafficCounters::default();

        traffic.record_out(RelayOutKind::PushOutput, 0, 0);

        assert!(!traffic.has_activity());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_supervisor_retries_after_close_and_keeps_mux_alive_with_idle_ping() {
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
                if state.attempts.load(Ordering::SeqCst) >= 3 {
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
            "daemon mux 空闲时应发送 WebSocket Ping，避免公网代理清理静默主干"
        );
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            3,
            "首个 control socket 故意失败后，稳定连接应只包含 control/data 两条 WebSocket"
        );

        connector.abort();
        server.abort();
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
            let (mut socket, _data_socket) = accept_relay_mux_pair(&listener, server_id).await;

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
            if !matches!(
                route_hello.role,
                RouteRole::DaemonMux | RouteRole::DaemonMuxData
            ) {
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
                    Ok(AxumMessage::Binary(_)) => {}
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

        let keepalive = handle_mux_envelope(
            RelayMuxEnvelope::Keepalive { nonce: 42 },
            &protocol,
            &mut connections,
        )
        .await
        .unwrap();
        let keepalive_ack = handle_mux_envelope(
            RelayMuxEnvelope::KeepaliveAck { nonce: 42 },
            &protocol,
            &mut connections,
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

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected { client_id },
            &protocol,
            &mut connections,
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

        handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: bad_client_id,
            },
            &protocol,
            &mut connections,
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
        )
        .await;

        assert_eq!(pair_response.kind, MessageType::PairAccept);
        assert!(connections.contains_key(&good_client_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_mux_pushes_session_output_without_client_poll_frame() {
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
        let (mut relay_socket, mut data_socket) =
            accept_relay_mux_pair(&listener, expected_server_id).await;

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
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee).await;
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
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee).await;
        assert_eq!(created.kind, MessageType::SessionCreated);
        let created_payload: termd_proto::SessionCreatedPayload =
            decode_payload(created.payload).unwrap();

        // relay client 不再发送 ping 或任何业务帧；daemon mux 必须像直连 WebSocket 一样主动推送。
        let pushed = tokio::time::timeout(
            Duration::from_secs(2),
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee),
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
        let (mut relay_socket, mut data_socket) =
            accept_relay_mux_pair(&listener, expected_server_id).await;

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
            read_encrypted_daemon_frame(&mut data_socket, slow_client_id, &mut slow_e2ee).await;
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
            read_encrypted_daemon_frame(&mut data_socket, slow_client_id, &mut slow_e2ee).await;
        assert_eq!(created.kind, MessageType::SessionCreated);

        // 先读到第一批 output，证明 slow client 已经触发大量推送；随后故意不再
        // 消费它的剩余 output，模拟公网 relay/web 侧慢写。
        let first_output =
            read_encrypted_daemon_frame(&mut data_socket, slow_client_id, &mut slow_e2ee).await;
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
        let connector = tokio::spawn(connect_relay_mux_base_once(
            base,
            None,
            None,
            protocol.clone(),
            RELAY_HEARTBEAT_INTERVAL,
        ));
        let client_id = RelayClientId(78);
        let expected_server_id = protocol.lock().await.server_id();
        let (mut relay_socket, mut data_socket) =
            accept_relay_mux_pair(&listener, expected_server_id).await;

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
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee).await;
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
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee).await;
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
            read_encrypted_daemon_frame(&mut data_socket, client_id, &mut device_e2ee).await;
        assert_eq!(initial_files.kind, MessageType::SessionFilesResult);

        let mut direct_protocol = protocol.lock().await;
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
                termd_proto::E2eeKeyExchangePayload::new(
                    direct_server_key_exchange.server_id,
                    direct_device_id,
                    direct_keypair.public_key_wire(),
                    termd_proto::Nonce("direct-file-tree-e2ee-nonce".to_owned()),
                    current_unix_timestamp_millis(),
                ),
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
                        watch_updates: true,
                        last_terminal_seq: None,
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
                &mut data_socket,
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

    async fn complete_pairing_via_mux(
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
    ) -> (
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let (control_tcp, _) = listener.accept().await.unwrap();
        let mut control_socket = tokio_tungstenite::accept_async(control_tcp).await.unwrap();
        complete_relay_route_prelude(
            &mut control_socket,
            expected_server_id,
            RouteRole::DaemonMux,
        )
        .await;

        let (data_tcp, _) = listener.accept().await.unwrap();
        let mut data_socket = tokio_tungstenite::accept_async(data_tcp).await.unwrap();
        complete_relay_route_prelude(
            &mut data_socket,
            expected_server_id,
            RouteRole::DaemonMuxData,
        )
        .await;

        (control_socket, data_socket)
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

        (device_id, device_e2ee)
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
