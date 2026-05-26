//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth、session 和 E2EE
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{AddrParseError, IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use serde::Serialize;
use termd_proto::{
    ErrorPayload, MessageType, PROTOCOL_PACKET_VERSION, PairingToken, ProtocolVersion,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId, SessionId, SessionState,
    TerminalSize, UnixTimestampMillis,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::DaemonConfig;
use crate::pty::supervisor::{SupervisorPtyBackend, SupervisorRestoreCandidate};
use crate::pty::{PtyRestoreInfo, PtySupervisorStatus};
use crate::state::{DaemonState, SessionStateRecord, StateError, StateStore};

use super::protocol::{
    DaemonProtocol, JsonEnvelope, ProtocolConnection, ProtocolConnectionDebugSnapshot,
    ProtocolConnectionDebugTraffic, ProtocolError, ProtocolWireMessage, decode_payload,
    envelope_value,
};
use super::signature::Ed25519SignatureVerifier;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;
// transport 超时只关闭当前 WebSocket 连接；session/supervisor 仍由协议和 PTY 层保持持久。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const WEBSOCKET_MAX_FRAME_SIZE: usize = 1024 * 1024;
const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const SESSION_ACTIVITY_PUSH_MIN_INTERVAL: Duration = Duration::from_millis(250);
const WEBSOCKET_TRAFFIC_LOG_INTERVAL: Duration = Duration::from_secs(1);
const WEBSOCKET_SEND_SLOW_LOG_THRESHOLD: Duration = Duration::from_millis(50);
const WEBSOCKET_SEND_DEBUG_LOG_THRESHOLD: Duration = Duration::from_millis(10);
const WEBSOCKET_SEND_DEBUG_BATCH_ENVELOPES: usize = 8;
const WEBSOCKET_SEND_DEBUG_BATCH_BYTES: usize = 32 * 1024;
const WEBSOCKET_SEND_INFO_BATCH_ENVELOPES: usize = 20;
const WEBSOCKET_SEND_INFO_BATCH_BYTES: usize = 256 * 1024;
const WEBSOCKET_WIRE_QUEUE_CAPACITY: usize = 256;
const WEBSOCKET_PUSH_EVENT_QUEUE_CAPACITY: usize = 1024;
const WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK: usize = 4;
const WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK: usize = 256 * 1024;
const WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED: Duration = Duration::from_millis(2);

fn websocket_idle_timeout_enabled() -> bool {
    // 浏览器页面打开时，WebSocket 的生命周期应由真实 close/error 决定。
    // 后台标签页、移动端挂起或终端长时间静默都不能因为 daemon 侧固定 idle timer 被关闭。
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SessionPushEvent {
    Output(SessionId),
    Activity(SessionId),
    FileTree(SessionId),
    Resize(SessionId),
}

impl SessionPushEvent {
    fn label(self) -> &'static str {
        match self {
            Self::Output(_) => "output",
            Self::Activity(_) => "activity",
            Self::FileTree(_) => "file_tree",
            Self::Resize(_) => "resize",
        }
    }

    fn session_id(self) -> SessionId {
        match self {
            Self::Output(session_id)
            | Self::Activity(session_id)
            | Self::FileTree(session_id)
            | Self::Resize(session_id) => session_id,
        }
    }

    fn min_interval(self) -> Option<Duration> {
        match self {
            // Activity 只是前端列表里的“后台有新输出”提示；不需要按 PTY 输出频率逐包推送。
            // 多窗口 attach 时，如果后台 session 高频输出，未限速的小加密包会按窗口数放大，
            // 造成浏览器 WebSocket 队列和 daemon 事件循环都被固定长度小包拖慢。
            SessionPushEvent::Activity(_) => Some(SESSION_ACTIVITY_PUSH_MIN_INTERVAL),
            SessionPushEvent::Output(_)
            | SessionPushEvent::FileTree(_)
            | SessionPushEvent::Resize(_) => None,
        }
    }
}

#[derive(Debug, Default)]
struct SessionPushEventQueue {
    pending: VecDeque<SessionPushEvent>,
    pending_set: HashSet<SessionPushEvent>,
}

impl SessionPushEventQueue {
    fn enqueue(&mut self, event: SessionPushEvent) {
        if self.pending_set.contains(&event) {
            return;
        }
        self.pending_set.insert(event);
        self.pending.push_back(event);
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn len(&self) -> usize {
        self.pending.len()
    }

    fn peek_front(&self) -> Option<SessionPushEvent> {
        self.pending.front().copied()
    }

    fn pop_front_after_queue_accept(&mut self) -> Option<SessionPushEvent> {
        let event = self.pending.pop_front()?;
        self.pending_set.remove(&event);
        Some(event)
    }
}

#[derive(Debug)]
enum WebSocketWrite {
    Wire {
        kind: WebSocketOutKind,
        messages: Vec<ProtocolWireMessage>,
    },
    Raw {
        kind: WebSocketOutKind,
        message: Message,
    },
}

#[derive(Debug, Clone, Copy)]
struct WebSocketWriteDebug {
    kind: WebSocketOutKind,
    messages: usize,
    bytes: usize,
    raw: bool,
}

impl WebSocketWrite {
    fn debug_snapshot(&self) -> WebSocketWriteDebug {
        match self {
            Self::Wire { kind, messages } => WebSocketWriteDebug {
                kind: *kind,
                messages: messages.len(),
                bytes: websocket_wire_messages_wire_len(messages),
                raw: false,
            },
            Self::Raw { kind, message } => WebSocketWriteDebug {
                kind: *kind,
                messages: 1,
                bytes: websocket_message_bytes(message),
                raw: true,
            },
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct WebSocketTrafficBucket {
    calls: u64,
    envelopes: u64,
    bytes: u64,
}

impl WebSocketTrafficBucket {
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
struct WebSocketTrafficCounters {
    in_text: WebSocketTrafficBucket,
    in_binary: WebSocketTrafficBucket,
    in_ping: WebSocketTrafficBucket,
    in_pong: WebSocketTrafficBucket,
    in_close: WebSocketTrafficBucket,
    out_route_ready: WebSocketTrafficBucket,
    out_initial: WebSocketTrafficBucket,
    out_response: WebSocketTrafficBucket,
    out_push_output: WebSocketTrafficBucket,
    out_push_activity: WebSocketTrafficBucket,
    out_push_file_tree: WebSocketTrafficBucket,
    out_push_resize: WebSocketTrafficBucket,
    out_plain_error: WebSocketTrafficBucket,
    out_ping: WebSocketTrafficBucket,
    out_pong: WebSocketTrafficBucket,
    send_errors: u64,
}

impl WebSocketTrafficCounters {
    fn record_in(&mut self, message: &Message) {
        match message {
            Message::Text(raw) => self.in_text.record(1, raw.len()),
            Message::Binary(raw) => self.in_binary.record(1, raw.len()),
            Message::Ping(payload) => self.in_ping.record(0, payload.len()),
            Message::Pong(payload) => self.in_pong.record(0, payload.len()),
            Message::Close(_) => self.in_close.record(0, 0),
        }
    }

    fn record_out(&mut self, kind: WebSocketOutKind, envelopes: usize, bytes: usize) {
        if kind.is_payload_batch() && envelopes == 0 && bytes == 0 {
            return;
        }
        match kind {
            WebSocketOutKind::RouteReady => self.out_route_ready.record(envelopes, bytes),
            WebSocketOutKind::Initial => self.out_initial.record(envelopes, bytes),
            WebSocketOutKind::Response => self.out_response.record(envelopes, bytes),
            WebSocketOutKind::PushOutput => self.out_push_output.record(envelopes, bytes),
            WebSocketOutKind::PushActivity => self.out_push_activity.record(envelopes, bytes),
            WebSocketOutKind::PushFileTree => self.out_push_file_tree.record(envelopes, bytes),
            WebSocketOutKind::PushResize => self.out_push_resize.record(envelopes, bytes),
            WebSocketOutKind::PlainError => self.out_plain_error.record(envelopes, bytes),
            WebSocketOutKind::Ping => self.out_ping.record(envelopes, bytes),
            WebSocketOutKind::Pong => self.out_pong.record(envelopes, bytes),
        }
    }

    fn record_send_error(&mut self) {
        self.send_errors = self.send_errors.saturating_add(1);
    }

    fn record_queued_raw(&mut self, kind: WebSocketOutKind, bytes: usize) {
        self.record_out(kind, 0, bytes);
    }

    fn has_activity(&self) -> bool {
        !self.in_text.is_empty()
            || !self.in_binary.is_empty()
            || !self.in_ping.is_empty()
            || !self.in_pong.is_empty()
            || !self.in_close.is_empty()
            || !self.out_route_ready.is_empty()
            || !self.out_initial.is_empty()
            || !self.out_response.is_empty()
            || !self.out_push_output.is_empty()
            || !self.out_push_activity.is_empty()
            || !self.out_push_file_tree.is_empty()
            || !self.out_push_resize.is_empty()
            || !self.out_plain_error.is_empty()
            || !self.out_ping.is_empty()
            || !self.out_pong.is_empty()
            || self.send_errors > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketOutKind {
    RouteReady,
    Initial,
    Response,
    PushOutput,
    PushActivity,
    PushFileTree,
    PushResize,
    PlainError,
    Ping,
    Pong,
}

impl WebSocketOutKind {
    fn is_payload_batch(self) -> bool {
        // control frame 发送本身就是事件；业务 batch 如果没有 envelope，就不要污染空转诊断。
        !matches!(self, Self::Ping | Self::Pong)
    }

    fn label(self) -> &'static str {
        match self {
            Self::RouteReady => "route_ready",
            Self::Initial => "initial",
            Self::Response => "response",
            Self::PushOutput => "push_output",
            Self::PushActivity => "push_activity",
            Self::PushFileTree => "push_file_tree",
            Self::PushResize => "push_resize",
            Self::PlainError => "plain_error",
            Self::Ping => "ping",
            Self::Pong => "pong",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WebSocketWatcherCounts {
    output: usize,
    activity: usize,
    file_tree: usize,
    resize: usize,
}

pub type DefaultDaemonProtocol = DaemonProtocol<SupervisorPtyBackend, Ed25519SignatureVerifier>;
/// daemon 的协议核心仍是单线程语义，但等待这把锁必须让出 Tokio worker。
///
/// 直连 WebSocket 和 relay mux 共用同一个协议状态；如果使用 `std::sync::Mutex`，
/// 快速切换大输出 session 时多个任务会在 worker 线程上阻塞等待锁，连心跳、输入和
/// relay 主干读写都会一起迟滞。`tokio::sync::Mutex` 保持串行临界区，同时让等待者挂起。
pub type SharedDaemonProtocol = Arc<Mutex<DefaultDaemonProtocol>>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("invalid listen address")]
    InvalidListenAddress(#[from] AddrParseError),
    #[error("failed to bind daemon HTTP listener")]
    Bind(#[source] std::io::Error),
    #[error("daemon HTTP server failed")]
    Serve(#[source] std::io::Error),
    #[error("failed to load TLS certificate chain")]
    TlsCertificate(#[source] std::io::Error),
    #[error("failed to load TLS private key")]
    TlsPrivateKey(#[source] std::io::Error),
    #[error("TLS private key is missing")]
    MissingTlsPrivateKey,
    #[error("TLS configuration is invalid")]
    TlsConfig,
    #[error("daemon state persistence failed")]
    State(#[from] StateError),
}

#[derive(Clone, PartialEq, Eq)]
pub struct TlsPaths {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

impl TlsPaths {
    pub fn new(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
        }
    }
}

impl std::fmt::Debug for TlsPaths {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 证书路径可用于排障；私钥路径按敏感启动材料处理，不进入 Debug 输出。
        formatter
            .debug_struct("TlsPaths")
            .field("cert_path", &self.cert_path)
            .field("key_path_configured", &true)
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct HealthzPayload {
    status: &'static str,
    protocol_version: ProtocolVersion,
    server_id: ServerId,
}

#[derive(Debug, Serialize)]
struct LocalPairingTokenPayload {
    token: PairingToken,
    expires_at_ms: UnixTimestampMillis,
    ttl_ms: u64,
    server_id: ServerId,
    daemon_public_key: termd_proto::PublicKey,
    /// Web 端默认优先使用当前页面地址；这里提供兼容回退地址。
    ws_url: String,
}

/// 构造生产默认协议状态，并接入本地状态文件。
pub fn try_default_protocol(config: DaemonConfig) -> Result<SharedDaemonProtocol, ServerError> {
    let mut state = StateStore::load(&config.state_path)?;
    let supervisor_backend = SupervisorPtyBackend::for_state_path(&config.state_path);
    let mut live_supervisor_session_ids: Option<HashSet<SessionId>> = None;
    let repaired_count = match supervisor_backend.live_supervisor_restore_candidates() {
        Ok(supervisors) => {
            let session_ids = supervisors
                .iter()
                .filter_map(|supervisor| uuid::Uuid::parse_str(&supervisor.session_id).ok())
                .map(SessionId)
                .collect::<HashSet<_>>();
            live_supervisor_session_ids = Some(session_ids);
            adopt_or_repair_runtime_sessions_from_supervisors(
                &mut state,
                supervisors,
                current_unix_timestamp_millis(),
            )
        }
        // /proc 不可读只会影响异常升级恢复，不能阻断 daemon 正常启动。
        Err(error) => {
            warn!(%error, "failed to inspect live session supervisors");
            0
        }
    };
    if repaired_count > 0 {
        warn!(
            repaired_count,
            "adopted or repaired live session supervisors in runtime state"
        );
    }
    let valid_supervisor_session_ids = state
        .sessions
        .iter()
        .filter(|session| session.state == SessionState::Running && session.restore_info.is_some())
        .map(|session| session.session_id.0.to_string());
    warn_about_orphaned_supervisors(&supervisor_backend, valid_supervisor_session_ids);
    let protocol = DaemonProtocol::from_state(
        config.clone(),
        supervisor_backend,
        Ed25519SignatureVerifier,
        state,
    )?;
    let restored_supervisor_session_ids = protocol
        .snapshot_state()
        .sessions
        .into_iter()
        .filter(|session| session.state == SessionState::Running && session.restore_info.is_some())
        .map(|session| session.session_id.0.to_string())
        .collect::<Vec<_>>();
    warn_about_orphaned_supervisors(
        &SupervisorPtyBackend::for_state_path(&config.state_path),
        restored_supervisor_session_ids,
    );
    // 首次启动时立即写入 daemon identity，避免已展示的 server id 只停留在内存里。
    let mut protocol = protocol;
    protocol.persist_state()?;
    if let Some(protected_session_ids) = live_supervisor_session_ids.as_ref() {
        if let Err(error) = protocol.prune_closed_sessions_except(protected_session_ids) {
            warn!(%error, "failed to prune closed session records during startup");
        }
    }
    Ok(Arc::new(Mutex::new(protocol)))
}

fn warn_about_orphaned_supervisors<I, S>(backend: &SupervisorPtyBackend, valid_session_ids: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match backend.orphaned_supervisor_count(valid_session_ids) {
        Ok(orphaned_count) if orphaned_count > 0 => {
            // 启动/升级恢复路径绝不能因为判断为孤儿就主动 SIGTERM supervisor。
            // 如果 socket 文件临时缺失或状态迁移失败，里面仍可能是用户正在跑的 shell。
            warn!(
                orphaned_count,
                "left orphaned session supervisors running during startup"
            );
        }
        Ok(_) => {}
        Err(error) => warn!(%error, "failed to inspect orphaned session supervisors"),
    }
}

pub(crate) fn adopt_or_repair_runtime_sessions_from_supervisors(
    state: &mut DaemonState,
    supervisors: impl IntoIterator<Item = SupervisorRestoreCandidate>,
    now_ms: UnixTimestampMillis,
) -> usize {
    let mut session_positions = state
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.session_id, index))
        .collect::<HashMap<_, _>>();
    let mut repaired_count = 0;

    for supervisor in supervisors {
        let Ok(raw_session_id) = uuid::Uuid::parse_str(&supervisor.session_id) else {
            continue;
        };
        let session_id = SessionId(raw_session_id);
        let mut restored_session = SessionStateRecord {
            session_id,
            state: SessionState::Running,
            size: TerminalSize {
                rows: supervisor.size.rows,
                cols: supervisor.size.cols,
                pixel_width: supervisor.size.pixel_width,
                pixel_height: supervisor.size.pixel_height,
            },
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            restore_info: Some(PtyRestoreInfo::UnixSocket {
                socket_path: supervisor.socket_path,
                supervisor_pid: supervisor.supervisor_pid,
                supervisor_status: PtySupervisorStatus::Running,
            }),
        };

        if let Some(index) = session_positions.get(&session_id).copied() {
            let existing_session = &mut state.sessions[index];
            // live supervisor 是 runtime 事实来源。旧安装脚本或异常重启可能已经把 SQLite
            // runtime 行误标成 closed / 去掉 restore_info；supervisor 仍在时必须修回 Running，
            // 否则 daemon 重启会把用户还在运行的 shell 从 session 列表里“丢掉”。
            let needs_repair = existing_session.state != SessionState::Running
                || !restore_info_is_running_supervisor(existing_session.restore_info.as_ref());
            if needs_repair {
                restored_session.created_at_ms = existing_session.created_at_ms;
                *existing_session = restored_session;
                repaired_count += 1;
            }
            continue;
        }

        state.sessions.push(restored_session);
        session_positions.insert(session_id, state.sessions.len() - 1);
        repaired_count += 1;
    }

    repaired_count
}

fn restore_info_is_running_supervisor(restore_info: Option<&PtyRestoreInfo>) -> bool {
    matches!(
        restore_info,
        Some(PtyRestoreInfo::UnixSocket {
            supervisor_status: PtySupervisorStatus::Running,
            ..
        })
    )
}

/// 测试与旧调用点使用的便捷构造器；生产启动路径使用 `try_default_protocol` 返回结构化错误。
pub fn default_protocol(config: DaemonConfig) -> SharedDaemonProtocol {
    try_default_protocol(config).expect("default daemon protocol should initialize")
}

pub fn router(protocol: SharedDaemonProtocol, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/local/pairing-token", post(local_pairing_token))
        .route("/ws", get(ws_handler))
        .with_state(protocol);

    if web_enabled {
        router.fallback(termweb::embedded_web_handler)
    } else {
        router
    }
}

pub async fn serve(
    config: DaemonConfig,
    protocol: SharedDaemonProtocol,
    web_enabled: bool,
) -> Result<(), ServerError> {
    let addr = listen_addr_from_config(&config)?;
    let listener = TcpListener::bind(addr).await.map_err(ServerError::Bind)?;

    serve_listener(listener, protocol, web_enabled).await
}

pub async fn serve_tls(
    config: DaemonConfig,
    protocol: SharedDaemonProtocol,
    tls_paths: TlsPaths,
    web_enabled: bool,
) -> Result<(), ServerError> {
    let addr = listen_addr_from_config(&config)?;
    let listener = TcpListener::bind(addr).await.map_err(ServerError::Bind)?;

    serve_tls_listener(listener, protocol, tls_paths, web_enabled).await
}

fn listen_addr_from_config(config: &DaemonConfig) -> Result<SocketAddr, ServerError> {
    // 分开解析 IP 和端口，避免 IPv6 监听地址被普通字符串拼接破坏。
    let ip: IpAddr = config.listen_host.parse()?;
    Ok(SocketAddr::new(ip, config.listen_port))
}

/// 使用调用方已经绑定好的 listener 启动 daemon HTTP 服务。
///
/// 该函数只服务网络启动边界，方便集成测试使用随机端口；auth、session 和 E2EE 语义仍全部
/// 留在 `DaemonProtocol` 中，避免为了测试放宽生产协议。
pub async fn serve_listener(
    listener: TcpListener,
    protocol: SharedDaemonProtocol,
    web_enabled: bool,
) -> Result<(), ServerError> {
    axum::serve(
        listener,
        router(protocol, web_enabled).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(ServerError::Serve)
}

pub async fn serve_tls_listener(
    listener: TcpListener,
    protocol: SharedDaemonProtocol,
    tls_paths: TlsPaths,
    web_enabled: bool,
) -> Result<(), ServerError> {
    let tls_config = load_rustls_server_config(&tls_paths)?;

    // TLS 只替换 transport accept 层；router 和协议状态机保持同一套路径与 E2EE 规则。
    serve_rustls_listener(listener, router(protocol, web_enabled), tls_config).await
}

fn load_rustls_server_config(tls_paths: &TlsPaths) -> Result<rustls::ServerConfig, ServerError> {
    let certs = rustls::pki_types::CertificateDer::pem_file_iter(&tls_paths.cert_path)
        .map_err(io_error_for_tls_cert)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error_for_tls_cert)?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(&tls_paths.key_path)
        .map_err(io_error_for_tls_key)?;

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| ServerError::TlsConfig)
}

fn io_error_for_tls_cert(error: rustls::pki_types::pem::Error) -> ServerError {
    ServerError::TlsCertificate(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn io_error_for_tls_key(error: rustls::pki_types::pem::Error) -> ServerError {
    match error {
        rustls::pki_types::pem::Error::NoItemsFound => ServerError::MissingTlsPrivateKey,
        other => {
            ServerError::TlsPrivateKey(std::io::Error::new(std::io::ErrorKind::InvalidData, other))
        }
    }
}

async fn serve_rustls_listener(
    listener: TcpListener,
    router: Router,
    tls_config: rustls::ServerConfig,
) -> Result<(), ServerError> {
    use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
    use axum_core::{body::Body, extract::Request};
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::{server::conn::auto::Builder, service::TowerToHyperService};
    use std::convert::Infallible;
    use std::future::poll_fn;
    use std::sync::Arc;
    use tokio_rustls::TlsAcceptor;
    use tower::ServiceExt as _;
    use tower_service::Service;

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let mut make_service: IntoMakeServiceWithConnectInfo<_, SocketAddr> =
        router.into_make_service_with_connect_info::<SocketAddr>();

    loop {
        let (tcp_stream, remote_addr) = listener.accept().await.map_err(ServerError::Serve)?;
        let acceptor = acceptor.clone();

        poll_fn(|cx| Service::<SocketAddr>::poll_ready(&mut make_service, cx))
            .await
            .unwrap_or_else(|error: Infallible| match error {});
        let service = make_service
            .call(remote_addr)
            .await
            .unwrap_or_else(|error: Infallible| match error {})
            .map_request(|req: Request<Incoming>| req.map(Body::new));

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    warn!(%error, "TLS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let hyper_service = TowerToHyperService::new(service);
            if let Err(error) = Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await
            {
                warn!(%error, "TLS HTTP/WebSocket connection failed");
            }
        });
    }
}

async fn healthz(State(protocol): State<SharedDaemonProtocol>) -> Json<HealthzPayload> {
    let protocol = protocol.lock().await;

    Json(HealthzPayload {
        status: "ok",
        protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
        server_id: protocol.server_id(),
    })
}

async fn local_pairing_token(
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(protocol): State<SharedDaemonProtocol>,
) -> Response {
    if !is_loopback_peer(peer_addr) {
        // 本地管理端点只允许 loopback；错误响应不回显 peer、token 或内部状态。
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorPayload {
                code: "local_only".to_owned(),
                message: "local pairing endpoint is only available from loopback".to_owned(),
            }),
        )
            .into_response();
    }

    let now_ms = current_unix_timestamp_millis();
    let mut protocol = protocol.lock().await;
    let ttl_ms = protocol.config().pairing_token_ttl_ms;
    let server_id = protocol.server_id();
    let daemon_public_key = protocol.daemon_public_identity().public_key.clone();
    let ws_url = pairing_ws_url_from_config(protocol.config(), server_id);
    let record = match protocol.issue_pairing_token(now_ms) {
        Ok(record) => record,
        Err(error) => {
            // PairingError 不包含 token 明文；日志仍只记录脱敏失败原因。
            warn!(%error, "failed to issue local pairing token");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorPayload {
                    code: "pairing_token_unavailable".to_owned(),
                    message: "pairing token could not be issued".to_owned(),
                }),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(LocalPairingTokenPayload {
            token: record.token().clone(),
            expires_at_ms: record.expires_at_ms(),
            ttl_ms,
            server_id,
            daemon_public_key,
            ws_url,
        }),
    )
        .into_response()
}

fn pairing_ws_url_from_config(config: &DaemonConfig, server_id: ServerId) -> String {
    // 配置里保存的是模板；本地 token 接口返回实际可用的 URL，CLI 生成二维码时无需用户拼 server_id。
    config
        .default_pairing_ws_url
        .trim()
        .replace("{server_id}", &server_id.0.to_string())
}

fn is_loopback_peer(peer_addr: SocketAddr) -> bool {
    peer_addr.ip().is_loopback()
}

async fn ws_handler(
    websocket: WebSocketUpgrade,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(protocol): State<SharedDaemonProtocol>,
) -> impl IntoResponse {
    websocket
        .max_frame_size(WEBSOCKET_MAX_FRAME_SIZE)
        .max_message_size(WEBSOCKET_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, protocol, peer_addr))
}

async fn read_route_hello(
    write_wire_tx: &mpsc::Sender<WebSocketWrite>,
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    expected_server_id: ServerId,
) -> Result<RouteHelloPayload, ProtocolError> {
    loop {
        let Some(message) = receiver.next().await else {
            return Err(ProtocolError::InvalidEnvelope);
        };
        let message = message.map_err(|error| {
            warn!(%error, "websocket receive failed while waiting for route prelude");
            ProtocolError::InvalidEnvelope
        })?;

        match message {
            Message::Ping(payload) => {
                if enqueue_websocket_control_raw(
                    write_wire_tx,
                    WebSocketOutKind::Pong,
                    Message::Pong(payload),
                )
                .await
                .is_err()
                {
                    return Err(ProtocolError::InvalidEnvelope);
                }
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return Err(ProtocolError::InvalidEnvelope),
            other => {
                let Some(envelope) = message_to_envelope(other)? else {
                    return Err(ProtocolError::InvalidEnvelope);
                };
                if envelope.kind != MessageType::RouteHello {
                    return Err(ProtocolError::InvalidEnvelope);
                }

                let payload: RouteHelloPayload = decode_payload(envelope.payload)?;
                if payload.protocol_version != ProtocolVersion(PROTOCOL_PACKET_VERSION) {
                    return Err(ProtocolError::InvalidEnvelope);
                }
                if payload.server_id != expected_server_id {
                    return Err(ProtocolError::InvalidEnvelope);
                }
                if payload.role != RouteRole::Client {
                    return Err(ProtocolError::InvalidEnvelope);
                }

                return Ok(payload);
            }
        }
    }
}

fn route_ready_envelope(server_id: ServerId, role: RouteRole) -> JsonEnvelope {
    envelope_value(
        MessageType::RouteReady,
        RouteReadyPayload { server_id, role },
    )
    .expect("route_ready payload should serialize")
}

fn current_websocket_watcher_counts(
    watched_output_sessions: &HashSet<SessionId>,
    watched_activity_sessions: &HashSet<SessionId>,
    watched_file_tree_sessions: &HashSet<SessionId>,
    watched_resize_sessions: &HashSet<SessionId>,
) -> WebSocketWatcherCounts {
    WebSocketWatcherCounts {
        output: watched_output_sessions.len(),
        activity: watched_activity_sessions.len(),
        file_tree: watched_file_tree_sessions.len(),
        resize: watched_resize_sessions.len(),
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

fn maybe_log_websocket_traffic(
    peer_addr: SocketAddr,
    traffic: &mut WebSocketTrafficCounters,
    last_logged_at: &mut Instant,
    connection: &mut ProtocolConnection,
    watchers: WebSocketWatcherCounts,
    force: bool,
) {
    if !traffic.has_activity() {
        return;
    }
    if !force && last_logged_at.elapsed() < WEBSOCKET_TRAFFIC_LOG_INTERVAL {
        return;
    }

    let flow = connection.debug_snapshot();
    let protocol_traffic = connection.take_debug_traffic();
    if websocket_traffic_should_promote_to_info(traffic, &protocol_traffic) {
        info_websocket_traffic(peer_addr, traffic, &protocol_traffic, watchers, flow);
    } else {
        debug_websocket_traffic(peer_addr, traffic, &protocol_traffic, watchers, flow);
    }
    *traffic = WebSocketTrafficCounters::default();
    *last_logged_at = Instant::now();
}

fn websocket_traffic_should_promote_to_info(
    traffic: &WebSocketTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
) -> bool {
    // 正常心跳、RPC 和 activity 计数只进 debug；只有疑似空转/背压/断连时提升到 info，
    // 这样线上默认日志能抓到异常，又不会长期刷屏。
    traffic.send_errors > 0
        || traffic.out_push_output.calls > 1_000
        || traffic.out_response.calls > 20
        || traffic.out_response.envelopes > 20
        || traffic.out_push_activity.calls > 100
        || protocol_traffic.inbound_flow_packets > 200
        || protocol_traffic.method_count_exceeds(20)
        || protocol_traffic.inbound_stream_chunks > 100
        || protocol_traffic.outbound_stream_chunks > 100
}

fn info_websocket_traffic(
    peer_addr: SocketAddr,
    traffic: &WebSocketTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    watchers: WebSocketWatcherCounts,
    flow: ProtocolConnectionDebugSnapshot,
) {
    info!(
        peer_addr = %peer_addr,
        ?traffic,
        ?protocol_traffic,
        watchers_output = watchers.output,
        watchers_activity = watchers.activity,
        watchers_file_tree = watchers.file_tree,
        watchers_resize = watchers.resize,
        flow_packet_mode = flow.packet_mode,
        flow_binary_mode = flow.binary_mode,
        flow_attached_sessions = flow.attached_sessions,
        flow_watched_sessions = flow.watched_sessions,
        flow_terminal_streams = flow.terminal_streams,
        flow_zero_credit_terminal_streams = flow.zero_credit_terminal_streams,
        flow_total_output_credit = flow.total_output_credit,
        flow_pending_raw_chunks = flow.pending_raw_chunks,
        flow_pending_terminal_frames = flow.pending_terminal_frames,
        "websocket traffic counters"
    );
}

fn debug_websocket_traffic(
    peer_addr: SocketAddr,
    traffic: &WebSocketTrafficCounters,
    protocol_traffic: &ProtocolConnectionDebugTraffic,
    watchers: WebSocketWatcherCounts,
    flow: ProtocolConnectionDebugSnapshot,
) {
    debug!(
        peer_addr = %peer_addr,
        ?traffic,
        ?protocol_traffic,
        watchers_output = watchers.output,
        watchers_activity = watchers.activity,
        watchers_file_tree = watchers.file_tree,
        watchers_resize = watchers.resize,
        flow_packet_mode = flow.packet_mode,
        flow_binary_mode = flow.binary_mode,
        flow_attached_sessions = flow.attached_sessions,
        flow_watched_sessions = flow.watched_sessions,
        flow_terminal_streams = flow.terminal_streams,
        flow_zero_credit_terminal_streams = flow.zero_credit_terminal_streams,
        flow_total_output_credit = flow.total_output_credit,
        flow_pending_raw_chunks = flow.pending_raw_chunks,
        flow_pending_terminal_frames = flow.pending_terminal_frames,
        "websocket traffic counters"
    );
}

async fn handle_socket(socket: WebSocket, protocol: SharedDaemonProtocol, peer_addr: SocketAddr) {
    let (sender, mut receiver) = socket.split();
    let (write_wire_tx, write_wire_rx) =
        mpsc::channel::<WebSocketWrite>(WEBSOCKET_WIRE_QUEUE_CAPACITY);
    let (writer_failed_tx, mut writer_failed_rx) = mpsc::channel::<()>(1);
    // 中文注释：直连从 route prelude 开始就只保留一条有界 writer queue。
    // 入队成功即代表当前连接的输出责任已经交给 transport；真实 socket 失败
    // 只通过 writer_failed_rx 传播回来，关闭当前连接。
    let writer_task = tokio::spawn(run_websocket_writer(
        peer_addr,
        write_wire_rx,
        writer_failed_tx,
        sender,
    ));
    let (push_event_tx, mut push_event_rx) =
        mpsc::channel::<SessionPushEvent>(WEBSOCKET_PUSH_EVENT_QUEUE_CAPACITY);
    let mut watched_output_sessions = HashSet::new();
    let mut watched_activity_sessions = HashSet::new();
    let mut watched_file_tree_sessions = HashSet::new();
    let mut watched_resize_sessions = HashSet::new();
    let mut watcher_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut push_event_queue = SessionPushEventQueue::default();
    let mut push_drain_wake_pending = false;
    let mut traffic = WebSocketTrafficCounters::default();
    let mut last_traffic_log = Instant::now();
    let server_id = {
        let protocol = protocol.lock().await;
        protocol.server_id()
    };

    let route_hello = match timeout(
        ROUTE_PRELUDE_TIMEOUT,
        read_route_hello(&write_wire_tx, &mut receiver, server_id),
    )
    .await
    {
        Ok(Ok(route_hello)) => route_hello,
        Ok(Err(error)) => {
            let envelope = plaintext_error(error);
            let messages = vec![ProtocolWireMessage::Json(envelope)];
            let bytes = websocket_wire_messages_wire_len(&messages);
            if enqueue_websocket_wire(&write_wire_tx, WebSocketOutKind::PlainError, messages)
                .await
                .is_ok()
            {
                traffic.record_out(WebSocketOutKind::PlainError, 1, bytes);
                finish_websocket_writer(write_wire_tx, writer_task).await;
            } else {
                traffic.record_send_error();
                writer_task.abort();
            }
            return;
        }
        Err(_) => {
            let envelope = route_prelude_timeout_error();
            let messages = vec![ProtocolWireMessage::Json(envelope)];
            let bytes = websocket_wire_messages_wire_len(&messages);
            if enqueue_websocket_wire(&write_wire_tx, WebSocketOutKind::PlainError, messages)
                .await
                .is_ok()
            {
                traffic.record_out(WebSocketOutKind::PlainError, 1, bytes);
                finish_websocket_writer(write_wire_tx, writer_task).await;
            } else {
                traffic.record_send_error();
                writer_task.abort();
            }
            return;
        }
    };
    let route_ready = route_ready_envelope(route_hello.server_id, route_hello.role);
    let route_ready_bytes =
        websocket_wire_messages_wire_len(&[ProtocolWireMessage::Json(route_ready.clone())]);
    if enqueue_websocket_wire(
        &write_wire_tx,
        WebSocketOutKind::RouteReady,
        vec![ProtocolWireMessage::Json(route_ready)],
    )
    .await
    .is_err()
    {
        traffic.record_send_error();
        writer_task.abort();
        return;
    }
    traffic.record_out(WebSocketOutKind::RouteReady, 1, route_ready_bytes);

    let (mut connection, initial_messages) = {
        let protocol = protocol.lock().await;
        protocol.start_connection_for_peer(Some(peer_addr.ip().to_string()))
    };

    let initial_count = initial_messages.len();
    let initial_wire_messages = initial_messages
        .into_iter()
        .map(ProtocolWireMessage::Json)
        .collect::<Vec<_>>();
    let initial_bytes = websocket_wire_messages_wire_len(&initial_wire_messages);
    if enqueue_websocket_wire(
        &write_wire_tx,
        WebSocketOutKind::Initial,
        initial_wire_messages,
    )
    .await
    .is_err()
    {
        traffic.record_send_error();
        writer_task.abort();
        return;
    }
    traffic.record_out(WebSocketOutKind::Initial, initial_count, initial_bytes);

    let mut idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
    let mut heartbeat = tokio::time::interval_at(
        Instant::now() + WEBSOCKET_HEARTBEAT_INTERVAL,
        WEBSOCKET_HEARTBEAT_INTERVAL,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending_pong_deadline: Option<Instant> = None;
    let mut last_activity = Instant::now();
    loop {
        let pending_pong_deadline_snapshot = pending_pong_deadline;
        // 控制和 client close 必须先于输出队列处理；快速切换时旧 attach 才能及时取消。
        // 中文注释：push drain 只在主循环内按预算推进，writer 成功发送不再产生回调。
        // 队列容量就是背压边界，避免高频发送回执反过来饿住输入、close 和 pong。
        tokio::select! {
            biased;

            _ = tokio::time::sleep_until(idle_deadline), if websocket_idle_timeout_enabled() => {
                warn!(peer_addr = %peer_addr, "websocket idle timeout");
                break;
            }
            _ = async move {
                if let Some(deadline) = pending_pong_deadline_snapshot {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                warn!(peer_addr = %peer_addr, "websocket pong timed out");
                break;
            }
            maybe_message = receiver.next() => {
                let Some(message) = maybe_message else {
                    break;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(%error, "websocket receive failed");
                        break;
                    }
                };
                let now = Instant::now();
                idle_deadline = now + WEBSOCKET_IDLE_TIMEOUT;
                last_activity = now;
                traffic.record_in(&message);
                debug!(
                    peer_addr = %peer_addr,
                    message_kind = websocket_message_kind(&message),
                    message_bytes = websocket_message_bytes(&message),
                    "websocket inbound frame received"
                );

                match message {
                    Message::Ping(payload) => {
                        let pong_bytes = payload.len();
                        if enqueue_websocket_control_raw(
                            &write_wire_tx,
                            WebSocketOutKind::Pong,
                            Message::Pong(payload),
                        )
                        .await
                        .is_err() {
                            traffic.record_send_error();
                            break;
                        }
                        traffic.record_queued_raw(WebSocketOutKind::Pong, pong_bytes);
                        maybe_log_websocket_traffic(
                            peer_addr,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connection,
                            current_websocket_watcher_counts(
                                &watched_output_sessions,
                                &watched_activity_sessions,
                                &watched_file_tree_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                        continue;
                    }
                    Message::Pong(_) => {
                        pending_pong_deadline = None;
                        maybe_log_websocket_traffic(
                            peer_addr,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connection,
                            current_websocket_watcher_counts(
                                &watched_output_sessions,
                                &watched_activity_sessions,
                                &watched_file_tree_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                        continue;
                    }
                    Message::Close(_) => break,
                    other => {
                        let Some(wire_message) = (match message_to_wire_message(other) {
                            Ok(message) => message,
                            Err(error) => {
                                let responses =
                                    vec![ProtocolWireMessage::Json(plaintext_error(error))];
                                let response_count = responses.len();
                                let response_bytes = websocket_wire_messages_wire_len(&responses);
                                if enqueue_websocket_wire(
                                    &write_wire_tx,
                                    WebSocketOutKind::PlainError,
                                    responses,
                                )
                                .await
                                .is_err()
                                {
                                    traffic.record_send_error();
                                    break;
                                }
                                traffic.record_out(
                                    WebSocketOutKind::PlainError,
                                    response_count,
                                    response_bytes,
                                );
                                maybe_log_websocket_traffic(
                                    peer_addr,
                                    &mut traffic,
                                    &mut last_traffic_log,
                                    &mut connection,
                                    current_websocket_watcher_counts(
                                        &watched_output_sessions,
                                        &watched_activity_sessions,
                                        &watched_file_tree_sessions,
                                        &watched_resize_sessions,
                                    ),
                                    false,
                                );
                                continue;
                            }
                        }) else {
                            break;
                        };

                        let responses = {
                            let mut protocol = protocol.lock().await;
                            connection.handle_wire_message(&mut protocol, wire_message)
                        };
                        queue_deferred_output_wakeups(&mut connection, &mut push_event_queue);
                        let response_count = responses.len();
                        let response_bytes = websocket_wire_messages_wire_len(&responses);

                        if enqueue_websocket_wire(
                            &write_wire_tx,
                            WebSocketOutKind::Response,
                            responses,
                        )
                        .await
                        .is_err()
                        {
                            traffic.record_send_error();
                            break;
                        }
                        traffic.record_out(
                            WebSocketOutKind::Response,
                            response_count,
                            response_bytes,
                        );

                        let initial_output_sessions = register_session_watchers(
                            &connection,
                            &protocol,
                            &mut watched_output_sessions,
                            &mut watched_activity_sessions,
                            &mut watched_file_tree_sessions,
                            &mut watched_resize_sessions,
                            &push_event_tx,
                            &mut watcher_tasks,
                        )
                        .await;
                        queue_initial_output_events(&initial_output_sessions, &mut push_event_queue);
                        if let Err(()) = drain_websocket_push_events(
                            &protocol,
                            &mut connection,
                            &mut push_event_queue,
                            &write_wire_tx,
                            &mut traffic,
                            &push_event_tx,
                            &mut push_drain_wake_pending,
                        )
                        .await
                        {
                            traffic.record_send_error();
                            break;
                        }
                        maybe_log_websocket_traffic(
                            peer_addr,
                            &mut traffic,
                            &mut last_traffic_log,
                            &mut connection,
                            current_websocket_watcher_counts(
                                &watched_output_sessions,
                                &watched_activity_sessions,
                                &watched_file_tree_sessions,
                                &watched_resize_sessions,
                            ),
                            false,
                        );
                    }
                };
            }
            maybe_failed = writer_failed_rx.recv() => {
                if maybe_failed.is_some() {
                    traffic.record_send_error();
                    warn!(
                        peer_addr = %peer_addr,
                        "websocket writer reported failure"
                    );
                }
                maybe_log_websocket_traffic(
                    peer_addr,
                    &mut traffic,
                    &mut last_traffic_log,
                    &mut connection,
                    current_websocket_watcher_counts(
                        &watched_output_sessions,
                        &watched_activity_sessions,
                        &watched_file_tree_sessions,
                        &watched_resize_sessions,
                    ),
                    false,
                );
                break;
            }
            _ = heartbeat.tick() => {
                if pending_pong_deadline.is_none()
                    && last_activity.elapsed() >= WEBSOCKET_HEARTBEAT_INTERVAL
                {
                    let ping_bytes = 0;
                    if enqueue_websocket_control_raw(
                        &write_wire_tx,
                        WebSocketOutKind::Ping,
                        Message::Ping(Vec::new()),
                    )
                    .await
                    .is_err()
                    {
                        traffic.record_send_error();
                        break;
                    }
                    traffic.record_queued_raw(WebSocketOutKind::Ping, ping_bytes);
                    maybe_log_websocket_traffic(
                        peer_addr,
                        &mut traffic,
                        &mut last_traffic_log,
                        &mut connection,
                        current_websocket_watcher_counts(
                            &watched_output_sessions,
                            &watched_activity_sessions,
                            &watched_file_tree_sessions,
                            &watched_resize_sessions,
                        ),
                        false,
                    );
                    pending_pong_deadline = Some(Instant::now() + WEBSOCKET_PONG_DEADLINE);
                }
            }
            maybe_event = push_event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                idle_deadline = Instant::now() + WEBSOCKET_IDLE_TIMEOUT;
                push_drain_wake_pending = false;
                debug!(
                    peer_addr = %peer_addr,
                    event = event.label(),
                    session_id = ?event.session_id(),
                    queue_pending_before = push_event_queue.len(),
                    "websocket push event received from watcher"
                );
                push_event_queue.enqueue(event);
                if let Err(()) = drain_websocket_push_events(
                    &protocol,
                    &mut connection,
                    &mut push_event_queue,
                    &write_wire_tx,
                    &mut traffic,
                    &push_event_tx,
                    &mut push_drain_wake_pending,
                )
                .await
                {
                    traffic.record_send_error();
                    break;
                }
                maybe_log_websocket_traffic(
                    peer_addr,
                    &mut traffic,
                    &mut last_traffic_log,
                    &mut connection,
                            current_websocket_watcher_counts(
                        &watched_output_sessions,
                        &watched_activity_sessions,
                        &watched_file_tree_sessions,
                        &watched_resize_sessions,
                    ),
                    false,
                );
            }
        }
    }

    maybe_log_websocket_traffic(
        peer_addr,
        &mut traffic,
        &mut last_traffic_log,
        &mut connection,
        current_websocket_watcher_counts(
            &watched_output_sessions,
            &watched_activity_sessions,
            &watched_file_tree_sessions,
            &watched_resize_sessions,
        ),
        true,
    );

    for task in watcher_tasks {
        task.abort();
    }

    let mut protocol = protocol.lock().await;
    connection.close(&mut protocol);
    drop(protocol);
    finish_websocket_writer(write_wire_tx, writer_task).await;
    debug!("websocket connection closed and detached");
}

fn queue_websocket_push_drain_wakeup(
    queue: &SessionPushEventQueue,
    push_event_tx: &mpsc::Sender<SessionPushEvent>,
    push_drain_wake_pending: &mut bool,
) {
    if *push_drain_wake_pending {
        return;
    }
    let Some(event) = queue.peek_front() else {
        return;
    };
    // 中文注释：唤醒只把 pending 事件交回主循环，不能在当前调用栈继续递归 drain。
    // 大输出需要按预算让出调度权，避免压住输入、close 和 pong。
    if push_event_tx.try_send(event).is_ok() {
        *push_drain_wake_pending = true;
        debug!(
            event = event.label(),
            session_id = ?event.session_id(),
            queue_pending = queue.len(),
            "websocket push drain wakeup queued"
        );
    } else {
        warn!(
            event = event.label(),
            session_id = ?event.session_id(),
            queue_pending = queue.len(),
            "websocket push drain wakeup queue full"
        );
    }
}

fn websocket_push_drain_budget_exhausted(
    drained_events: usize,
    enqueued_bytes: usize,
    started_at: Instant,
) -> bool {
    drained_events >= WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK
        || enqueued_bytes >= WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK
        || started_at.elapsed() >= WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED
}

fn websocket_wire_messages_wire_len(messages: &[ProtocolWireMessage]) -> usize {
    messages
        .iter()
        .map(|message| match message {
            ProtocolWireMessage::Json(envelope) => match serde_json::to_vec(envelope) {
                Ok(raw) => raw.len(),
                Err(_) => 0,
            },
            ProtocolWireMessage::Binary(raw) => raw.len(),
        })
        .sum()
}

async fn drain_websocket_push_events(
    protocol: &SharedDaemonProtocol,
    connection: &mut ProtocolConnection,
    queue: &mut SessionPushEventQueue,
    write_wire_tx: &mpsc::Sender<WebSocketWrite>,
    traffic: &mut WebSocketTrafficCounters,
    push_event_tx: &mpsc::Sender<SessionPushEvent>,
    push_drain_wake_pending: &mut bool,
) -> Result<(), ()> {
    let started_at = Instant::now();
    let mut drained_events = 0_usize;
    let mut enqueued_bytes = 0_usize;
    while queue.has_pending() {
        debug!(
            queue_pending = queue.len(),
            drained_events, enqueued_bytes, "websocket push drain reserving writer queue"
        );
        let permit = match write_wire_tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    queue_pending = queue.len(),
                    drained_events,
                    enqueued_bytes,
                    "websocket writer queue closed while reserving push output"
                );
                return Err(());
            }
        };
        let Some(event) = queue.pop_front_after_queue_accept() else {
            break;
        };
        debug!(
            event = event.label(),
            session_id = ?event.session_id(),
            queue_pending_after_pop = queue.len(),
            "websocket push event dequeued"
        );
        let (kind, responses) = collect_websocket_push_event(protocol, connection, event).await;
        queue_deferred_output_wakeups(connection, queue);
        if responses.is_empty() {
            drained_events = drained_events.saturating_add(1);
            if websocket_push_drain_budget_exhausted(drained_events, enqueued_bytes, started_at) {
                log_websocket_push_drain_reschedule(
                    kind,
                    drained_events,
                    enqueued_bytes,
                    queue.len(),
                    started_at.elapsed(),
                );
                queue_websocket_push_drain_wakeup(queue, push_event_tx, push_drain_wake_pending);
                break;
            }
            continue;
        }
        let response_count = responses.len();
        let batch_bytes = websocket_wire_messages_wire_len(&responses);
        permit.send(WebSocketWrite::Wire {
            kind,
            messages: responses,
        });
        debug!(
            kind = kind.label(),
            response_count,
            batch_bytes,
            queue_pending = queue.len(),
            "websocket push batch accepted by writer queue"
        );
        traffic.record_out(kind, response_count, batch_bytes);
        drained_events = drained_events.saturating_add(1);
        enqueued_bytes = enqueued_bytes.saturating_add(batch_bytes);
        // 中文注释：统计记录 queue accepted，而不是 socket send completed。
        // 成功入队就是这条连接的背压边界；失败只在 writer 层关闭连接。
        if websocket_push_drain_budget_exhausted(drained_events, enqueued_bytes, started_at) {
            // 中文注释：直连 WebSocket 和 relay 使用同一类输出调度预算。
            // 一轮输出入队后主动回到 select!，让输入、cancel、close、pong 和新 attach
            // 有机会插队处理，避免多个大输出窗口一起占住 daemon 协议状态。
            log_websocket_push_drain_reschedule(
                kind,
                drained_events,
                enqueued_bytes,
                queue.len(),
                started_at.elapsed(),
            );
            queue_websocket_push_drain_wakeup(queue, push_event_tx, push_drain_wake_pending);
            break;
        }
    }
    Ok(())
}

fn log_websocket_push_drain_reschedule(
    kind: WebSocketOutKind,
    drained_events: usize,
    enqueued_bytes: usize,
    pending_events: usize,
    elapsed: Duration,
) {
    if pending_events == 0 {
        return;
    }
    debug!(
        kind = kind.label(),
        drained_events,
        enqueued_bytes,
        pending_events,
        elapsed_ms = elapsed.as_millis(),
        "websocket push drain rescheduled"
    );
}

async fn collect_websocket_push_event(
    protocol: &SharedDaemonProtocol,
    connection: &mut ProtocolConnection,
    event: SessionPushEvent,
) -> (WebSocketOutKind, Vec<ProtocolWireMessage>) {
    match event {
        SessionPushEvent::Output(session_id) => {
            let lock_started = Instant::now();
            let collect_started = Instant::now();
            let (lock_wait, messages) = {
                let mut protocol = protocol.lock().await;
                let lock_wait = lock_started.elapsed();
                // 中文注释：protocol lock 只覆盖 PTY/runtime 读取；E2EE 加密在锁外完成。
                // 这能避免某个直连 WebSocket 的大输出阻塞 relay mux 或其它直连输入。
                let messages = connection.collect_session_output_messages(
                    &mut protocol,
                    session_id,
                    OUTPUT_FLUSH_MAX_BYTES_PER_SESSION,
                );
                (lock_wait, messages)
            };
            let collect_elapsed = collect_started.elapsed();
            if lock_wait >= WEBSOCKET_SEND_DEBUG_LOG_THRESHOLD
                || collect_elapsed >= WEBSOCKET_SEND_DEBUG_LOG_THRESHOLD
            {
                debug!(
                    session_id = ?session_id,
                    lock_wait_ms = lock_wait.as_millis(),
                    collect_ms = collect_elapsed.as_millis(),
                    "websocket output collection latency"
                );
            }
            (
                WebSocketOutKind::PushOutput,
                connection.encrypt_collected_inner_messages_wire(messages),
            )
        }
        SessionPushEvent::Activity(session_id) => {
            let mut protocol = protocol.lock().await;
            (
                WebSocketOutKind::PushActivity,
                connection.read_session_activity_wire(&mut protocol, session_id),
            )
        }
        SessionPushEvent::FileTree(session_id) => {
            let messages = {
                let mut protocol = protocol.lock().await;
                connection.read_session_file_tree_update_messages(&mut protocol, session_id)
            };
            (
                WebSocketOutKind::PushFileTree,
                connection.encrypt_collected_inner_messages_wire(messages),
            )
        }
        SessionPushEvent::Resize(session_id) => {
            let messages = {
                let mut protocol = protocol.lock().await;
                connection.read_session_resize_update_messages(&mut protocol, session_id)
            };
            (
                WebSocketOutKind::PushResize,
                connection.encrypt_collected_inner_messages_wire(messages),
            )
        }
    }
}

async fn register_session_watchers(
    connection: &ProtocolConnection,
    protocol: &SharedDaemonProtocol,
    watched_output_sessions: &mut HashSet<SessionId>,
    watched_activity_sessions: &mut HashSet<SessionId>,
    watched_file_tree_sessions: &mut HashSet<SessionId>,
    watched_resize_sessions: &mut HashSet<SessionId>,
    push_event_tx: &mpsc::Sender<SessionPushEvent>,
    watcher_tasks: &mut Vec<JoinHandle<()>>,
) -> Vec<SessionId> {
    let mut initial_output_sessions = Vec::new();
    let (output_signals, activity_signals, file_tree_signals, resize_signals) = {
        let protocol = protocol.lock().await;
        (
            connection.attached_output_signals(&protocol),
            connection.session_activity_signals(&protocol),
            connection.attached_file_tree_signals(&protocol),
            connection.attached_resize_signals(&protocol),
        )
    };
    let desired_output_sessions: HashSet<_> = output_signals
        .iter()
        .map(|(session_id, _)| *session_id)
        .collect();
    let desired_activity_sessions: HashSet<_> = activity_signals
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

    if !watched_output_sessions.is_subset(&desired_output_sessions)
        || !watched_activity_sessions.is_subset(&desired_activity_sessions)
        || !watched_file_tree_sessions.is_subset(&desired_file_tree_sessions)
        || !watched_resize_sessions.is_subset(&desired_resize_sessions)
    {
        // 中文注释：切换 terminal stream 后旧 session 不应继续产生 push。
        // 一旦发现当前 watcher 集合不再是 desired 集合的子集，就整体重建本连接 watcher。
        debug!("rebuilding websocket watchers after subscription set changed");
        for task in watcher_tasks.drain(..) {
            task.abort();
        }
        watched_output_sessions.clear();
        watched_activity_sessions.clear();
        watched_file_tree_sessions.clear();
        watched_resize_sessions.clear();
    }

    for (session_id, signal) in output_signals {
        if !watched_output_sessions.insert(session_id) {
            continue;
        }
        initial_output_sessions.push(session_id);

        spawn_session_push_watcher(
            session_id,
            signal,
            SessionPushEvent::Output(session_id),
            push_event_tx,
            watcher_tasks,
        );
    }

    for (session_id, signal) in activity_signals {
        if !watched_activity_sessions.insert(session_id) {
            continue;
        }
        if watched_output_sessions.contains(&session_id) {
            continue;
        }

        spawn_session_push_watcher(
            session_id,
            signal,
            SessionPushEvent::Activity(session_id),
            push_event_tx,
            watcher_tasks,
        );
    }

    for (session_id, signal) in file_tree_signals {
        if !watched_file_tree_sessions.insert(session_id) {
            continue;
        }

        spawn_session_push_watcher(
            session_id,
            signal,
            SessionPushEvent::FileTree(session_id),
            push_event_tx,
            watcher_tasks,
        );
    }

    for (session_id, signal) in resize_signals {
        if !watched_resize_sessions.insert(session_id) {
            continue;
        }

        spawn_session_push_watcher(
            session_id,
            signal,
            SessionPushEvent::Resize(session_id),
            push_event_tx,
            watcher_tasks,
        );
    }

    initial_output_sessions
}

fn queue_initial_output_events(
    initial_output_sessions: &[SessionId],
    push_event_queue: &mut SessionPushEventQueue,
) {
    for session_id in initial_output_sessions {
        // attach/create 刚完成时 watcher 会忽略当前 watch 值；显式排一次输出读取，
        // 但让它走 push 队列，给 close、cancel、pong 等控制事件抢先处理的机会。
        push_event_queue.enqueue(SessionPushEvent::Output(*session_id));
        debug!(
            session_id = ?session_id,
            queue_pending = push_event_queue.len(),
            "websocket initial output event queued"
        );
    }
}

fn queue_deferred_output_wakeups(
    connection: &mut ProtocolConnection,
    push_event_queue: &mut SessionPushEventQueue,
) {
    for session_id in connection.take_deferred_output_wakeups() {
        // 中文注释：terminal 输出不再等待 flow credit；这里只处理 batch/transport 上限
        // 造成的后续 drain，让 input/cancel/pong 可以先被 select 处理。
        push_event_queue.enqueue(SessionPushEvent::Output(session_id));
        debug!(
            session_id = ?session_id,
            queue_pending = push_event_queue.len(),
            "websocket deferred output event queued"
        );
    }
}

fn spawn_session_push_watcher<T>(
    session_id: SessionId,
    mut signal: watch::Receiver<T>,
    event: SessionPushEvent,
    push_event_tx: &mpsc::Sender<SessionPushEvent>,
    watcher_tasks: &mut Vec<JoinHandle<()>>,
) where
    T: Clone + Send + Sync + 'static,
{
    // watch 新订阅者可能把当前版本视为“未读”；先标记已读，避免 attach 时把历史输出
    // 误推成 session_activity，导致前端一直显示 new output。
    signal.borrow_and_update();

    let push_event_tx = push_event_tx.clone();
    let min_interval = event.min_interval();
    watcher_tasks.push(tokio::spawn(async move {
        loop {
            if signal.changed().await.is_err() {
                break;
            }
            let next_event = match event {
                SessionPushEvent::Output(_) => SessionPushEvent::Output(session_id),
                SessionPushEvent::Activity(_) => SessionPushEvent::Activity(session_id),
                SessionPushEvent::FileTree(_) => SessionPushEvent::FileTree(session_id),
                SessionPushEvent::Resize(_) => SessionPushEvent::Resize(session_id),
            };
            if push_event_tx.send(next_event).await.is_err() {
                debug!(
                    event = next_event.label(),
                    session_id = ?session_id,
                    "websocket push watcher stopped because event queue closed"
                );
                break;
            }
            debug!(
                event = next_event.label(),
                session_id = ?session_id,
                "websocket push watcher enqueued event"
            );
            if let Some(interval) = min_interval {
                tokio::time::sleep(interval).await;
            }
        }
    }));
}

fn message_to_envelope(message: Message) -> Result<Option<JsonEnvelope>, ProtocolError> {
    match message {
        Message::Text(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|_| ProtocolError::InvalidEnvelope),
        Message::Binary(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|_| ProtocolError::InvalidEnvelope),
        Message::Close(_) | Message::Ping(_) | Message::Pong(_) => Ok(None),
    }
}

fn message_to_wire_message(message: Message) -> Result<Option<ProtocolWireMessage>, ProtocolError> {
    match message {
        Message::Text(raw) => serde_json::from_str(&raw)
            .map(|envelope| Some(ProtocolWireMessage::Json(envelope)))
            .map_err(|_| ProtocolError::InvalidEnvelope),
        Message::Binary(raw) => Ok(Some(ProtocolWireMessage::Binary(raw.to_vec()))),
        Message::Close(_) | Message::Ping(_) | Message::Pong(_) => Ok(None),
    }
}

fn plaintext_error(error: ProtocolError) -> JsonEnvelope {
    envelope_value(
        MessageType::Error,
        ErrorPayload {
            code: error.code().to_owned(),
            message: error.safe_message().to_owned(),
        },
    )
    .expect("error payload should serialize")
}

fn route_prelude_timeout_error() -> JsonEnvelope {
    envelope_value(
        MessageType::Error,
        ErrorPayload {
            code: "route_prelude_timeout".to_owned(),
            message: "route prelude timed out".to_owned(),
        },
    )
    .expect("route prelude timeout payload should serialize")
}

async fn enqueue_websocket_wire(
    tx: &mpsc::Sender<WebSocketWrite>,
    kind: WebSocketOutKind,
    messages: Vec<ProtocolWireMessage>,
) -> Result<(), ()> {
    // 中文注释：所有已经完成 E2EE 的业务帧都必须进入同一个 FIFO。
    // 这样 response、push_output、error 的相对顺序就和加密顺序一致，不会把 seq 打乱。
    if messages.is_empty() {
        return Ok(());
    }
    let message_count = messages.len();
    let bytes = websocket_wire_messages_wire_len(&messages);
    tx.send(WebSocketWrite::Wire { kind, messages })
        .await
        .map(|()| {
            debug!(
                kind = kind.label(),
                messages = message_count,
                bytes,
                "websocket writer queue accepted wire batch"
            );
        })
        .map_err(|_| {
            warn!(
                kind = kind.label(),
                messages = message_count,
                bytes,
                "websocket writer queue closed while enqueueing wire batch"
            );
        })
}

async fn enqueue_websocket_control_raw(
    tx: &mpsc::Sender<WebSocketWrite>,
    kind: WebSocketOutKind,
    message: Message,
) -> Result<(), ()> {
    // 中文注释：WebSocket 控制帧也走同一条 writer queue。队列满时等待容量，
    // 让当前连接整体承压，而不是再维护一条旁路控制队列。
    let message_kind = websocket_message_kind(&message);
    let bytes = websocket_message_bytes(&message);
    tx.send(WebSocketWrite::Raw { kind, message })
        .await
        .map(|()| {
            debug!(
                kind = kind.label(),
                message_kind, bytes, "websocket writer queue accepted raw frame"
            );
        })
        .map_err(|_| {
            warn!(
                kind = kind.label(),
                message_kind, bytes, "websocket writer queue closed while enqueueing raw frame"
            );
        })
}

async fn run_websocket_writer(
    peer_addr: SocketAddr,
    mut wire_rx: mpsc::Receiver<WebSocketWrite>,
    writer_failed_tx: mpsc::Sender<()>,
    mut sender: futures_util::stream::SplitSink<WebSocket, Message>,
) {
    // 中文注释：writer 只负责把有界队列里的内容顺序写入 WebSocket。
    // 成功写入不再回报；queue accepted 已经是本连接的背压边界。
    while let Some(write) = wire_rx.recv().await {
        let snapshot = write.debug_snapshot();
        debug!(
            peer_addr = %peer_addr,
            kind = snapshot.kind.label(),
            messages = snapshot.messages,
            bytes = snapshot.bytes,
            raw = snapshot.raw,
            "websocket writer dequeued frame"
        );
        if !send_websocket_write(peer_addr, &mut sender, write).await {
            let _ = writer_failed_tx.try_send(());
            break;
        }
    }
    debug!(peer_addr = %peer_addr, "websocket writer stopped");
}

async fn finish_websocket_writer(
    write_wire_tx: mpsc::Sender<WebSocketWrite>,
    writer_task: JoinHandle<()>,
) {
    // 中文注释：关闭所有 producer 后让 writer 自然 drain 已接受的队列内容。
    // queue accepted 是输出责任边界；正常收尾不能再 abort 掉已入队帧。
    drop(write_wire_tx);
    let _ = writer_task.await;
}

async fn send_websocket_write(
    peer_addr: SocketAddr,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    write: WebSocketWrite,
) -> bool {
    match write {
        WebSocketWrite::Wire { kind, messages } => {
            send_wire_messages_logged(peer_addr, sender, messages, kind.label())
                .await
                .is_ok()
        }
        WebSocketWrite::Raw { kind, message } => {
            let deadline = match kind {
                WebSocketOutKind::Pong => WEBSOCKET_PONG_DEADLINE,
                _ => WEBSOCKET_SEND_DEADLINE,
            };
            send_message_with_deadline(sender, message, deadline, "websocket control frame")
                .await
                .is_ok()
        }
    }
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
            warn!(%error, context = context, "websocket send failed");
            Err(())
        }
        Err(_) => {
            warn!(?deadline, context = context, "websocket send timed out");
            Err(())
        }
    }
}

async fn send_envelope_logged(
    peer_addr: SocketAddr,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    envelope: JsonEnvelope,
    label: &'static str,
    log_send: bool,
) -> Result<usize, ()> {
    let raw = serde_json::to_string(&envelope).map_err(|error| {
        warn!(%error, "failed to serialize websocket envelope");
    })?;
    let bytes = raw.len();
    let started_at = Instant::now();

    send_message_with_deadline(
        sender,
        Message::Text(raw),
        WEBSOCKET_SEND_DEADLINE,
        "websocket envelope",
    )
    .await?;
    if log_send {
        log_websocket_send(
            peer_addr,
            label,
            1,
            bytes,
            started_at.elapsed(),
            "websocket send batch",
        );
    }
    Ok(bytes)
}

async fn send_wire_messages_logged(
    peer_addr: SocketAddr,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    messages: Vec<ProtocolWireMessage>,
    label: &'static str,
) -> Result<usize, ()> {
    let message_count = messages.len();
    let mut bytes = 0_usize;
    let started_at = Instant::now();
    for message in messages {
        match message {
            ProtocolWireMessage::Json(envelope) => {
                bytes = bytes.saturating_add(
                    send_envelope_logged(peer_addr, sender, envelope, label, false).await?,
                );
            }
            ProtocolWireMessage::Binary(raw) => {
                let len = raw.len();
                send_message_with_deadline(
                    sender,
                    Message::Binary(raw),
                    WEBSOCKET_SEND_DEADLINE,
                    "websocket binary packet",
                )
                .await?;
                bytes = bytes.saturating_add(len);
            }
        }
    }
    log_websocket_send(
        peer_addr,
        label,
        message_count,
        bytes,
        started_at.elapsed(),
        "websocket send batch",
    );
    Ok(bytes)
}

fn log_websocket_send(
    peer_addr: SocketAddr,
    label: &str,
    envelopes: usize,
    bytes: usize,
    elapsed: Duration,
    event: &'static str,
) {
    let elapsed_ms = elapsed.as_millis();
    let promote_to_info = elapsed >= WEBSOCKET_SEND_SLOW_LOG_THRESHOLD
        || envelopes >= WEBSOCKET_SEND_INFO_BATCH_ENVELOPES
        || bytes >= WEBSOCKET_SEND_INFO_BATCH_BYTES;
    let emit_debug = elapsed >= WEBSOCKET_SEND_DEBUG_LOG_THRESHOLD
        || envelopes >= WEBSOCKET_SEND_DEBUG_BATCH_ENVELOPES
        || bytes >= WEBSOCKET_SEND_DEBUG_BATCH_BYTES;

    if promote_to_info {
        info!(
            peer_addr = %peer_addr,
            label,
            envelopes,
            bytes,
            elapsed_ms,
            "{event}"
        );
    } else if emit_debug {
        debug!(
            peer_addr = %peer_addr,
            label,
            envelopes,
            bytes,
            elapsed_ms,
            "{event}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde::Deserialize;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use termd_proto::{
        DeviceId, E2eeKeyExchangePayload, Envelope, PairAcceptPayload, PairRequestPayload,
        PublicKey, SessionCreatePayload, SessionCreatedPayload, SessionDataPayload, TerminalSize,
        UnixTimestampMillis,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::tungstenite::Message as ClientWsMessage;

    use crate::auth::current_unix_timestamp_millis;
    use crate::net::protocol::{
        ProtocolConnection, decode_payload, encrypted_frame_from_envelope, envelope_value,
    };
    use crate::net::{
        E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
    };
    use crate::pty::PtySize;
    use crate::pty::supervisor::SupervisorRestoreCandidate;
    use crate::state::{
        DaemonState, SessionStateRecord, StateStore, client_history::ClientHistoryStore,
    };
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    #[derive(Debug, Deserialize)]
    struct PairingTokenResponse {
        token: String,
        expires_at_ms: UnixTimestampMillis,
        ttl_ms: u64,
        server_id: ServerId,
        ws_url: String,
    }

    type TestWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    #[test]
    fn websocket_traffic_ignores_empty_output_pushes() {
        let mut traffic = WebSocketTrafficCounters::default();

        traffic.record_out(WebSocketOutKind::PushOutput, 0, 0);

        assert!(!traffic.has_activity());
    }

    #[test]
    fn websocket_binary_message_stays_opaque_for_protocol_layer() {
        let raw = b"TD2E-binary-frame".to_vec();
        let decoded = message_to_wire_message(Message::Binary(raw.clone().into()))
            .expect("binary websocket message should be accepted")
            .expect("binary websocket message should not be control frame");

        assert_eq!(decoded, ProtocolWireMessage::Binary(raw));
    }

    #[test]
    fn websocket_push_queue_coalesces_duplicate_pending_events() {
        let session_id = SessionId::new();
        let mut queue = SessionPushEventQueue::default();

        queue.enqueue(SessionPushEvent::Output(session_id));
        queue.enqueue(SessionPushEvent::Output(session_id));

        assert_eq!(
            queue.pop_front_after_queue_accept(),
            Some(SessionPushEvent::Output(session_id))
        );
        assert_eq!(queue.pop_front_after_queue_accept(), None);
    }

    #[test]
    fn websocket_push_wakeup_peeks_pending_event_without_draining() {
        let session_id = SessionId::new();
        let event = SessionPushEvent::Output(session_id);
        let mut queue = SessionPushEventQueue::default();
        let (tx, mut rx) = mpsc::channel(1);
        let mut wake_pending = false;

        queue.enqueue(event);
        queue_websocket_push_drain_wakeup(&queue, &tx, &mut wake_pending);
        queue_websocket_push_drain_wakeup(&queue, &tx, &mut wake_pending);

        // 中文注释：唤醒只是把 pending 事件交回主循环调度，不能同步 drain。
        // writer queue accepted 才是输出责任边界，不再等待 socket 成功发送回执。
        assert_eq!(queue.peek_front(), Some(event));
        assert_eq!(rx.try_recv(), Ok(event));
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert!(wake_pending);
    }

    #[tokio::test]
    async fn websocket_wire_queue_preserves_e2ee_sequence_order_across_kinds() {
        let (wire_tx, mut wire_rx) = mpsc::channel(4);

        enqueue_websocket_wire(
            &wire_tx,
            WebSocketOutKind::PushOutput,
            vec![ProtocolWireMessage::Binary(vec![3])],
        )
        .await
        .unwrap();
        enqueue_websocket_wire(
            &wire_tx,
            WebSocketOutKind::Response,
            vec![ProtocolWireMessage::Binary(vec![4])],
        )
        .await
        .unwrap();

        // 中文注释：PushOutput 和 Response 都是 E2EE 业务帧，不能再分队列插队。
        // 这里用 3/4 模拟已加密 sequence，保证 writer 看到的顺序就是加密顺序。
        let first = wire_rx.try_recv().unwrap();
        let second = wire_rx.try_recv().unwrap();
        match first {
            WebSocketWrite::Wire { kind, messages } => {
                assert_eq!(kind, WebSocketOutKind::PushOutput);
                assert_eq!(messages, vec![ProtocolWireMessage::Binary(vec![3])]);
            }
            other => panic!("expected first wire write, got {other:?}"),
        }
        match second {
            WebSocketWrite::Wire { kind, messages } => {
                assert_eq!(kind, WebSocketOutKind::Response);
                assert_eq!(messages, vec![ProtocolWireMessage::Binary(vec![4])]);
            }
            other => panic!("expected second wire write, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn websocket_push_drain_requeues_when_data_queue_is_full() {
        let protocol = test_protocol("websocket-data-queue-full");
        let (mut connection, _) = {
            let protocol = protocol.lock().await;
            protocol.start_connection()
        };
        let (write_wire_tx, mut _write_wire_rx) = mpsc::channel(1);
        let event = SessionPushEvent::Output(SessionId::new());
        let mut queue = SessionPushEventQueue::default();
        queue.enqueue(event);
        write_wire_tx
            .try_send(WebSocketWrite::Wire {
                kind: WebSocketOutKind::PushOutput,
                messages: Vec::new(),
            })
            .unwrap();

        let (push_event_tx, _push_event_rx) = mpsc::channel(1);
        let mut wake_pending = false;
        let mut traffic = WebSocketTrafficCounters::default();
        let mut drain = Box::pin(drain_websocket_push_events(
            &protocol,
            &mut connection,
            &mut queue,
            &write_wire_tx,
            &mut traffic,
            &push_event_tx,
            &mut wake_pending,
        ));

        tokio::select! {
            result = &mut drain => panic!("drain should wait for writer queue capacity, got {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        drop(drain);
        assert_eq!(queue.peek_front(), Some(event));

        let _queued = _write_wire_rx.try_recv().unwrap();
        drain_websocket_push_events(
            &protocol,
            &mut connection,
            &mut queue,
            &write_wire_tx,
            &mut traffic,
            &push_event_tx,
            &mut wake_pending,
        )
        .await
        .unwrap();

        // 中文注释：queue accepted 是输出责任边界。容量释放后事件才会被弹出，
        // 避免 writer queue 满时提前消费 daemon terminal cache。
        assert_eq!(queue.peek_front(), None);
    }

    #[test]
    fn websocket_push_drain_budget_limits_hot_loop() {
        assert!(!websocket_push_drain_budget_exhausted(
            1,
            1024,
            Instant::now()
        ));
        assert!(websocket_push_drain_budget_exhausted(
            WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK,
            0,
            Instant::now()
        ));
        assert!(websocket_push_drain_budget_exhausted(
            1,
            WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK,
            Instant::now()
        ));
        assert!(websocket_push_drain_budget_exhausted(
            1,
            0,
            Instant::now() - WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED
        ));
    }

    #[tokio::test]
    async fn websocket_push_drain_stops_after_event_budget_and_wakes_once() {
        let protocol = test_protocol("websocket-drain-budget");
        let (mut connection, _) = {
            let protocol = protocol.lock().await;
            protocol.start_connection()
        };
        let (write_wire_tx, _write_wire_rx) = mpsc::channel(WEBSOCKET_WIRE_QUEUE_CAPACITY);
        let (push_event_tx, mut push_event_rx) = mpsc::channel(8);
        let mut queue = SessionPushEventQueue::default();
        let mut wake_pending = false;
        let mut traffic = WebSocketTrafficCounters::default();
        let events: Vec<_> = (0..WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK + 2)
            .map(|_| SessionPushEvent::Output(SessionId::new()))
            .collect();
        for event in &events {
            queue.enqueue(*event);
        }

        drain_websocket_push_events(
            &protocol,
            &mut connection,
            &mut queue,
            &write_wire_tx,
            &mut traffic,
            &push_event_tx,
            &mut wake_pending,
        )
        .await
        .unwrap();

        // 中文注释：即使当前事件没有真实输出，direct drain 也必须按事件预算让出调度权。
        // 否则多窗口快速切换时，一个连接能在同一轮 select 里清完大量 session 事件。
        assert!(wake_pending);
        assert!(queue.has_pending());
        assert_eq!(
            push_event_rx.try_recv(),
            Ok(events[WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK])
        );
        assert_eq!(
            push_event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );
    }

    fn test_config(name: &str) -> DaemonConfig {
        DaemonConfig::default_for_state_path(std::env::temp_dir().join(format!(
            "termd-server-test-{}-{}-{name}.json",
            std::process::id(),
            current_unix_timestamp_millis().0
        )))
    }

    fn test_protocol(name: &str) -> SharedDaemonProtocol {
        default_protocol(test_config(name))
    }

    #[test]
    fn missing_runtime_rows_are_adopted_from_live_supervisors_before_cleanup() {
        let session_id = SessionId::new();
        let socket_path = PathBuf::from(format!(
            "/var/lib/termd/termd-supervisors/{}.sock",
            session_id.0
        ));
        let mut state = crate::state::DaemonState::default();
        let candidates = vec![SupervisorRestoreCandidate {
            session_id: session_id.0.to_string(),
            socket_path: socket_path.clone(),
            supervisor_pid: 4242,
            size: PtySize::with_pixels(35, 120, 1600, 1000),
        }];

        let adopted = adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            candidates,
            UnixTimestampMillis(12_345),
        );

        assert_eq!(adopted, 1);
        assert_eq!(state.sessions.len(), 1);
        let adopted_session = &state.sessions[0];
        assert_eq!(adopted_session.session_id, session_id);
        assert_eq!(adopted_session.state, SessionState::Running);
        assert_eq!(adopted_session.size.rows, 35);
        assert_eq!(adopted_session.size.cols, 120);
        assert_eq!(adopted_session.size.pixel_width, 1600);
        assert_eq!(adopted_session.size.pixel_height, 1000);
        assert_eq!(adopted_session.created_at_ms, UnixTimestampMillis(12_345));
        assert!(adopted_session.restore_info.is_some());
    }

    #[test]
    fn startup_prunes_closed_rows_without_live_supervisors() {
        let state_dir = std::env::temp_dir().join(format!(
            "termd-server-startup-prune-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("daemon-state.json");
        let session_id = SessionId::new();
        let running_state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_000),
                restore_info: Some(crate::pty::PtyRestoreInfo::UnixSocket {
                    socket_path: PathBuf::from("/tmp/orphan.sock"),
                    supervisor_pid: 123,
                    supervisor_status: crate::pty::PtySupervisorStatus::Running,
                }),
            }],
        };
        StateStore::save(&state_path, &running_state).unwrap();
        StateStore::record_runtime_session_closed(
            &state_path,
            session_id,
            UnixTimestampMillis(2_000),
        )
        .unwrap();
        let mut history = ClientHistoryStore::open(&state_path).unwrap();
        history
            .record_session_created(
                session_id,
                SessionState::Running,
                termd_proto::TerminalSize::new(24, 80),
                Some("closed shell"),
                "/tmp",
                UnixTimestampMillis(1_000),
            )
            .unwrap();
        history
            .record_session_closed(session_id, UnixTimestampMillis(2_000))
            .unwrap();
        drop(history);

        let _protocol =
            try_default_protocol(DaemonConfig::default_for_state_path(&state_path)).unwrap();

        let loaded = StateStore::load(&state_path).unwrap();
        assert!(loaded.sessions.is_empty(), "{:?}", loaded.sessions);
        let history = ClientHistoryStore::open(&state_path).unwrap();
        assert!(
            history
                .session_record_including_closed(session_id)
                .unwrap()
                .is_none()
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn closed_runtime_rows_are_repaired_from_live_supervisors_before_cleanup() {
        let session_id = SessionId::new();
        let socket_path = PathBuf::from(format!(
            "/var/lib/termd/termd-supervisors/{}.sock",
            session_id.0
        ));
        let mut state = crate::state::DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Closed,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_500),
                restore_info: None,
            }],
        };
        let candidates = vec![SupervisorRestoreCandidate {
            session_id: session_id.0.to_string(),
            socket_path: socket_path.clone(),
            supervisor_pid: 4242,
            size: PtySize::with_pixels(35, 120, 1600, 1000),
        }];

        let adopted = adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            candidates,
            UnixTimestampMillis(12_345),
        );

        assert_eq!(adopted, 1);
        assert_eq!(state.sessions.len(), 1);
        let repaired_session = &state.sessions[0];
        assert_eq!(repaired_session.session_id, session_id);
        assert_eq!(repaired_session.state, SessionState::Running);
        assert_eq!(repaired_session.size.rows, 35);
        assert_eq!(repaired_session.size.cols, 120);
        assert_eq!(repaired_session.created_at_ms, UnixTimestampMillis(1_000));
        assert_eq!(repaired_session.updated_at_ms, UnixTimestampMillis(12_345));
        assert!(repaired_session.restore_info.is_some());
    }

    #[test]
    fn router_exposes_healthz_and_ws_routes() {
        let protocol = test_protocol("router");
        let _router = router(protocol, false);
    }

    #[tokio::test]
    async fn web_fallback_is_opt_in() {
        let protocol = test_protocol("web-fallback");
        let disabled_response = router(protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(disabled_response.status(), StatusCode::NOT_FOUND);

        let enabled_response = router(protocol, true)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(enabled_response.status(), StatusCode::OK);
    }

    struct RawHttpResponse {
        status: u16,
        body: String,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_pairing_token_endpoint_issues_runtime_token() {
        let protocol = test_protocol("local-pairing-token");
        let server_id = protocol.lock().await.server_id();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });
        let response = tokio::task::spawn_blocking(move || post_pairing_token(addr))
            .await
            .unwrap();
        server.abort();

        assert_eq!(response.status, 200);
        let payload: PairingTokenResponse = serde_json::from_str(&response.body).unwrap();

        assert!(payload.token.starts_with("termd-pair-"));
        assert_eq!(payload.ttl_ms, DaemonConfig::default().pairing_token_ttl_ms);
        assert!(payload.expires_at_ms.0 > current_unix_timestamp_millis().0);
        assert_eq!(payload.server_id, server_id);
        assert_eq!(payload.ws_url, "ws://127.0.0.1:8765/ws");
        assert!(!response.body.contains("server_private_key"));
        assert!(!response.body.contains("terminal sentinel"));

        let pair_accept = pair_device_with_http_token(protocol, payload.token).await;
        assert_eq!(pair_accept.server_id, server_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_pairing_token_endpoint_returns_configured_relay_client_url() {
        let mut config = test_config("local-pairing-token-relay-url");
        config.relay_endpoints = vec!["wss://relay.example/ws".to_owned()];
        config.default_pairing_ws_url = "wss://relay.example/ws".to_owned();
        let protocol = default_protocol(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });
        let response = tokio::task::spawn_blocking(move || post_pairing_token(addr))
            .await
            .unwrap();
        server.abort();

        assert_eq!(response.status, 200);
        let payload: PairingTokenResponse = serde_json::from_str(&response.body).unwrap();
        assert_eq!(payload.ws_url, "wss://relay.example/ws");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_route_prelude_times_out_before_first_message() {
        let protocol = test_protocol("websocket-route-prelude-timeout");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let message = timeout(
            ROUTE_PRELUDE_TIMEOUT + Duration::from_secs(2),
            socket.next(),
        )
        .await
        .expect("daemon should reject missing route_hello before the outer test timeout")
        .expect("daemon should send a route prelude error")
        .expect("route prelude error should be a websocket message");
        let ClientWsMessage::Text(raw) = message else {
            panic!("expected plaintext route prelude error, got {message:?}");
        };
        let envelope: JsonEnvelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(envelope.kind, MessageType::Error);
        let error: ErrorPayload = decode_payload(envelope.payload).unwrap();
        assert_eq!(error.code, "route_prelude_timeout");

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_route_prelude_ping_is_written_through_writer_queue() {
        let protocol = test_protocol("websocket-route-prelude-ping");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        socket
            .send(ClientWsMessage::Ping(vec![4, 2]))
            .await
            .unwrap();
        let message = timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("route prelude ping should be answered by the writer queue")
            .expect("websocket should remain open")
            .expect("pong frame should be readable");

        // 中文注释：route prelude 发生在认证和 initial 之前，也必须复用同一条 writer queue。
        // 这个断言防止后续重新引入握手阶段的旁路直写。
        assert_eq!(message, ClientWsMessage::Pong(vec![4, 2].into()));

        server.abort();
    }

    #[test]
    fn websocket_timeout_policy_matches_browser_lifecycle() {
        assert_eq!(ROUTE_PRELUDE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(WEBSOCKET_HEARTBEAT_INTERVAL, Duration::from_secs(10));
        assert!(!websocket_idle_timeout_enabled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_writer_streams_frames_without_send_ack() {
        let expected_frames = 300_usize;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/ws",
            get(move |websocket: WebSocketUpgrade| async move {
                websocket.on_upgrade(move |socket| async move {
                    let peer_addr = SocketAddr::from(([127, 0, 0, 1], 0));
                    let (sender, mut receiver) = socket.split();
                    let reader_task = tokio::spawn(async move {
                        while let Some(message) = receiver.next().await {
                            if message.is_err() {
                                break;
                            }
                        }
                    });
                    let (wire_tx, wire_rx) = mpsc::channel(WEBSOCKET_WIRE_QUEUE_CAPACITY);
                    let (failure_tx, mut failure_rx) = mpsc::channel(1);
                    let writer_task =
                        tokio::spawn(run_websocket_writer(peer_addr, wire_rx, failure_tx, sender));

                    for index in 0..expected_frames {
                        wire_tx
                            .send(WebSocketWrite::Wire {
                                kind: WebSocketOutKind::Initial,
                                messages: vec![ProtocolWireMessage::Binary(
                                    format!("frame-{index}").into_bytes(),
                                )],
                            })
                            .await
                            .unwrap();
                    }

                    drop(wire_tx);
                    let _ = writer_task.await;
                    assert!(failure_rx.try_recv().is_err());
                    // 中文注释：测试只验证 writer 不再依赖成功发送回执。
                    // 保持连接短暂存活，让客户端把已经写入 socket 的帧读完，避免 drop
                    // websocket 时的关闭握手噪声污染断言。
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    reader_task.abort();
                })
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        // 中文注释：writer 成功发送没有任何回执消费者。
        // 真实 WebSocket 字节流不能被诊断/记账通道反向限制。
        let reached = timeout(Duration::from_secs(2), async {
            let mut seen = 0_usize;
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                match message {
                    ClientWsMessage::Text(_) | ClientWsMessage::Binary(_) => {
                        seen = seen.saturating_add(1);
                        if seen >= expected_frames {
                            return seen;
                        }
                    }
                    ClientWsMessage::Ping(payload) => {
                        socket.send(ClientWsMessage::Pong(payload)).await.unwrap();
                    }
                    ClientWsMessage::Pong(_) | ClientWsMessage::Frame(_) => {}
                    ClientWsMessage::Close(_) => break,
                }
            }
            seen
        })
        .await
        .expect("websocket writer should keep writing without send ack");
        assert_eq!(reached, expected_frames);

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_pushes_session_output_without_client_poll_frame() {
        let protocol = test_protocol("websocket-push");
        let server_id = protocol.lock().await.server_id();
        let pairing_token = {
            protocol
                .lock()
                .await
                .issue_pairing_token(current_unix_timestamp_millis())
                .unwrap()
                .token()
                .clone()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        send_ws_route_hello(&mut socket, server_id).await;
        let hello = read_ws_envelope(&mut socket).await;
        assert_eq!(hello.kind, MessageType::Hello);
        let key_exchange = read_ws_envelope(&mut socket).await;
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);
        let daemon_exchange: E2eeKeyExchangePayload = decode_payload(key_exchange.payload).unwrap();
        let device_id = DeviceId::new();
        let mut device_session = open_client_e2ee(&mut socket, daemon_exchange, device_id).await;

        send_encrypted_ws(
            &mut socket,
            &mut device_session,
            envelope_value(
                MessageType::PairRequest,
                PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("ed25519-v1:test-device-key".to_owned()),
                    token: pairing_token,
                    nonce: termd_proto::Nonce("push-test-pairing-nonce".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap(),
        )
        .await;
        let pair_accept = read_encrypted_ws(&mut socket, &mut device_session).await;
        assert_eq!(pair_accept.kind, MessageType::PairAccept);

        send_encrypted_ws(
            &mut socket,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec![
                        "sh".to_owned(),
                        "-lc".to_owned(),
                        "sleep 0.15; printf pushed-output".to_owned(),
                    ],
                    size: TerminalSize::default(),
                },
            )
            .unwrap(),
        )
        .await;
        let created = read_encrypted_ws(&mut socket, &mut device_session).await;
        assert_eq!(created.kind, MessageType::SessionCreated);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        // 这里不再向 WebSocket 发送 ping 或任意业务帧；PTY 后续输出必须由 daemon 主动推送。
        // 等待窗口需要覆盖 CI 或本地 workspace 并发测试时的 PTY 进程启动抖动，
        // 这个值不是产品 WebSocket 的超时语义。
        let pushed = timeout(
            Duration::from_secs(8),
            read_encrypted_ws(&mut socket, &mut device_session),
        )
        .await
        .expect("daemon should push PTY output without client polling");
        assert_eq!(pushed.kind, MessageType::SessionData);
        let payload: SessionDataPayload = decode_payload(pushed.payload).unwrap();
        assert_eq!(payload.session_id, created_payload.session_id);
        let output = base64::engine::general_purpose::STANDARD
            .decode(payload.data_base64)
            .unwrap();
        assert_eq!(output, b"pushed-output");

        server.abort();
    }

    #[tokio::test]
    async fn session_push_watcher_ignores_initial_watch_value() {
        let session_id = SessionId::new();
        let (signal_tx, signal_rx) = watch::channel(41_u64);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        let mut watcher_tasks = Vec::new();

        spawn_session_push_watcher(
            session_id,
            signal_rx,
            SessionPushEvent::Activity(session_id),
            &event_tx,
            &mut watcher_tasks,
        );

        // 新建 watcher 时的当前值只是历史状态，不应立刻变成前端的 new output。
        assert!(
            timeout(Duration::from_millis(80), event_rx.recv())
                .await
                .is_err()
        );

        signal_tx.send(42).unwrap();
        let pushed = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("watcher should push after a real signal change")
            .expect("push channel should remain open");
        assert_eq!(pushed, SessionPushEvent::Activity(session_id));

        for task in watcher_tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn session_activity_push_watcher_coalesces_frequent_output_signals() {
        let session_id = SessionId::new();
        let (signal_tx, signal_rx) = watch::channel(1_u64);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        let mut watcher_tasks = Vec::new();

        spawn_session_push_watcher(
            session_id,
            signal_rx,
            SessionPushEvent::Activity(session_id),
            &event_tx,
            &mut watcher_tasks,
        );

        signal_tx.send(2).unwrap();
        let first = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("first activity should be pushed immediately")
            .expect("push channel should remain open");
        assert_eq!(first, SessionPushEvent::Activity(session_id));

        signal_tx.send(3).unwrap();
        signal_tx.send(4).unwrap();
        signal_tx.send(5).unwrap();

        // 高频后台输出只需要一个“有新输出”提示；250ms 合并窗口内不能继续刷固定小包。
        assert!(
            timeout(SESSION_ACTIVITY_PUSH_MIN_INTERVAL / 2, event_rx.recv())
                .await
                .is_err()
        );

        let second = timeout(SESSION_ACTIVITY_PUSH_MIN_INTERVAL * 2, event_rx.recv())
            .await
            .expect("coalesced activity should be pushed after the throttle window")
            .expect("push channel should remain open");
        assert_eq!(second, SessionPushEvent::Activity(session_id));
        assert!(
            timeout(Duration::from_millis(80), event_rx.recv())
                .await
                .is_err()
        );

        for task in watcher_tasks {
            task.abort();
        }
    }

    #[test]
    fn local_pairing_token_peer_check_rejects_non_loopback_peer() {
        assert!(is_loopback_peer(SocketAddr::from(([127, 0, 0, 1], 34_567))));
        assert!(is_loopback_peer(SocketAddr::from((
            [0, 0, 0, 0, 0, 0, 0, 1],
            34_567
        ))));
        assert!(!is_loopback_peer(SocketAddr::from((
            [192, 0, 2, 10],
            34_567
        ))));
    }

    #[test]
    fn websocket_transport_limits_keep_frames_smaller_than_messages() {
        assert_eq!(WEBSOCKET_MAX_FRAME_SIZE, 1024 * 1024);
        assert_eq!(WEBSOCKET_MAX_MESSAGE_SIZE, 4 * 1024 * 1024);
        assert!(WEBSOCKET_MAX_FRAME_SIZE <= WEBSOCKET_MAX_MESSAGE_SIZE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tls_listener_serves_healthz_without_touching_protocol_payloads() {
        let (cert_path, key_path) = write_test_tls_files("healthz");
        let tls_paths = TlsPaths::new(&cert_path, &key_path);
        let protocol = test_protocol("tls-healthz");
        let server_id = protocol.lock().await.server_id();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_tls_listener(listener, server_protocol, tls_paths, false).await;
        });

        let response = tls_healthz_request(addr, &cert_path).await;
        server.abort();
        fs::remove_file(cert_path).ok();
        fs::remove_file(key_path).ok();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"status\":\"ok\""));
        assert!(response.contains(&server_id.0.to_string()));
    }

    #[test]
    fn tls_paths_debug_and_invalid_key_errors_do_not_leak_key_material() {
        let (cert_path, key_path) = write_test_tls_files("invalid-key");
        fs::write(&key_path, "not a private key\n").unwrap();
        let tls_paths = TlsPaths::new(&cert_path, &key_path);

        let error = load_rustls_server_config(&tls_paths).unwrap_err();
        let rendered_error = error.to_string();
        let rendered_paths = format!("{tls_paths:?}");

        assert!(matches!(
            error,
            ServerError::MissingTlsPrivateKey | ServerError::TlsPrivateKey(_)
        ));
        assert!(!rendered_paths.contains("termd-test-tls-invalid-key-key"));
        assert!(!rendered_error.contains("not a private key"));
        fs::remove_file(cert_path).ok();
        fs::remove_file(key_path).ok();
    }

    fn post_pairing_token(addr: SocketAddr) -> RawHttpResponse {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let request = format!(
            "POST /local/pairing-token HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();

        let mut raw_response = String::new();
        stream.read_to_string(&mut raw_response).unwrap();
        let (head, body) = raw_response.split_once("\r\n\r\n").unwrap();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();

        RawHttpResponse {
            status,
            body: body.to_owned(),
        }
    }

    async fn read_ws_envelope(socket: &mut TestWs) -> JsonEnvelope {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            match message {
                ClientWsMessage::Text(raw) => return serde_json::from_str(&raw).unwrap(),
                ClientWsMessage::Binary(raw) => return serde_json::from_slice(&raw).unwrap(),
                ClientWsMessage::Ping(payload) => {
                    socket.send(ClientWsMessage::Pong(payload)).await.unwrap();
                }
                ClientWsMessage::Pong(_) => continue,
                ClientWsMessage::Close(frame) => panic!("websocket closed unexpectedly: {frame:?}"),
                ClientWsMessage::Frame(_) => continue,
            }
        }
    }

    async fn send_ws_route_hello(socket: &mut TestWs, server_id: ServerId) {
        let envelope = Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role: RouteRole::Client,
                protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                nonce: termd_proto::Nonce("route-test-nonce".to_owned()),
                route_generation: None,
                timestamp_ms: current_unix_timestamp_millis(),
            },
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        socket.send(ClientWsMessage::Text(raw)).await.unwrap();

        let ready = read_ws_envelope(socket).await;
        assert_eq!(ready.kind, MessageType::RouteReady);
        let payload: RouteReadyPayload = decode_payload(ready.payload).unwrap();
        assert_eq!(payload.server_id, server_id);
        assert_eq!(payload.role, RouteRole::Client);
    }

    async fn send_ws_envelope(socket: &mut TestWs, envelope: JsonEnvelope) {
        let raw = serde_json::to_string(&envelope).unwrap();
        socket.send(ClientWsMessage::Text(raw)).await.unwrap();
    }

    async fn open_client_e2ee(
        socket: &mut TestWs,
        daemon_exchange: E2eeKeyExchangePayload,
        device_id: DeviceId,
    ) -> E2eeSession {
        let daemon_public_key = E2eePeerPublicKey::try_from(&daemon_exchange.public_key).unwrap();
        let device_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            daemon_exchange.server_id,
            device_id,
            daemon_public_key,
            device_keypair.public_key(),
        );
        let device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            daemon_public_key,
            context,
        )
        .unwrap();
        send_ws_envelope(
            socket,
            envelope_value(
                MessageType::E2eeKeyExchange,
                E2eeKeyExchangePayload::new(
                    daemon_exchange.server_id,
                    device_id,
                    device_keypair.public_key_wire(),
                    termd_proto::Nonce("push-test-e2ee-nonce".to_owned()),
                    UnixTimestampMillis(1_000),
                ),
            )
            .unwrap(),
        )
        .await;
        device_session
    }

    async fn send_encrypted_ws(
        socket: &mut TestWs,
        device_session: &mut E2eeSession,
        inner: JsonEnvelope,
    ) {
        let frame = device_session.encrypt_json_payload(&inner).unwrap();
        send_ws_envelope(
            socket,
            envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
        )
        .await;
    }

    async fn read_encrypted_ws(
        socket: &mut TestWs,
        device_session: &mut E2eeSession,
    ) -> JsonEnvelope {
        let outer = read_ws_envelope(socket).await;
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_session.decrypt_json_payload(&frame).unwrap()
    }

    async fn tls_healthz_request(addr: SocketAddr, cert_path: &PathBuf) -> String {
        let mut root_store = rustls::RootCertStore::empty();
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for cert in certs {
            root_store.add(cert).unwrap();
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from("localhost")
            .unwrap()
            .to_owned();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = connector.connect(server_name, tcp).await.unwrap();
        let request = format!(
            "GET /healthz HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
            port = addr.port()
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    fn write_test_tls_files(name: &str) -> (PathBuf, PathBuf) {
        let cert_path = std::env::temp_dir().join(format!(
            "termd-test-tls-{name}-cert-{}-{}.pem",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        let key_path = std::env::temp_dir().join(format!(
            "termd-test-tls-{name}-key-{}-{}.pem",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::write(&cert_path, TEST_TLS_CERT_PEM).unwrap();
        fs::write(&key_path, TEST_TLS_KEY_PEM).unwrap();
        (cert_path, key_path)
    }

    const TEST_TLS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDHzCCAgegAwIBAgIUFT0JPphPVviedOwVfBgtvRlWaBswDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUwNzAzNDYxM1oXDTM2MDUw
NDAzNDYxM1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAp1LIkvOYe7VEamUgwSGpS3K9bH7DTl7sZXZLK4H4S3Ik
/68PSKWs8k+J079wrdq7Pft2u+NMACqwWK4uO30NetgQPGLB+awxqgLXyxyouTNp
XSX30gkxG1WhRWLq0JTtHZM86cFH3wZkrNIM6vzCGh5F/azICCkMyfoUJOkNezk2
T3nagv4/BeT/IDVNMEjRstwDGuuyOcKnvzUGtgwvvYbXuHmn956vAc7As3jAQNm1
eTFcg4FHzwDT5ZCYbeXeHGVtF+t+MXpbU9fbYncwLQNznni3Ngvg39XsEpsh17/I
shjHxjyJPs8Wx/TerRJ/frLcxvdFse044YcMZIQ9zQIDAQABo2kwZzAdBgNVHQ4E
FgQUVgawzOdJe6rn6Qc8o7sGNCOSJZcwHwYDVR0jBBgwFoAUVgawzOdJe6rn6Qc8
o7sGNCOSJZcwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAkGA1UdEwQCMAAw
DQYJKoZIhvcNAQELBQADggEBAEm25sfAoFRwcXTGJOfhEo9GM6JDESMxulolgR+4
IiwniOYUXvK5e51mszNzxu4AsG9OO4+myqEE0AXrhgG7kjFvUWwOVQ4wgwCUUfbj
qRpnH5SRYaKqQMJviz7adU0biGyRBN7+6YChZW8XEEE7+lGpDw979URChb/shtX7
Yb9UYaOsqvLRh+MHXMfZMPTawI1o5x6oar1a6D3SswB9omWPQABuFXeJeZcK4B/0
PEx176/dWuU6shATtBw9s3r4pJTJ5H+9awx7xyS9WYiVyt9SRxppJiwAPU9mS1Sa
T+luYJ3JUrIbrKq4qET6e3ut8nJZcnJbryvWVpegnuNiH6k=
-----END CERTIFICATE-----"#;

    const TEST_TLS_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCnUsiS85h7tURq
ZSDBIalLcr1sfsNOXuxldksrgfhLciT/rw9IpazyT4nTv3Ct2rs9+3a740wAKrBY
ri47fQ162BA8YsH5rDGqAtfLHKi5M2ldJffSCTEbVaFFYurQlO0dkzzpwUffBmSs
0gzq/MIaHkX9rMgIKQzJ+hQk6Q17OTZPedqC/j8F5P8gNU0wSNGy3AMa67I5wqe/
NQa2DC+9hte4eaf3nq8BzsCzeMBA2bV5MVyDgUfPANPlkJht5d4cZW0X634xeltT
19tidzAtA3OeeLc2C+Df1ewSmyHXv8iyGMfGPIk+zxbH9N6tEn9+stzG90Wx7Tjh
hwxkhD3NAgMBAAECggEABMD/Xd156Zne1b8FzTbtnm0mIJ0BY4qi4McZn6TTryER
GAqbPo8meMP1wIRh6S6bv0kTuIbes+qClCJuwdXtuh3FaFHN/Q/9YT0vcF/iE1D4
n2LixZ7pPEOUj2oeDcsNaZezVVjed+GwnpBhOZPw19kgV/K+xCyWZm6qf9n3Phb4
Pg9ODsq3+45cjk10Qvk+VWva1xcw8qHOpHbTLguZ3e13rL9HXbaZAfFvKGpDhzpX
m7dZ7jOqnpZt9oll8Ean2SIOfhQdACcsuz+FDIYVj1PufA3WlOeGq4gAfoBKGUNb
OFp49W0MHhSH/kmwhz9lF83okXqYJtZtxXGMiQOhKQKBgQDf4E2/BbcePEhdnMkq
wTygBN+eEyZcN5nPnNZZ8wefaLSoO3BMbkjyjr0kPQnN/FCFMWr2Rs0ga3kCN/rr
985D+DwObOSXtYBa16+w0bHoKOrxs27tX1Vnaj2djeTZggK/2k5l5YTcxrL+dSQI
LnYowViOacuaxcqy0nzRxQamowKBgQC/VRyxVh/5tB3aV2zhwZuM4RrhdpSpExql
Ohc7FAcM9X8ywjLc6ZSbGnd5j894P+EQpoJBLVxTExgasCWxuwdck4nv1dboGPZO
PodEIcz4FGOZ177oiJsJH/xkuNlliyh7i/Cyu97IXIXzFupMVEaAGIGTd2h8zhU9
wiQUUwaAzwKBgG8P14HsU+ur/Dp0jVeohWrdABJrbZxR+PwF0lDNP/rU9sp+sjc4
fvfV1/8iSLrncQqieW2zsg9jQaTYIKLvTGRrwV9mpgCdChAG8CHH5XpG0kcVvPIF
WVj0W5zNx7ofxT1oD3x9YGwmJqYVdsqYQgX15PjBg0BE30nXIhTuqV4BAoGAcWdF
BmcBtMLpHszKoFRcmfeiMxhRrJTCKkRwGHgaZbfsmG06MG3RwszBG6/9TEywXWoT
sgXsvuCGXOsirGEqT9iy3RBlvFNvSZkOG3fdQPz0u+6AHNs66QGoWxqk3+bHK9MZ
6xYnSaJtUlO2s18QGkRsKLeRmsebF2vGbrV3GUkCgYAT5lgVHUx435Zy9mOgWCEl
4OLdzEEZm8OmMiRDzgxHs0Nx4zCUYZRf5HaHUhz936R8Ez0DVCj1GAdQjkV1kCEI
joi6qSEnJBpLL35fFZfHkF1jBOfv8otRgWJuJwyit3B7LR89GAw2VgZWu03QugPN
zZZR5LzKVu9X7paftR7K8Q==
-----END PRIVATE KEY-----"#;

    async fn pair_device_with_http_token(
        protocol: SharedDaemonProtocol,
        token: String,
    ) -> PairAcceptPayload {
        let mut protocol = protocol.lock().await;
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let mut device_session =
            open_test_e2ee(&mut protocol, &mut connection, device_id, &device_keypair);
        let pair_request = envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id,
                device_public_key: PublicKey("ed25519-v1:test-device-key".to_owned()),
                token: termd_proto::PairingToken(token),
                nonce: termd_proto::Nonce("nonce-from-http-token-test".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let frame = device_session.encrypt_json_payload(&pair_request).unwrap();
        let responses = connection.handle_wire_envelope(
            &mut protocol,
            envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
        );

        let response_frame =
            encrypted_frame_from_envelope(responses.into_iter().next().unwrap()).unwrap();
        let response = device_session
            .decrypt_json_payload::<JsonEnvelope>(&response_frame)
            .unwrap();

        assert_eq!(response.kind, MessageType::PairAccept);
        assert!(connection.is_authenticated());
        decode_payload(response.payload).unwrap()
    }

    fn open_test_e2ee(
        protocol: &mut DefaultDaemonProtocol,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
        device_keypair: &E2eeKeyPair,
    ) -> E2eeSession {
        let context = E2eeSessionContext::new(
            protocol.server_id(),
            device_id,
            protocol.e2ee_public_key(),
            device_keypair.public_key(),
        );
        let device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            device_keypair,
            protocol.e2ee_public_key(),
            context,
        )
        .unwrap();
        let handshake = envelope_value(
            MessageType::E2eeKeyExchange,
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_keypair.public_key_wire(),
                termd_proto::Nonce("nonce-e2ee-test".to_owned()),
                UnixTimestampMillis(1_000),
            ),
        )
        .unwrap();

        let responses = connection.handle_wire_envelope(protocol, handshake);
        assert!(responses.is_empty());
        device_session
    }
}
