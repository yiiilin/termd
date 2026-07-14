//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName};
use futures_util::{Sink, SinkExt, StreamExt};
use rustls::{ClientConfig, RootCertStore};
use termd_proto::{
    Envelope as ProtoEnvelope, MessageType as ProtoMessageType, Nonce as ProtoNonce,
    PROTOCOL_PACKET_VERSION, ProtocolVersion as ProtoProtocolVersion,
    RelayAdmissionPayload as ProtoRelayAdmissionPayload, RelayClientId, RelayControlEnvelope,
    RelayHttpTunnelFrame, RelayRouteKind, RouteHelloPayload as ProtoRouteHelloPayload,
    RouteReadyPayload as ProtoRouteReadyPayload, RouteRole as ProtoRouteRole, ServerId, SessionId,
    decode_relay_data_control, decode_relay_http_tunnel_frame,
    encode_relay_http_tunnel_response_body, encode_relay_http_tunnel_response_end,
    encode_relay_http_tunnel_response_head_with_headers,
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
use tracing::{debug, trace, warn};

use super::protocol::{JsonEnvelope, ProtocolConnection, ProtocolWireMessage, V070TerminalOpen};
use super::server::{SharedDaemonProtocol, handle_http_tunnel_stream_request};
use crate::auth::current_unix_timestamp_millis;
use crate::config::RelayReconnectConfig;

const MIN_RELAY_RETRY_DELAY_MS: u64 = 1;
const MIN_RELAY_HEARTBEAT_INTERVAL_MS: u64 = 1;
const MAX_METADATA_TIMESTAMP_MS: u64 = 9_007_199_254_740_991;
// relay mux transport 失败只会断开当前 relay 连接并触发重连，不关闭持久 session/supervisor。
// 公网 relay 往往还隔着 TLS 和反向代理，2s 级 deadline 容易把短暂抖动误判成断线。
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_ROUTE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_SEND_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const RELAY_PONG_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
const RELAY_PONG_DEADLINE: Duration = Duration::from_millis(50);
const RELAY_RECONNECT_STABLE_RESET_AFTER: Duration = Duration::from_secs(60);
// relay 是 trusted routing 层，但 daemon 侧仍不能依赖 relay 解析终端业务分片。
// 允许 MB 级 terminal snapshot 通过，同时保留明确的单帧/单消息内存上限。
const RELAY_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const RELAY_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
// 中文注释：daemon->relay data writer 只保留一个待写批次，让 WebSocket 写速率成为真实背压。
// HTTP tunnel 下载不能在 daemon 侧按 256KiB * 2048 继续堆积。
const RELAY_DATA_WIRE_QUEUE_CAPACITY: usize = 1;

type RelayWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
type RelaySender = futures_util::stream::SplitSink<RelayWs, Message>;
type RelayReceiver = futures_util::stream::SplitStream<RelayWs>;
type RelayDataTaskMap = HashMap<RelayClientId, JoinHandle<()>>;

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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct RelayTransportDebugSnapshot {
    since_last_inbound_ms: u64,
    since_last_outbound_ms: u64,
    last_inbound_kind: &'static str,
    last_outbound_kind: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayOutKind {
    Response,
    FileTunnelBody,
    PushOutput,
    Pong,
}

impl RelayOutKind {
    fn label(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::FileTunnelBody => "file_tunnel_body",
            Self::PushOutput => "push_output",
            Self::Pong => "pong",
        }
    }
}

#[derive(Debug)]
enum RelayDataWrite {
    Raw {
        kind: RelayOutKind,
        message: Message,
    },
}

impl RelayDataWrite {
    fn debug_snapshot(&self) -> RelayMuxWriteDebugSnapshot {
        match self {
            Self::Raw { kind, message } => RelayMuxWriteDebugSnapshot {
                kind: *kind,
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
    envelopes: usize,
    bytes: usize,
    raw: bool,
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
    #[error("trusted relay device registration failed")]
    RelayRegistrationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayRouteConnectPhase {
    TcpConnect,
    RouteHello,
    RouteReady,
}

impl RelayRouteConnectPhase {
    fn label(self) -> &'static str {
        match self {
            Self::TcpConnect => "tcp_connect",
            Self::RouteHello => "route_hello",
            Self::RouteReady => "route_ready",
        }
    }

    fn timeout_ms(self, connect_timeout: Duration) -> u64 {
        match self {
            Self::TcpConnect => connect_timeout.as_millis() as u64,
            Self::RouteHello => RELAY_SEND_DEADLINE.as_millis() as u64,
            Self::RouteReady => RELAY_ROUTE_READY_TIMEOUT.as_millis() as u64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelayRouteConnectProgress {
    phase: RelayRouteConnectPhase,
}

impl RelayRouteConnectProgress {
    fn new() -> Self {
        Self {
            phase: RelayRouteConnectPhase::TcpConnect,
        }
    }

    fn phase_label(self) -> &'static str {
        self.phase.label()
    }

    fn timeout_ms(self, connect_timeout: Duration) -> u64 {
        self.phase.timeout_ms(connect_timeout)
    }

    fn mark_tcp_connected(&mut self) {
        self.phase = RelayRouteConnectPhase::RouteHello;
    }

    fn mark_route_hello_sent(&mut self) {
        self.phase = RelayRouteConnectPhase::RouteReady;
    }
}

fn relay_route_log_url(url: &str) -> &str {
    url.split_once('?').map_or(url, |(base, _)| base)
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

    pub fn client_url_template(&self) -> String {
        self.unified_ws_url()
    }

    pub fn api_url(&self, api_path: &str) -> String {
        let scheme = match self.scheme {
            RelayUrlScheme::Ws => "http",
            RelayUrlScheme::Wss => "https",
        };
        let prefix = self.base_path.api_prefix();
        format!("{scheme}://{}{}{}", self.authority, prefix, api_path)
    }

    fn unified_ws_url(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme.as_str(),
            self.authority,
            self.base_path.endpoint_suffix()
        )
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

    fn api_prefix(&self) -> &str {
        self.canonical_suffix
            .strip_suffix("/ws")
            .unwrap_or(self.canonical_suffix.as_str())
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
    connect_relay_mux_with_admission(relay_url, None, protocol).await
}

pub async fn connect_relay_mux_with_admission(
    relay_url: &str,
    daemon_admission_token: Option<&str>,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    connect_relay_mux_base(base, daemon_admission_token, protocol).await
}

pub async fn connect_relay_mux_base(
    base: RelayBaseUrl,
    daemon_admission_token: Option<&str>,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    connect_relay_control_base_once(
        base,
        daemon_admission_token,
        None,
        protocol,
        RelayReconnectPolicy::default().heartbeat_interval(),
    )
    .await
}

pub async fn run_relay_mux_with_reconnect(
    relay_url: &str,
    daemon_admission_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    run_relay_mux_with_reconnect_base(base, daemon_admission_token, proxy, policy, protocol).await
}

pub async fn run_relay_mux_with_reconnect_base(
    base: RelayBaseUrl,
    daemon_admission_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    policy: RelayReconnectPolicy,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let mut retry_delay = policy.first_retry_delay();

    loop {
        let attempt_started_at = Instant::now();
        let result = connect_relay_control_base_once(
            base.clone(),
            daemon_admission_token,
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
    daemon_admission_token: Option<&str>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    heartbeat_interval: Duration,
) -> Result<(), RelayConnectorError> {
    let server_id = { protocol.lock().await.server_id() };
    let relay_endpoint = base.canonical_url();
    let url = base.daemon_mux_url(server_id);
    // 中文注释：同一条 daemon control 生命周期内派生出的 data pipe 必须共享同一个
    // route_generation；relay 依赖它拒绝上一代 control 派生出的迟到 data 回连。
    let route_generation = relay_route_nonce();
    let (mut sender, mut receiver) = connect_relay_route_socket(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonControl,
        daemon_admission_token,
        Some(route_generation.clone()),
        None,
        None,
    )
    .await?;
    // 中文注释：trusted relay 的 device admission 表是 relay 进程内缓存；
    // relay 升级/重启后必须由 daemon 用本地持久 trusted devices 重新声明，
    // 否则已配对 Web 页面重连时会因为 relay 不认识 device 公钥而被拒绝。
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_activity = Instant::now();
    let mut last_idle_ping_sent_at = Instant::now();
    let mut idle_ping_nonce = 0_u64;
    let mut data_tasks = RelayDataTaskMap::new();

    let result = loop {
        prune_finished_relay_data_tasks(&mut data_tasks);
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
                            daemon_admission_token.map(str::to_owned),
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
                            daemon_admission_token.map(str::to_owned),
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
        }
    };

    let aborted = abort_all_relay_data_tasks(&mut data_tasks);
    if aborted > 0 {
        debug!(
            relay = %relay_endpoint,
            ?server_id,
            aborted,
            "relay daemon control aborted data pipes on shutdown"
        );
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_relay_control_envelope(
    envelope: RelayControlEnvelope,
    base: RelayBaseUrl,
    daemon_admission_token: Option<String>,
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
            route_kind,
            access_token,
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
                    daemon_admission_token,
                    proxy,
                    protocol,
                    server_id,
                    route_generation,
                    client_id,
                    data_token,
                    route_kind,
                    access_token,
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

#[allow(clippy::too_many_arguments)]
async fn run_relay_data_connection(
    base: RelayBaseUrl,
    daemon_admission_token: Option<String>,
    proxy: Option<RelayProxyUrl>,
    protocol: SharedDaemonProtocol,
    server_id: ServerId,
    route_generation: ProtoNonce,
    client_id: RelayClientId,
    data_token: ProtoNonce,
    route_kind: RelayRouteKind,
    access_token: Option<String>,
) -> Result<(), RelayConnectorError> {
    let relay_endpoint = base.canonical_url();
    let url = base.daemon_mux_url(server_id);
    let (sender, mut receiver) = connect_relay_route_socket_with_timeout(
        &url,
        proxy.as_ref(),
        server_id,
        ProtoRouteRole::DaemonData,
        daemon_admission_token.as_deref(),
        Some(route_generation),
        Some(client_id),
        Some(data_token),
        RELAY_DATA_CONNECT_TIMEOUT,
    )
    .await?;
    let outcome = match route_kind {
        RelayRouteKind::Metadata | RelayRouteKind::Terminal => {
            run_relay_v070_data_connection(
                relay_endpoint,
                protocol,
                client_id,
                sender,
                &mut receiver,
                route_kind,
                access_token,
            )
            .await?
        }
        RelayRouteKind::Http => {
            run_relay_http_data_connection(
                relay_endpoint,
                protocol,
                client_id,
                sender,
                &mut receiver,
            )
            .await?
        }
        RelayRouteKind::Legacy => return Err(RelayConnectorError::InvalidEnvelope),
    };
    let _ = outcome;
    Ok(())
}

async fn run_relay_http_data_connection(
    relay_endpoint: String,
    protocol: SharedDaemonProtocol,
    client_id: RelayClientId,
    sender: RelaySender,
    receiver: &mut RelayReceiver,
) -> Result<RelayEstablishedDataOutcome, RelayConnectorError> {
    let first = receiver
        .next()
        .await
        .ok_or(RelayConnectorError::ReceiveFailed)?
        .map_err(|_| RelayConnectorError::ReceiveFailed)?;
    let Message::Binary(raw) = first else {
        return Err(RelayConnectorError::InvalidEnvelope);
    };
    let Some(RelayHttpTunnelFrame::RequestHead {
        method,
        path,
        headers,
    }) = decode_relay_http_tunnel_frame(&raw)
    else {
        return Err(RelayConnectorError::InvalidEnvelope);
    };
    let (write_tx, write_rx) = mpsc::channel::<RelayDataWrite>(RELAY_DATA_WIRE_QUEUE_CAPACITY);
    let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel::<()>(1);
    let (_writer_stop_tx, writer_stop_rx) = oneshot::channel();
    let writer_task = tokio::spawn(run_relay_data_writer(
        relay_endpoint.clone(),
        client_id,
        write_rx,
        writer_failed_tx,
        sender,
        writer_stop_rx,
    ));
    let result = handle_relay_http_tunnel_stream(
        &relay_endpoint,
        protocol,
        client_id,
        method,
        path,
        headers,
        &write_tx,
        receiver,
        &mut writer_failed_rx,
    )
    .await;
    drop(write_tx);
    writer_task.abort();
    let _ = writer_task.await;
    result?;
    Ok(RelayEstablishedDataOutcome::SocketClosed)
}

async fn enqueue_v070_json(
    write_tx: &mpsc::Sender<RelayDataWrite>,
    kind: &str,
    payload: serde_json::Value,
) -> Result<(), RelayConnectorError> {
    let raw = serde_json::json!({"type": kind, "payload": payload}).to_string();
    enqueue_relay_data_raw(write_tx, RelayOutKind::Response, Message::Text(raw)).await
}

async fn run_relay_v070_data_connection(
    relay_endpoint: String,
    protocol: SharedDaemonProtocol,
    client_id: RelayClientId,
    sender: RelaySender,
    receiver: &mut RelayReceiver,
    route_kind: RelayRouteKind,
    access_token: Option<String>,
) -> Result<RelayEstablishedDataOutcome, RelayConnectorError> {
    let token = access_token.ok_or(RelayConnectorError::InvalidEnvelope)?;
    let device_id = protocol
        .lock()
        .await
        .verify_access_token_credential(&token, current_unix_timestamp_millis())
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    let (write_tx, write_rx) = mpsc::channel::<RelayDataWrite>(RELAY_DATA_WIRE_QUEUE_CAPACITY);
    let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel::<()>(1);
    let (_writer_stop_tx, writer_stop_rx) = oneshot::channel();
    let writer_task = tokio::spawn(run_relay_data_writer(
        relay_endpoint,
        client_id,
        write_rx,
        writer_failed_tx,
        sender,
        writer_stop_rx,
    ));

    let result = match route_kind {
        RelayRouteKind::Metadata => {
            run_relay_v070_metadata(
                protocol,
                device_id,
                receiver,
                &write_tx,
                &mut writer_failed_rx,
            )
            .await
        }
        RelayRouteKind::Terminal => {
            run_relay_v070_terminal(
                protocol,
                device_id,
                receiver,
                &write_tx,
                &mut writer_failed_rx,
            )
            .await
        }
        RelayRouteKind::Legacy | RelayRouteKind::Http => Err(RelayConnectorError::InvalidEnvelope),
    };
    drop(write_tx);
    writer_task.abort();
    let _ = writer_task.await;
    result?;
    Ok(RelayEstablishedDataOutcome::SocketClosed)
}

async fn run_relay_v070_metadata(
    protocol: SharedDaemonProtocol,
    device_id: termd_proto::DeviceId,
    receiver: &mut RelayReceiver,
    write_tx: &mpsc::Sender<RelayDataWrite>,
    writer_failed_rx: &mut mpsc::Receiver<()>,
) -> Result<(), RelayConnectorError> {
    let mut revision = 1_u64;
    let (mut changes, mut previous) = {
        let mut guard = protocol.lock().await;
        let changes = guard.v070_metadata_signal();
        let payload = guard
            .v070_metadata_payload(device_id)
            .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
        (changes, payload)
    };
    enqueue_v070_json(
        write_tx,
        "metadata.snapshot",
        serde_json::json!({"revision": revision, "state": previous}),
    )
    .await?;
    loop {
        tokio::select! {
            inbound = receiver.next() => match inbound {
                Some(Ok(Message::Text(raw))) => {
                    let timestamp_ms = serde_json::from_str::<serde_json::Value>(&raw).ok()
                        .filter(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("metadata.ping"))
                        .and_then(|value| value.get("payload")?.get("timestamp_ms")?.as_u64())
                        .filter(|timestamp_ms| *timestamp_ms <= MAX_METADATA_TIMESTAMP_MS);
                    if let Some(timestamp_ms) = timestamp_ms {
                        enqueue_v070_json(write_tx, "metadata.pong", serde_json::json!({
                            "timestamp_ms": timestamp_ms
                        })).await?;
                    }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    enqueue_relay_data_raw(write_tx, RelayOutKind::Pong, Message::Pong(bytes)).await?;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Ok(()),
                _ => {}
            },
            changed = changes.changed() => {
                changed.map_err(|_| RelayConnectorError::ReceiveFailed)?;
                let current = protocol.lock().await.v070_metadata_payload(device_id)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                if current != previous {
                    revision = revision.saturating_add(1);
                    previous = current.clone();
                    enqueue_v070_json(write_tx, "metadata.update", serde_json::json!({
                        "revision": revision, "state": current
                    })).await?;
                }
            },
            failed = writer_failed_rx.recv() => {
                let _ = failed;
                return Err(RelayConnectorError::SendFailed);
            }
        }
    }
}

async fn run_relay_v070_terminal(
    protocol: SharedDaemonProtocol,
    device_id: termd_proto::DeviceId,
    receiver: &mut RelayReceiver,
    write_tx: &mpsc::Sender<RelayDataWrite>,
    writer_failed_rx: &mut mpsc::Receiver<()>,
) -> Result<(), RelayConnectorError> {
    let first = timeout(Duration::from_secs(30), receiver.next())
        .await
        .map_err(|_| RelayConnectorError::ReceiveFailed)?
        .ok_or(RelayConnectorError::ReceiveFailed)?
        .map_err(|_| RelayConnectorError::ReceiveFailed)?;
    let Message::Text(raw) = first else {
        return Err(RelayConnectorError::InvalidEnvelope);
    };
    let command: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    let payload = command
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let open = match command.get("type").and_then(serde_json::Value::as_str) {
        Some("terminal.create") => serde_json::from_value(payload)
            .map(V070TerminalOpen::Create)
            .map_err(|_| RelayConnectorError::InvalidEnvelope)?,
        Some("terminal.attach") => serde_json::from_value(payload)
            .map(V070TerminalOpen::Attach)
            .map_err(|_| RelayConnectorError::InvalidEnvelope)?,
        _ => return Err(RelayConnectorError::InvalidEnvelope),
    };
    let mut connection = ProtocolConnection::authenticated_v070_terminal(device_id);
    let opened = protocol
        .lock()
        .await
        .open_v070_terminal(&mut connection, open)
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    let session_id = opened.snapshot.session_id;
    if let Some(created) = opened.created {
        enqueue_v070_json(
            write_tx,
            "terminal.created",
            serde_json::to_value(created).map_err(|_| RelayConnectorError::InvalidEnvelope)?,
        )
        .await?;
    } else if let Some(attached) = opened.attached {
        enqueue_v070_json(
            write_tx,
            "terminal.attached",
            serde_json::to_value(attached).map_err(|_| RelayConnectorError::InvalidEnvelope)?,
        )
        .await?;
    }
    enqueue_v070_json(
        write_tx,
        "terminal.snapshot",
        serde_json::to_value(opened.snapshot).map_err(|_| RelayConnectorError::InvalidEnvelope)?,
    )
    .await?;
    flush_relay_v070_terminal_frames(&protocol, &mut connection, session_id, write_tx).await?;
    let mut output = tokio::time::interval(Duration::from_millis(16));
    output.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            inbound = receiver.next() => match inbound {
                Some(Ok(Message::Binary(bytes))) => {
                    connection
                        .write_v070_terminal_frame(&mut *protocol.lock().await, session_id, &bytes)
                        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                }
                Some(Ok(Message::Ping(bytes))) => {
                    enqueue_relay_data_raw(write_tx, RelayOutKind::Pong, Message::Pong(bytes)).await?;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break Ok(()),
                Some(Ok(Message::Text(_))) => break Err(RelayConnectorError::InvalidEnvelope),
                _ => {}
            },
            _ = output.tick() => {
                flush_relay_v070_terminal_frames(&protocol, &mut connection, session_id, write_tx).await?;
            },
            failed = writer_failed_rx.recv() => {
                let _ = failed;
                break Err(RelayConnectorError::SendFailed);
            }
        }
    };
    connection.close(&mut *protocol.lock().await);
    result
}

async fn flush_relay_v070_terminal_frames(
    protocol: &SharedDaemonProtocol,
    connection: &mut ProtocolConnection,
    session_id: SessionId,
    write_tx: &mpsc::Sender<RelayDataWrite>,
) -> Result<(), RelayConnectorError> {
    let frames = connection
        .drain_v070_terminal_frames(&mut *protocol.lock().await, session_id)
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    for frame in frames {
        enqueue_relay_data_raw(write_tx, RelayOutKind::PushOutput, Message::Binary(frame)).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
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
    let response = handle_http_tunnel_stream_request(protocol, method, path, headers, body).await;
    let status = response.status().as_u16();
    let headers = relay_http_end_to_end_response_headers(response.headers());
    let response_head = encode_relay_http_tunnel_response_head_with_headers(status, headers)
        .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    debug!(status, "relay daemon HTTP tunnel handler returned response");
    enqueue_relay_data_raw(
        &write_tx,
        RelayOutKind::Response,
        Message::Binary(response_head),
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

fn relay_http_end_to_end_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut connection_named = HashSet::new();
    for value in headers.get_all("connection").iter() {
        let Ok(value) = value.to_str() else {
            return Vec::new();
        };
        for token in value.split(',') {
            let token = token.trim();
            let Ok(name) = HeaderName::from_bytes(token.as_bytes()) else {
                return Vec::new();
            };
            connection_named.insert(name);
        }
    }

    headers
        .iter()
        .filter_map(|(name, value)| {
            if relay_http_header_is_hop_by_hop(name) || connection_named.contains(name) {
                return None;
            }
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

fn relay_http_header_is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
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
    _relay_endpoint: &str,
    _client_id: RelayClientId,
    sender: &mut S,
    write: RelayDataWrite,
) -> bool
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match write {
        RelayDataWrite::Raw { kind, message } => {
            send_relay_message(sender, message, relay_send_deadline(kind))
                .await
                .is_ok()
        }
    }
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

#[allow(clippy::too_many_arguments)]
async fn connect_relay_route_socket(
    url: &str,
    proxy: Option<&RelayProxyUrl>,
    server_id: ServerId,
    role: ProtoRouteRole,
    daemon_admission_token: Option<&str>,
    route_generation: Option<ProtoNonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<ProtoNonce>,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    connect_relay_route_socket_with_timeout(
        url,
        proxy,
        server_id,
        role,
        daemon_admission_token,
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
    daemon_admission_token: Option<&str>,
    route_generation: Option<ProtoNonce>,
    client_id: Option<RelayClientId>,
    data_token: Option<ProtoNonce>,
    connect_timeout: Duration,
) -> Result<(RelaySender, RelayReceiver), RelayConnectorError> {
    let relay_log_url = relay_route_log_url(url);
    let emit_phase_logs = matches!(role, ProtoRouteRole::DaemonData);
    let mut progress = RelayRouteConnectProgress::new();

    let (socket, _) = match connect_relay_websocket(url, proxy, connect_timeout).await {
        Ok(socket) => socket,
        Err(error) => {
            if emit_phase_logs {
                warn!(
                    layer = "termd",
                    relay = relay_log_url,
                    server_id = %server_id.0,
                    client_id = client_id.map(|id| id.0),
                    role = ?role,
                    phase = progress.phase_label(),
                    timeout_ms = progress.timeout_ms(connect_timeout),
                    %error,
                    "relay route socket phase failed"
                );
            }
            return Err(error);
        }
    };
    if emit_phase_logs {
        trace!(
            relay = relay_log_url,
            server_id = %server_id.0,
            client_id = client_id.map(|id| id.0),
            role = ?role,
            phase = RelayRouteConnectPhase::TcpConnect.label(),
            timeout_ms = RelayRouteConnectPhase::TcpConnect.timeout_ms(connect_timeout),
            "relay route socket phase completed"
        );
    }
    progress.mark_tcp_connected();
    let (mut sender, mut receiver) = socket.split();
    if let Err(error) = send_route_hello(
        &mut sender,
        server_id,
        role,
        daemon_admission_token,
        route_generation,
        client_id,
        data_token,
    )
    .await
    {
        if emit_phase_logs {
            warn!(
                layer = "termd",
                relay = relay_log_url,
                server_id = %server_id.0,
                client_id = client_id.map(|id| id.0),
                role = ?role,
                phase = progress.phase_label(),
                timeout_ms = progress.timeout_ms(connect_timeout),
                %error,
                "relay route socket phase failed"
            );
        }
        return Err(error);
    }
    if emit_phase_logs {
        trace!(
            relay = relay_log_url,
            server_id = %server_id.0,
            client_id = client_id.map(|id| id.0),
            role = ?role,
            phase = RelayRouteConnectPhase::RouteHello.label(),
            timeout_ms = RelayRouteConnectPhase::RouteHello.timeout_ms(connect_timeout),
            "relay route socket phase completed"
        );
    }
    progress.mark_route_hello_sent();
    if let Err(error) = read_route_ready(&mut sender, &mut receiver, server_id, role).await {
        if emit_phase_logs {
            warn!(
                layer = "termd",
                relay = relay_log_url,
                server_id = %server_id.0,
                client_id = client_id.map(|id| id.0),
                role = ?role,
                phase = progress.phase_label(),
                timeout_ms = progress.timeout_ms(connect_timeout),
                %error,
                "relay route socket phase failed"
            );
        }
        return Err(error);
    }
    if emit_phase_logs {
        debug!(
            relay = relay_log_url,
            server_id = %server_id.0,
            client_id = client_id.map(|id| id.0),
            role = ?role,
            phase = RelayRouteConnectPhase::RouteReady.label(),
            timeout_ms = RelayRouteConnectPhase::RouteReady.timeout_ms(connect_timeout),
            "relay route socket phase completed"
        );
    }
    Ok((sender, receiver))
}

async fn send_route_hello(
    sender: &mut RelaySender,
    server_id: ServerId,
    role: ProtoRouteRole,
    daemon_admission_token: Option<&str>,
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
            // 中文注释：relay 是可信入口，daemon route 必须在 prelude 提交 admission token。
            admission: daemon_admission_token.map(|token| ProtoRelayAdmissionPayload::Daemon {
                token: token.to_owned(),
            }),
            route_generation,
            client_id,
            data_token,
            timestamp_ms: current_unix_timestamp_millis(),
        },
    );
    let raw = serde_json::to_string(&envelope).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
    send_relay_message_with_deadline(sender, Message::Text(raw), RELAY_SEND_DEADLINE).await
}

async fn read_route_ready(
    sender: &mut RelaySender,
    receiver: &mut RelayReceiver,
    expected_server_id: ServerId,
    expected_role: ProtoRouteRole,
) -> Result<(), RelayConnectorError> {
    let route_deadline = Instant::now() + RELAY_ROUTE_READY_TIMEOUT;
    loop {
        let Some(message) = (match tokio::time::timeout_at(route_deadline, receiver.next()).await {
            Ok(message) => message,
            Err(_) => {
                warn!(
                    layer = "termd",
                    phase = "relay_route_ready",
                    timeout_code = "relay_route_ready_timeout",
                    timeout_ms = RELAY_ROUTE_READY_TIMEOUT.as_millis() as u64,
                    server_id = %expected_server_id.0,
                    role = ?expected_role,
                    "relay route_ready timed out"
                );
                return Err(RelayConnectorError::RouteReadyTimeout);
            }
        }) else {
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

fn relay_send_deadline(kind: RelayOutKind) -> Option<Duration> {
    match kind {
        RelayOutKind::FileTunnelBody => None,
        RelayOutKind::Pong => Some(RELAY_PONG_DEADLINE),
        _ => Some(RELAY_SEND_DEADLINE),
    }
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
    match deadline {
        Some(deadline) => send_relay_message_with_deadline(sender, message, deadline).await,
        None => sender
            .send(message)
            .await
            .map_err(|_| RelayConnectorError::SendFailed),
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

    let target = relay_target_from_ws_url(url).ok_or(RelayConnectorError::UnsupportedUrl)?;
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
    } else {
        let rest = url.strip_prefix("wss://")?;
        ("wss", rest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[tokio::test]
    async fn relay_metadata_pong_echoes_client_timestamp() {
        let state_dir = std::env::temp_dir().join(format!(
            "termd-relay-metadata-pong-{}-{}",
            std::process::id(),
            ServerId::new().0
        ));
        std::fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("daemon-state.json");
        let protocol = crate::net::server::default_protocol(
            crate::config::DaemonConfig::default_for_state_path(&state_path),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let timestamp_ms = 1_710_000_000_456_u64;
        let relay_peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "metadata.ping",
                        "payload": { "timestamp_ms": timestamp_ms }
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let (_relay_sender, mut receiver) = socket.split();
        let (write_tx, mut write_rx) = mpsc::channel(4);
        let (_writer_failed_tx, mut writer_failed_rx) = mpsc::channel(1);
        let relay_protocol = protocol.clone();
        let relay_task = tokio::spawn(async move {
            run_relay_v070_metadata(
                relay_protocol,
                termd_proto::DeviceId::new(),
                &mut receiver,
                &write_tx,
                &mut writer_failed_rx,
            )
            .await
        });

        let snapshot = write_rx
            .recv()
            .await
            .expect("metadata snapshot should be queued");
        let RelayDataWrite::Raw { message, .. } = snapshot;
        let snapshot: serde_json::Value =
            serde_json::from_str(&message.into_text().unwrap()).unwrap();
        assert_eq!(snapshot["type"], "metadata.snapshot");

        let pong = tokio::time::timeout(Duration::from_millis(250), write_rx.recv())
            .await
            .expect("metadata pong should be queued without polling")
            .expect("metadata writer should remain open");
        let RelayDataWrite::Raw { message, .. } = pong;
        let pong: serde_json::Value = serde_json::from_str(&message.into_text().unwrap()).unwrap();
        assert_eq!(pong["type"], "metadata.pong");
        let echoed_timestamp_ms = pong["payload"]["timestamp_ms"].as_u64();

        relay_task.abort();
        relay_peer.abort();
        drop(protocol);
        std::fs::remove_dir_all(state_dir).unwrap();
        assert_eq!(echoed_timestamp_ms, Some(timestamp_ms));
    }

    #[test]
    fn relay_http_response_headers_include_only_end_to_end_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        headers.insert(
            "connection",
            HeaderValue::from_static("X-Private, x-another"),
        );
        headers.insert("x-private", HeaderValue::from_static("private"));
        headers.insert("x-another", HeaderValue::from_static("private"));
        for name in [
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ] {
            headers.insert(name, HeaderValue::from_static("filtered"));
        }

        let forwarded = relay_http_end_to_end_response_headers(&headers);
        assert!(forwarded.contains(&("content-type".to_owned(), "application/json".to_owned())));
        assert_eq!(
            forwarded
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .count(),
            2
        );
        for blocked in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "x-private",
            "x-another",
        ] {
            assert!(
                forwarded.iter().all(|(name, _)| name != blocked),
                "{blocked} must not enter the relay response frame"
            );
        }
    }

    #[test]
    fn daemon_connector_url_never_contains_transport_token_query() {
        let base = RelayBaseUrl::parse("wss://relay.example/termd/ws").unwrap();
        let url = base.daemon_mux_url(ServerId::new());

        assert_eq!(url, "wss://relay.example/termd/ws");
        assert!(!url.contains('?'));
        assert!(!url.contains("relay_token"));
    }
}
