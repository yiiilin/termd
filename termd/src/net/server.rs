//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth、session 和 E2EE
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

mod recovery;

use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::net::{AddrParseError, IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use serde::Serialize;
use termd_proto::{
    ErrorPayload, HttpE2eeAuthPayload, MessageType, PROTOCOL_PACKET_VERSION, PairingToken,
    ProtocolVersion, PublicKey, RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
    SessionAttachPayload, SessionFileHttpDownloadPayload, SessionFileHttpUploadStreamPayload,
    SessionFileUploadPayload, SessionId, SessionState, Signature, UnixTimestampMillis,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tower::ServiceExt as _;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, info, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::DaemonConfig;
use crate::pty::PtyRestoreInfo;
use crate::pty::supervisor::SupervisorPtyBackend;
use crate::state::{StateError, StateStore};

use super::protocol::{
    DaemonProtocol, JsonEnvelope, ProtocolConnection, ProtocolConnectionDebugSnapshot,
    ProtocolConnectionDebugTraffic, ProtocolError, ProtocolWireMessage, SessionFileHttpUploadBegin,
    SessionFileHttpUploadCommit, cleanup_persisted_session_file_http_uploads, decode_payload,
    envelope_value, session_file_http_upload_chunks_len, write_session_file_http_upload_files,
};
use super::signature::Ed25519SignatureVerifier;
use super::{decode_binary_encrypted_frame, encode_binary_encrypted_frame};
#[cfg(test)]
pub(crate) use recovery::adopt_or_repair_runtime_sessions_from_supervisors;
use recovery::warn_about_orphaned_supervisors;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 512 * 1024;
// transport 超时只关闭当前 WebSocket 连接；session/supervisor 仍由协议和 PTY 层保持持久。
const ROUTE_PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_SEND_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_PONG_DEADLINE: Duration = Duration::from_secs(10);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
// direct WebSocket 与 relay 传输保持同量级限制；终端 snapshot 可能是数 MB 的
// 单个 E2EE binary frame，过小的 frame limit 会把正常重连误判为异常大消息。
const WEBSOCKET_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
const HTTP_E2EE_INIT_MAX_BYTES: usize = 1024 * 1024;
#[cfg(not(test))]
const HTTP_E2EE_SHORT_BODY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HTTP_E2EE_SHORT_BODY_TIMEOUT: Duration = Duration::from_millis(50);
const HTTP_E2EE_FRAME_LEN_BYTES: usize = 4;
const HTTP_E2EE_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
// 中文注释：HTTP body 是流式输入，必须在追加到 pending 前限制未解析数据大小，
// 否则一个异常大的底层 body chunk 会先占用内存，再等帧解析时报错。
const HTTP_E2EE_MAX_PENDING_BYTES: usize = HTTP_E2EE_FRAME_LEN_BYTES + HTTP_E2EE_MAX_FRAME_BYTES;
const SESSION_ACTIVITY_PUSH_MIN_INTERVAL: Duration = Duration::from_millis(250);
const WEBSOCKET_TRAFFIC_LOG_INTERVAL: Duration = Duration::from_secs(1);
const WEBSOCKET_SEND_SLOW_LOG_THRESHOLD: Duration = Duration::from_millis(50);
const WEBSOCKET_SEND_DEBUG_LOG_THRESHOLD: Duration = Duration::from_millis(10);
const WEBSOCKET_SEND_DEBUG_BATCH_ENVELOPES: usize = 8;
const WEBSOCKET_SEND_DEBUG_BATCH_BYTES: usize = 512 * 1024;
const WEBSOCKET_SEND_INFO_BATCH_ENVELOPES: usize = 64;
const WEBSOCKET_SEND_INFO_BATCH_BYTES: usize = 8 * 1024 * 1024;
const WEBSOCKET_WIRE_QUEUE_CAPACITY: usize = 256;
const WEBSOCKET_PUSH_EVENT_QUEUE_CAPACITY: usize = 1024;
const WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK: usize = 64;
const WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK: usize = 16 * 1024 * 1024;
const WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED_PER_TICK: Duration = Duration::from_millis(4);
const TERMINAL_OUTPUT_PUSH_COALESCE_DELAY: Duration = Duration::from_millis(10);

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

    fn coalesce_delay(self) -> Option<Duration> {
        match self {
            // 中文注释：终端输出是高频数据面。等待一个很短窗口可以把“一行一个 signal”
            // 合并为一次 drain，仍保持交互延迟在用户无感范围内。
            SessionPushEvent::Output(_) => Some(TERMINAL_OUTPUT_PUSH_COALESCE_DELAY),
            SessionPushEvent::Activity(_)
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
    #[error("daemon state persistence failed: {0}")]
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
    let state = StateStore::load(&config.state_path)?;
    cleanup_persisted_session_file_http_uploads(&config.state_path)?;
    let supervisor_backend = SupervisorPtyBackend::for_state_path(&config.state_path);
    // 中文注释：生产路径现在只接受 supervisor Unix socket restore_info；旧 tmux 时代
    // 遗留的 live supervisor 仍只做孤儿告警，不能再被默认启动路径自动接回运行态。
    let valid_supervisor_session_ids = state
        .sessions
        .iter()
        .filter(|session| {
            session.state == SessionState::Running
                && matches!(
                    session.restore_info,
                    Some(PtyRestoreInfo::UnixSocket { .. })
                )
        })
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
        .filter(|session| {
            session.state == SessionState::Running
                && matches!(
                    session.restore_info,
                    Some(PtyRestoreInfo::UnixSocket { .. })
                )
        })
        .map(|session| session.session_id.0.to_string())
        .collect::<Vec<_>>();
    warn_about_orphaned_supervisors(
        &SupervisorPtyBackend::for_state_path(&config.state_path),
        restored_supervisor_session_ids,
    );
    // 首次启动时立即写入 daemon identity，避免已展示的 server id 只停留在内存里。
    let mut protocol = protocol;
    protocol.persist_state()?;
    let protected_session_ids = HashSet::new();
    if let Err(error) = protocol.prune_closed_sessions_except(&protected_session_ids) {
        warn!(%error, "failed to prune closed session records during startup");
    }
    Ok(Arc::new(Mutex::new(protocol)))
}

/// 测试与旧调用点使用的便捷构造器；生产启动路径使用 `try_default_protocol` 返回结构化错误。
pub fn default_protocol(config: DaemonConfig) -> SharedDaemonProtocol {
    try_default_protocol(config).expect("default daemon protocol should initialize")
}

pub fn router(protocol: SharedDaemonProtocol, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/local/pairing-token", post(local_pairing_token))
        .merge(http_file_api_router())
        .route("/ws", get(ws_handler))
        .with_state(protocol);

    if web_enabled {
        router.fallback(termweb::embedded_web_handler)
    } else {
        router
    }
}

fn http_file_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route("/api/files/upload/init", post(http_file_upload_init))
        .route("/api/files/upload", post(http_file_upload_stream))
        .route("/api/files/upload/abort", post(http_file_upload_abort))
        .route("/api/files/download", post(http_file_download))
        // 中文注释：CORS 只允许挂在文件 HTTP 通道上，不能把本地配对等管理端点一起暴露出去。
        .route_layer(http_file_api_cors_layer())
}

fn http_file_api_cors_layer() -> CorsLayer {
    // 中文注释：文件上传/下载的 HTTP E2EE 通道会带自定义签名头，浏览器在跨源预览或
    // 分离部署时一定会先发 OPTIONS 预检。这里仅放开文件 API 所需的 method/header；
    // 真正的访问控制仍由设备签名、nonce 和 session attach 校验负责。
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            HeaderName::from_static("x-termd-server-id"),
            HeaderName::from_static("x-termd-device-id"),
            HeaderName::from_static("x-termd-e2ee-public-key"),
            HeaderName::from_static("x-termd-e2ee-nonce"),
            HeaderName::from_static("x-termd-e2ee-timestamp-ms"),
            HeaderName::from_static("x-termd-e2ee-signature"),
        ])
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

pub(crate) async fn handle_http_file_tunnel_stream_request(
    protocol: SharedDaemonProtocol,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Body,
) -> Response {
    if !is_http_file_tunnel_allowed(&method, &path) {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorPayload {
                code: "invalid_http_tunnel".to_owned(),
                message: "invalid HTTP tunnel request".to_owned(),
            }),
        )
            .into_response();
    }
    let mut builder = Request::builder()
        .method(method.as_str())
        .uri(path.as_str());
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = match builder.body(body) {
        Ok(request) => request,
        Err(_) => {
            let error = ErrorPayload {
                code: "invalid_http_tunnel".to_owned(),
                message: "invalid HTTP tunnel request".to_owned(),
            };
            return (StatusCode::BAD_REQUEST, Json(error)).into_response();
        }
    };
    match router(protocol, false).oneshot(request).await {
        Ok(response) => response,
        Err(_) => {
            let error = ErrorPayload {
                code: "http_tunnel_failed".to_owned(),
                message: "HTTP tunnel request failed".to_owned(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
    }
}

fn is_http_file_tunnel_allowed(method: &str, path: &str) -> bool {
    // 中文注释：relay 是不可信 dumb pipe；daemon 不能依赖 relay 的 HTTP route 限制。
    // tunnel 入口只允许文件传输 API，避免把 `/healthz`、本地配对等非文件路由暴露出去。
    method.eq_ignore_ascii_case("POST")
        && matches!(
            path,
            "/api/files/upload/init"
                | "/api/files/upload"
                | "/api/files/upload/abort"
                | "/api/files/download"
        )
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

async fn http_file_upload_init(
    State(protocol): State<SharedDaemonProtocol>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let auth = match http_e2ee_auth_from_headers(&headers, &method, uri.path()) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let body = match read_http_e2ee_short_body(body).await {
        Ok(body) => body,
        Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
    };
    let mut protocol = protocol.lock().await;
    let (device_id, mut e2ee) = match protocol.open_http_e2ee_session(auth) {
        Ok(result) => result,
        Err(error) => return http_e2ee_error(StatusCode::UNAUTHORIZED, error),
    };
    let payload: SessionFileUploadPayload = match decrypt_single_http_e2ee_json(&mut e2ee, &body) {
        Ok(payload) => payload,
        Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
    };
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Err(error) = protocol.attach_session(
        &mut connection,
        SessionAttachPayload {
            session_id: payload.session_id,
            watch_updates: false,
            last_terminal_seq: None,
        },
    ) {
        return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
    }
    let response = match protocol.prepare_session_file_http_upload(&connection, payload, device_id)
    {
        Ok(ready) => match encrypt_single_http_e2ee_json(&mut e2ee, &ready) {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(error) => http_e2ee_error(StatusCode::INTERNAL_SERVER_ERROR, error),
        },
        Err(error) => http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error),
    };
    connection.close(&mut protocol);
    response
}

async fn http_file_upload_stream(
    State(protocol): State<SharedDaemonProtocol>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let auth = match http_e2ee_auth_from_headers(&headers, &method, uri.path()) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let mut protocol_guard = protocol.lock().await;
    let (device_id, mut e2ee) = match protocol_guard.open_http_e2ee_session(auth) {
        Ok(result) => result,
        Err(error) => return http_e2ee_error(StatusCode::UNAUTHORIZED, error),
    };
    drop(protocol_guard);
    let mut stream = HttpE2eeBodyFrameStream::new(body);
    let meta_frame = match read_http_e2ee_metadata_frame(&mut stream, &mut e2ee).await {
        Ok(frame) => frame,
        Err(error) => {
            warn!(
                code = error.code(),
                "HTTP upload stream rejected metadata frame"
            );
            return http_e2ee_error(StatusCode::BAD_REQUEST, error);
        }
    };
    let mut meta: SessionFileHttpUploadStreamPayload = match serde_json::from_slice(&meta_frame) {
        Ok(meta) => meta,
        Err(_) => {
            return http_e2ee_encrypted_error(
                &mut e2ee,
                StatusCode::BAD_REQUEST,
                ProtocolError::InvalidEnvelope,
            );
        }
    };
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    {
        let mut protocol_guard = protocol.lock().await;
        if let Err(error) = protocol_guard.attach_session(
            &mut connection,
            SessionAttachPayload {
                session_id: meta.session_id,
                watch_updates: false,
                last_terminal_seq: None,
            },
        ) {
            return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
        }
    }
    let mut connection_guard = HttpConnectionCloseGuard::new(protocol.clone(), connection);
    let mut progress = None;
    let mut saw_chunk = false;
    loop {
        let chunk = match stream.next_plaintext(&mut e2ee).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                debug!(
                    session_id = %meta.session_id.0,
                    path = %meta.path,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    code = error.code(),
                    "HTTP upload stream rejected data frame detail"
                );
                warn!(
                    session_id = %meta.session_id.0,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    code = error.code(),
                    "HTTP upload stream rejected data frame"
                );
                connection_guard.close_now().await;
                return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
            }
        };
        saw_chunk = true;
        let chunk_len = chunk.len() as u64;
        match write_http_file_upload_chunks_without_protocol_io_lock(
            protocol.clone(),
            connection_guard.connection(),
            meta.clone(),
            device_id,
            vec![chunk],
        )
        .await
        {
            Ok(update) => {
                // 中文注释：update.offset_bytes 表示整个 upload_id 当前已收到多少字节；
                // 2 并发乱序上传时它不是本 POST 的下一个 offset。当前请求内多个
                // E2EE frame 必须只按本请求已消费的明文长度递增。
                meta.offset_bytes = meta
                    .offset_bytes
                    .checked_add(chunk_len)
                    .unwrap_or(meta.size_bytes);
                progress = Some(update);
            }
            Err(error) => {
                debug!(
                    session_id = %meta.session_id.0,
                    path = %meta.path,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    chunk_len,
                    code = error.code(),
                    "HTTP upload stream write failed detail"
                );
                warn!(
                    session_id = %meta.session_id.0,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    chunk_len,
                    code = error.code(),
                    "HTTP upload stream write failed"
                );
                connection_guard.close_now().await;
                return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
            }
        }
    }
    if !saw_chunk && meta.offset_bytes == meta.size_bytes {
        match write_http_file_upload_chunks_without_protocol_io_lock(
            protocol.clone(),
            connection_guard.connection(),
            meta.clone(),
            device_id,
            Vec::new(),
        )
        .await
        {
            Ok(update) => progress = Some(update),
            Err(error) => {
                debug!(
                    session_id = %meta.session_id.0,
                    path = %meta.path,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    code = error.code(),
                    "HTTP empty upload stream write failed detail"
                );
                warn!(
                    session_id = %meta.session_id.0,
                    upload_id = %meta.upload_id,
                    offset_bytes = meta.offset_bytes,
                    code = error.code(),
                    "HTTP empty upload stream write failed"
                );
                connection_guard.close_now().await;
                return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
            }
        }
    }
    let Some(progress) = progress else {
        debug!(
            session_id = %meta.session_id.0,
            path = %meta.path,
            upload_id = %meta.upload_id,
            offset_bytes = meta.offset_bytes,
            "HTTP upload stream ended without payload progress detail"
        );
        warn!(
            session_id = %meta.session_id.0,
            upload_id = %meta.upload_id,
            offset_bytes = meta.offset_bytes,
            "HTTP upload stream ended without payload progress"
        );
        connection_guard.close_now().await;
        return http_e2ee_encrypted_error(
            &mut e2ee,
            StatusCode::BAD_REQUEST,
            ProtocolError::InvalidEnvelope,
        );
    };
    // 中文注释：HTTP 上传现在允许一个 upload_id 被多个分片 POST 顺序或并发提交；
    // 非 eof 进度表示本次分片已落盘，不能再按旧单请求模型 abort。
    connection_guard.close_now().await;
    match encrypt_single_http_e2ee_json(&mut e2ee, &progress) {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(error) => http_e2ee_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn http_file_upload_abort(
    State(protocol): State<SharedDaemonProtocol>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let auth = match http_e2ee_auth_from_headers(&headers, &method, uri.path()) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let body = match read_http_e2ee_short_body(body).await {
        Ok(body) => body,
        Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
    };
    let mut protocol_guard = protocol.lock().await;
    let (device_id, mut e2ee) = match protocol_guard.open_http_e2ee_session(auth) {
        Ok(result) => result,
        Err(error) => return http_e2ee_error(StatusCode::UNAUTHORIZED, error),
    };
    let payload: SessionFileHttpUploadStreamPayload =
        match decrypt_single_http_e2ee_json(&mut e2ee, &body) {
            Ok(payload) => payload,
            Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
        };
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Err(error) = protocol_guard.attach_session(
        &mut connection,
        SessionAttachPayload {
            session_id: payload.session_id,
            watch_updates: false,
            last_terminal_seq: None,
        },
    ) {
        return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
    }
    let response = match protocol_guard.abort_session_file_http_upload(&connection, &payload) {
        Ok(()) => match encrypt_single_http_e2ee_json(&mut e2ee, &payload) {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(error) => http_e2ee_error(StatusCode::INTERNAL_SERVER_ERROR, error),
        },
        Err(error) => http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error),
    };
    connection.close(&mut protocol_guard);
    response
}

struct HttpConnectionCloseGuard {
    protocol: SharedDaemonProtocol,
    connection: Option<ProtocolConnection>,
}

impl HttpConnectionCloseGuard {
    fn new(protocol: SharedDaemonProtocol, connection: ProtocolConnection) -> Self {
        Self {
            protocol,
            connection: Some(connection),
        }
    }

    fn connection(&self) -> &ProtocolConnection {
        self.connection
            .as_ref()
            .expect("HTTP connection guard should still own connection")
    }

    async fn close_now(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            let mut protocol = self.protocol.lock().await;
            connection.close(&mut protocol);
        }
        self.connection = None;
    }
}

impl Drop for HttpConnectionCloseGuard {
    fn drop(&mut self) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        let protocol = self.protocol.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // 中文注释：HTTP upload stream 会临时 attach 到 session；handler 被浏览器取消时
        // 正常 close 路径不会执行，Drop 必须补一次 detach，避免 operator/runtime 计数泄漏。
        handle.spawn(async move {
            let mut protocol = protocol.lock().await;
            connection.close(&mut protocol);
        });
    }
}

struct HttpUploadInflightGuard {
    protocol: SharedDaemonProtocol,
    meta: SessionFileHttpUploadStreamPayload,
    reserved_range: Option<(u64, u64)>,
    file_result: Option<super::protocol::SessionFileHttpUploadFileWriteResult>,
    armed: bool,
}

impl HttpUploadInflightGuard {
    fn new(
        protocol: SharedDaemonProtocol,
        meta: SessionFileHttpUploadStreamPayload,
        reserved_range: Option<(u64, u64)>,
    ) -> Self {
        Self {
            protocol,
            meta,
            reserved_range,
            file_result: None,
            armed: true,
        }
    }

    fn mark_written(&mut self, file_result: super::protocol::SessionFileHttpUploadFileWriteResult) {
        self.file_result = Some(file_result);
    }

    async fn cancel_now(&mut self) {
        if !self.armed {
            return;
        }
        let mut protocol = self.protocol.lock().await;
        protocol.cancel_session_file_http_upload_write(&self.meta, self.reserved_range);
        drop(protocol);
        self.disarm();
    }

    async fn commit_now(&mut self) -> Result<SessionFileHttpUploadCommit, ProtocolError> {
        let file_result = self
            .file_result
            .as_ref()
            .ok_or(ProtocolError::InvalidState)?;
        let mut protocol = self.protocol.lock().await;
        let commit = protocol.commit_session_file_http_upload_write(&self.meta, file_result);
        drop(protocol);
        if commit.is_ok() {
            self.disarm();
        }
        commit
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.file_result = None;
    }
}

impl Drop for HttpUploadInflightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let protocol = self.protocol.clone();
        let meta = self.meta.clone();
        let reserved_range = self.reserved_range;
        let file_result = self.file_result.take();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // 中文注释：reserve 后文件写入过程本身不再 await；因此 Drop 只会发生在写入前后。
        // 写入前取消就释放 in-flight；写入后取消必须提交结果，不能让 retry 覆盖已落盘数据。
        handle.spawn(async move {
            let mut protocol = protocol.lock().await;
            if let Some(file_result) = file_result {
                if let Err(error) =
                    protocol.commit_session_file_http_upload_write(&meta, &file_result)
                {
                    warn!(%error, "failed to commit HTTP upload write after handler drop");
                }
            } else {
                protocol.cancel_session_file_http_upload_write(&meta, reserved_range);
            }
        });
    }
}

async fn write_http_file_upload_chunks_without_protocol_io_lock(
    protocol: SharedDaemonProtocol,
    connection: &ProtocolConnection,
    meta: SessionFileHttpUploadStreamPayload,
    device_id: termd_proto::DeviceId,
    chunks: Vec<Vec<u8>>,
) -> Result<termd_proto::SessionFileUploadProgressPayload, ProtocolError> {
    let write_len = session_file_http_upload_chunks_len(&chunks)?;
    let begin = {
        let mut protocol = protocol.lock().await;
        protocol.begin_session_file_http_upload_write(
            connection,
            meta.clone(),
            device_id,
            write_len,
        )
    };
    let begin = match begin {
        Ok(begin) => begin,
        Err(error) => {
            debug!(
                session_id = %meta.session_id.0,
                path = %meta.path,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload begin write failed detail"
            );
            warn!(
                session_id = %meta.session_id.0,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload begin write failed"
            );
            return Err(error);
        }
    };
    let plan = match begin {
        SessionFileHttpUploadBegin::Write(plan) => plan,
        SessionFileHttpUploadBegin::Complete(progress) => return Ok(progress),
    };
    let reserved_range = plan.reserved_range;
    let mut inflight_guard =
        HttpUploadInflightGuard::new(protocol.clone(), meta.clone(), reserved_range);
    let file_result = match write_session_file_http_upload_files(plan, chunks) {
        Ok(result) => result,
        Err(error) => {
            debug!(
                session_id = %meta.session_id.0,
                path = %meta.path,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload positional file write failed detail"
            );
            warn!(
                session_id = %meta.session_id.0,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload positional file write failed"
            );
            inflight_guard.cancel_now().await;
            return Err(error);
        }
    };
    inflight_guard.mark_written(file_result);
    let commit = match inflight_guard.commit_now().await {
        Ok(commit) => commit,
        Err(error) => {
            debug!(
                session_id = %meta.session_id.0,
                path = %meta.path,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload commit failed detail"
            );
            warn!(
                session_id = %meta.session_id.0,
                upload_id = %meta.upload_id,
                offset_bytes = meta.offset_bytes,
                write_len,
                code = error.code(),
                "HTTP upload commit failed"
            );
            return Err(error);
        }
    };
    match commit {
        SessionFileHttpUploadCommit::Progress(progress) => Ok(progress),
        SessionFileHttpUploadCommit::Complete(progress) => Ok(progress),
    }
}

async fn http_file_download(
    State(protocol): State<SharedDaemonProtocol>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let auth = match http_e2ee_auth_from_headers(&headers, &method, uri.path()) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let body = match read_http_e2ee_short_body(body).await {
        Ok(body) => body,
        Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
    };
    let mut protocol = protocol.lock().await;
    let (device_id, mut e2ee) = match protocol.open_http_e2ee_session(auth) {
        Ok(result) => result,
        Err(error) => return http_e2ee_error(StatusCode::UNAUTHORIZED, error),
    };
    let payload: SessionFileHttpDownloadPayload =
        match decrypt_single_http_e2ee_json(&mut e2ee, &body) {
            Ok(payload) => payload,
            Err(error) => return http_e2ee_error(StatusCode::BAD_REQUEST, error),
        };
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Err(error) = protocol.attach_session(
        &mut connection,
        SessionAttachPayload {
            session_id: payload.session_id,
            watch_updates: false,
            last_terminal_seq: None,
        },
    ) {
        return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
    }
    let (ready, mut file, offset) =
        match protocol.prepare_session_file_http_download(&connection, payload) {
            Ok(result) => result,
            Err(error) => {
                connection.close(&mut protocol);
                return http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error);
            }
        };
    connection.close(&mut protocol);
    drop(protocol);

    if file.seek(SeekFrom::Start(offset)).is_err() {
        return http_e2ee_encrypted_error(
            &mut e2ee,
            StatusCode::BAD_REQUEST,
            ProtocolError::InvalidEnvelope,
        );
    }
    let remaining = ready
        .size_bytes
        .checked_sub(offset)
        .ok_or(ProtocolError::InvalidEnvelope)
        .map_err(|error| http_e2ee_encrypted_error(&mut e2ee, StatusCode::BAD_REQUEST, error));
    let remaining = match remaining {
        Ok(remaining) => remaining,
        Err(response) => return response,
    };
    // 中文注释：只有文件已经可读并完成 seek 后才加密 ready 帧。否则错误响应会
    // 使用“跳过 ready 帧后”的 E2EE 序号，客户端收到第一帧错误时无法解密。
    let mut ready_body = Vec::new();
    if let Err(error) = append_http_e2ee_json_frame(&mut e2ee, &mut ready_body, &ready) {
        return http_e2ee_error(StatusCode::INTERNAL_SERVER_ERROR, error);
    }
    let stream = futures_util::stream::unfold(
        (
            Some(ready_body),
            file,
            e2ee,
            vec![0_u8; 256 * 1024],
            remaining,
        ),
        |(mut ready_body, mut file, mut e2ee, mut chunk, mut remaining)| async move {
            if let Some(body) = ready_body.take() {
                return Some((
                    Ok::<Bytes, io::Error>(Bytes::from(body)),
                    (ready_body, file, e2ee, chunk, remaining),
                ));
            }
            if remaining == 0 {
                return None;
            }
            let max_read = chunk.len().min(remaining as usize);
            let read = match file.read(&mut chunk[..max_read]) {
                Ok(read) => read,
                Err(error) => {
                    return Some((Err(error), (ready_body, file, e2ee, chunk, remaining)));
                }
            };
            if read == 0 {
                return Some((
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file ended before advertised download size",
                    )),
                    (ready_body, file, e2ee, chunk, remaining),
                ));
            }
            let mut body = Vec::new();
            if append_http_e2ee_binary_frame(&mut e2ee, &mut body, &chunk[..read]).is_err() {
                return Some((
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "failed to encrypt HTTP E2EE download frame",
                    )),
                    (ready_body, file, e2ee, chunk, remaining),
                ));
            }
            remaining = remaining.saturating_sub(read as u64);
            Some((
                Ok::<Bytes, io::Error>(Bytes::from(body)),
                (ready_body, file, e2ee, chunk, remaining),
            ))
        },
    );
    (StatusCode::OK, Body::from_stream(stream)).into_response()
}

fn http_e2ee_auth_from_headers(
    headers: &HeaderMap,
    method: &Method,
    path: &str,
) -> Result<HttpE2eeAuthPayload, Response> {
    let device_id = http_header(headers, "x-termd-device-id")?
        .parse()
        .map(termd_proto::DeviceId)
        .map_err(|_| http_e2ee_error(StatusCode::UNAUTHORIZED, ProtocolError::AuthFailed))?;
    let e2ee_public_key = PublicKey(http_header(headers, "x-termd-e2ee-public-key")?.to_owned());
    let nonce = termd_proto::Nonce(http_header(headers, "x-termd-e2ee-nonce")?.to_owned());
    let timestamp_ms = http_header(headers, "x-termd-e2ee-timestamp-ms")?
        .parse()
        .map(UnixTimestampMillis)
        .map_err(|_| http_e2ee_error(StatusCode::UNAUTHORIZED, ProtocolError::AuthFailed))?;
    let signature = Signature(http_header(headers, "x-termd-e2ee-signature")?.to_owned());

    Ok(HttpE2eeAuthPayload {
        device_id,
        e2ee_public_key,
        nonce,
        timestamp_ms,
        method: method.as_str().to_owned(),
        path: path.to_owned(),
        signature,
    })
}

fn http_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, Response> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorPayload {
                    code: "http_e2ee_required".to_owned(),
                    message: "HTTP E2EE headers are required".to_owned(),
                }),
            )
                .into_response()
        })
}

async fn read_http_e2ee_short_body(body: Body) -> Result<Bytes, ProtocolError> {
    // 中文注释：upload init 和 download metadata 都是短请求；不能让慢客户端无限占住
    // daemon handler。真正的大文件 upload body 不走这里，仍由连接背压自然推进。
    match timeout(
        HTTP_E2EE_SHORT_BODY_TIMEOUT,
        to_bytes(body, HTTP_E2EE_INIT_MAX_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(_)) | Err(_) => Err(ProtocolError::InvalidEnvelope),
    }
}

async fn read_http_e2ee_metadata_frame(
    stream: &mut HttpE2eeBodyFrameStream,
    e2ee: &mut super::E2eeSession,
) -> Result<Vec<u8>, ProtocolError> {
    // 中文注释：upload 长流只有首个 metadata frame 是短控制信息；它必须快速到达。
    // 后续文件内容不设置整体耗时上限，避免弱网大文件上传被误杀。
    match timeout(HTTP_E2EE_SHORT_BODY_TIMEOUT, stream.next_plaintext(e2ee)).await {
        Ok(Ok(Some(frame))) => Ok(frame),
        Ok(Ok(None)) => Err(ProtocolError::InvalidEnvelope),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ProtocolError::InvalidEnvelope),
    }
}

fn decrypt_single_http_e2ee_json<T>(
    e2ee: &mut super::E2eeSession,
    body: &Bytes,
) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    let mut frames = decrypt_http_e2ee_frames(e2ee, body)?;
    if frames.len() != 1 {
        return Err(ProtocolError::InvalidEnvelope);
    }
    serde_json::from_slice(&frames.remove(0)).map_err(|_| ProtocolError::InvalidEnvelope)
}

fn decrypt_http_e2ee_frames(
    e2ee: &mut super::E2eeSession,
    body: &Bytes,
) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let mut offset = 0;
    let mut frames = Vec::new();
    while offset < body.len() {
        let frame = read_http_e2ee_frame(body, &mut offset)?;
        let binary =
            decode_binary_encrypted_frame(frame).map_err(|_| ProtocolError::InvalidEnvelope)?;
        let plaintext = e2ee
            .decrypt_binary_payload(&binary)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;
        frames.push(plaintext);
    }
    Ok(frames)
}

struct HttpE2eeBodyFrameStream {
    stream: axum::body::BodyDataStream,
    pending: Vec<u8>,
    buffered: Option<Bytes>,
    buffered_offset: usize,
}

impl HttpE2eeBodyFrameStream {
    fn new(body: Body) -> Self {
        Self {
            stream: body.into_data_stream(),
            pending: Vec::new(),
            buffered: None,
            buffered_offset: 0,
        }
    }

    async fn next_plaintext(
        &mut self,
        e2ee: &mut super::E2eeSession,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        loop {
            if let Some(frame) = self.try_pop_frame()? {
                let binary = decode_binary_encrypted_frame(&frame)
                    .map_err(|_| ProtocolError::InvalidEnvelope)?;
                return e2ee
                    .decrypt_binary_payload(&binary)
                    .map(Some)
                    .map_err(|_| ProtocolError::InvalidEnvelope);
            }
            if self.drain_buffered_body_bytes()? {
                continue;
            }
            match self.stream.next().await {
                Some(Ok(bytes)) if bytes.is_empty() => {}
                Some(Ok(bytes)) => {
                    self.buffered = Some(bytes);
                    self.buffered_offset = 0;
                }
                Some(Err(_)) => return Err(ProtocolError::InvalidEnvelope),
                None if self.pending.is_empty() => return Ok(None),
                None => return Err(ProtocolError::InvalidEnvelope),
            }
        }
    }

    fn drain_buffered_body_bytes(&mut self) -> Result<bool, ProtocolError> {
        let Some(bytes) = self.buffered.as_ref() else {
            return Ok(false);
        };
        if self.buffered_offset >= bytes.len() {
            self.buffered = None;
            self.buffered_offset = 0;
            return Ok(false);
        }
        let capacity = HTTP_E2EE_MAX_PENDING_BYTES.saturating_sub(self.pending.len());
        if capacity == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        // 中文注释：底层 HTTP body chunk 可能合并多个合法 E2EE frame；分段搬入
        // pending，既保留 append 前内存上限，也允许大 coalesced chunk 被逐帧消费。
        let remaining = bytes.len().saturating_sub(self.buffered_offset);
        let take = capacity.min(remaining);
        let end = self.buffered_offset.saturating_add(take);
        self.pending
            .extend_from_slice(&bytes[self.buffered_offset..end]);
        self.buffered_offset = end;
        if self.buffered_offset >= bytes.len() {
            self.buffered = None;
            self.buffered_offset = 0;
        }
        Ok(true)
    }

    fn try_pop_frame(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.pending.len() < HTTP_E2EE_FRAME_LEN_BYTES {
            return Ok(None);
        }
        let len = u32::from_be_bytes(
            self.pending[0..HTTP_E2EE_FRAME_LEN_BYTES]
                .try_into()
                .map_err(|_| ProtocolError::InvalidEnvelope)?,
        ) as usize;
        if len == 0 || len > HTTP_E2EE_MAX_FRAME_BYTES {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let frame_end = HTTP_E2EE_FRAME_LEN_BYTES.saturating_add(len);
        if self.pending.len() < frame_end {
            return Ok(None);
        }
        let frame = self.pending[HTTP_E2EE_FRAME_LEN_BYTES..frame_end].to_vec();
        self.pending.drain(0..frame_end);
        Ok(Some(frame))
    }
}

fn encrypt_single_http_e2ee_json<T>(
    e2ee: &mut super::E2eeSession,
    payload: &T,
) -> Result<Body, ProtocolError>
where
    T: serde::Serialize,
{
    let mut body = Vec::new();
    append_http_e2ee_json_frame(e2ee, &mut body, payload)?;
    Ok(Body::from(body))
}

fn append_http_e2ee_json_frame<T>(
    e2ee: &mut super::E2eeSession,
    body: &mut Vec<u8>,
    payload: &T,
) -> Result<(), ProtocolError>
where
    T: serde::Serialize,
{
    let plaintext = serde_json::to_vec(payload).map_err(|_| ProtocolError::InvalidEnvelope)?;
    append_http_e2ee_binary_frame(e2ee, body, &plaintext)
}

fn append_http_e2ee_binary_frame(
    e2ee: &mut super::E2eeSession,
    body: &mut Vec<u8>,
    plaintext: &[u8],
) -> Result<(), ProtocolError> {
    let encrypted = e2ee
        .encrypt_binary_payload(plaintext)
        .map_err(|_| ProtocolError::InvalidEnvelope)?;
    body.extend_from_slice(&write_http_e2ee_frame(&encode_binary_encrypted_frame(
        &encrypted,
    )));
    Ok(())
}

fn read_http_e2ee_frame<'a>(body: &'a [u8], offset: &mut usize) -> Result<&'a [u8], ProtocolError> {
    if body.len().saturating_sub(*offset) < HTTP_E2EE_FRAME_LEN_BYTES {
        return Err(ProtocolError::InvalidEnvelope);
    }
    let len = u32::from_be_bytes(
        body[*offset..*offset + HTTP_E2EE_FRAME_LEN_BYTES]
            .try_into()
            .map_err(|_| ProtocolError::InvalidEnvelope)?,
    ) as usize;
    *offset += HTTP_E2EE_FRAME_LEN_BYTES;
    if len == 0 || len > HTTP_E2EE_MAX_FRAME_BYTES || body.len().saturating_sub(*offset) < len {
        return Err(ProtocolError::InvalidEnvelope);
    }
    let frame = &body[*offset..*offset + len];
    *offset += len;
    Ok(frame)
}

fn write_http_e2ee_frame(frame: &[u8]) -> Vec<u8> {
    let len = u32::try_from(frame.len()).expect("HTTP E2EE frame length should fit u32");
    let mut out = Vec::with_capacity(HTTP_E2EE_FRAME_LEN_BYTES + frame.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(frame);
    out
}

fn http_e2ee_error(status: StatusCode, error: ProtocolError) -> Response {
    (
        status,
        Json(ErrorPayload {
            code: error.code().to_owned(),
            message: error.safe_message().to_owned(),
        }),
    )
        .into_response()
}

fn http_e2ee_encrypted_error(
    e2ee: &mut super::E2eeSession,
    status: StatusCode,
    error: ProtocolError,
) -> Response {
    let payload = ErrorPayload {
        code: error.code().to_owned(),
        message: error.safe_message().to_owned(),
    };
    match encrypt_single_http_e2ee_json(e2ee, &payload) {
        Ok(body) => (status, body).into_response(),
        Err(error) => http_e2ee_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
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
    // 中文注释：进入业务态后，WebSocket 断开就是当前 client context 的生命周期结束。
    // 此时不能继续 drain 已入队的 terminal/file push，否则旧连接关闭后还会尝试写 socket，
    // 既浪费带宽，也会刷出 misleading 的 "Sending after closing" 日志。
    drop(write_wire_tx);
    writer_task.abort();
    let _ = writer_task.await;
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
    let elapsed_budget_exhausted =
        drained_events > 0 && started_at.elapsed() >= WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED_PER_TICK;
    drained_events >= WEBSOCKET_PUSH_DRAIN_MAX_EVENTS_PER_TICK
        || enqueued_bytes >= WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK
        || elapsed_budget_exhausted
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
                let messages = connection.drain_session_output_messages_for_push(
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
    let coalesce_delay = event.coalesce_delay();
    watcher_tasks.push(tokio::spawn(async move {
        loop {
            if signal.changed().await.is_err() {
                break;
            }
            if let Some(delay) = coalesce_delay {
                tokio::time::sleep(delay).await;
                // 中文注释：coalesce 窗口内发生的多次 watch 更新只需要一个 push 事件；
                // 真正读取时会从 daemon terminal log 一次 drain 已累计的 frames。
                signal.borrow_and_update();
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
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    use serde::Deserialize;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use termd_proto::{
        DeviceId, E2eeKeyExchangePayload, Envelope, HttpE2eeAuthPayload, PairAcceptPayload,
        PairRequestPayload, PublicKey, SessionCreatePayload, SessionCreatedPayload,
        SessionDataPayload, SessionFileDownloadStreamReadyPayload, SessionFileHttpDownloadPayload,
        SessionFileHttpUploadReadyPayload, SessionFileHttpUploadStreamPayload,
        SessionFileUploadPayload, Signature, TerminalSize, UnixTimestampMillis,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::tungstenite::Message as ClientWsMessage;

    use crate::auth::{HttpE2eeSigningInput, current_unix_timestamp_millis};
    use crate::net::protocol::{
        ProtocolConnection, decode_payload, encrypted_frame_from_envelope, envelope_value,
    };
    use crate::net::{
        E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
    };
    use crate::state::{
        DaemonState, SessionStateRecord, StateStore, client_history::ClientHistoryStore,
    };
    use axum::body::Body;
    use axum::http::Request;

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
    async fn websocket_output_watcher_coalesces_bursty_terminal_signals() {
        let session_id = SessionId::new();
        let (signal_tx, signal_rx) = watch::channel(0_u64);
        let (push_event_tx, mut push_event_rx) = mpsc::channel(8);
        let mut watcher_tasks = Vec::new();

        spawn_session_push_watcher(
            session_id,
            signal_rx,
            SessionPushEvent::Output(session_id),
            &push_event_tx,
            &mut watcher_tasks,
        );

        signal_tx.send(1).unwrap();
        signal_tx.send(2).unwrap();
        signal_tx.send(3).unwrap();

        // 中文注释：高频 PTY 输出不能一变更就立刻形成一个 WebSocket 小包；
        // watcher 应该等待一个很短 coalesce 窗口，让 daemon 一次 drain 已累计的 frames。
        assert!(
            tokio::time::timeout(Duration::from_millis(5), push_event_rx.recv())
                .await
                .is_err(),
            "output watcher should not push immediately for bursty terminal output"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), push_event_rx.recv())
                .await
                .unwrap(),
            Some(SessionPushEvent::Output(session_id))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), push_event_rx.recv())
                .await
                .is_err(),
            "coalesced watch changes should produce one output event"
        );

        for task in watcher_tasks {
            task.abort();
        }
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
        // 中文注释：direct WebSocket 和 relay 一样走高速 terminal 字节流；
        // 小 batch 只适合低频交互输出，大量输出必须保留 MB 级 in-flight 空间。
        assert!(OUTPUT_FLUSH_MAX_BYTES_PER_SESSION >= 512 * 1024);
        assert!(WEBSOCKET_PUSH_DRAIN_MAX_ENQUEUED_BYTES_PER_TICK >= 8 * 1024 * 1024);
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
            Instant::now() - Duration::from_secs(60)
        ));
    }

    #[test]
    fn websocket_push_drain_budget_stops_after_elapsed_window() {
        let started_at = Instant::now() - WEBSOCKET_PUSH_DRAIN_MAX_ELAPSED_PER_TICK;

        // 中文注释：时间预算只在至少 drain 过一个事件后生效，避免空 tick 自旋。
        assert!(websocket_push_drain_budget_exhausted(1, 1024, started_at));
        assert!(!websocket_push_drain_budget_exhausted(0, 1024, started_at));
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

    static TEST_CONFIG_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_config(name: &str) -> DaemonConfig {
        let unique = TEST_CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let state_dir = std::env::temp_dir().join(format!(
            "termd-server-test-{}-{}-{unique}-{name}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        // 中文注释：tmux backend 会把 socket 放在 state path 边界内。server 单测仍使用
        // 独立目录，避免并发测试或遗留 socket 影响同一组 daemon 状态。
        fs::create_dir_all(&state_dir).unwrap();
        DaemonConfig::default_for_state_path(state_dir.join("daemon-state.json"))
    }

    fn test_protocol(name: &str) -> SharedDaemonProtocol {
        default_protocol(test_config(name))
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

    #[tokio::test]
    async fn http_file_upload_init_requires_e2ee_headers() {
        let protocol = test_protocol("http-file-upload-init-requires-e2ee");
        let response = router(protocol, false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/files/upload/init")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_file_routes_answer_cors_preflight() {
        let protocol = test_protocol("http-file-cors-preflight");
        let response = router(protocol, false)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/files/upload/init")
                    .header("origin", "http://127.0.0.1:4173")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "content-type,x-termd-server-id,x-termd-device-id,x-termd-e2ee-public-key,x-termd-e2ee-nonce,x-termd-e2ee-timestamp-ms,x-termd-e2ee-signature",
                    )
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
    }

    #[tokio::test]
    async fn http_file_tunnel_rejects_non_file_routes_before_router_dispatch() {
        let protocol = test_protocol("http-file-tunnel-allowlist");
        let response = handle_http_file_tunnel_stream_request(
            protocol,
            "GET".to_owned(),
            "/healthz".to_owned(),
            Vec::new(),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_file_upload_init_accepts_signed_e2ee_body() {
        let protocol = test_protocol("http-file-upload-init-e2ee");
        let signing_key = SigningKey::generate(&mut OsRng);
        let device_public_key =
            PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));
        let (device_id, session_id) =
            pair_real_device_and_create_session(protocol.clone(), device_public_key.clone()).await;
        let server_id = protocol.lock().await.server_id();
        let daemon_identity = protocol.lock().await.daemon_public_identity().clone();
        let daemon_e2ee_public = protocol.lock().await.e2ee_public_key();
        let http_keypair = E2eeKeyPair::generate();
        let mut http_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &http_keypair,
            daemon_e2ee_public,
            E2eeSessionContext::new(
                server_id,
                device_id,
                daemon_e2ee_public,
                http_keypair.public_key(),
            ),
        )
        .unwrap();
        let path = "/api/files/upload/init";
        let mut auth = HttpE2eeAuthPayload {
            device_id,
            e2ee_public_key: http_keypair.public_key_wire(),
            nonce: termd_proto::Nonce("http-upload-init-nonce".to_owned()),
            timestamp_ms: current_unix_timestamp_millis(),
            method: "POST".to_owned(),
            path: path.to_owned(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        auth.signature = Signature(test_ed25519_wire(
            &signing_key
                .sign(&HttpE2eeSigningInput::from_payload(&auth, &daemon_identity).to_bytes())
                .to_bytes(),
        ));
        let upload_path = format!("http-upload-{}.bin", session_id.0);
        let encrypted = http_e2ee
            .encrypt_binary_payload(
                &serde_json::to_vec(&SessionFileUploadPayload {
                    session_id,
                    path: upload_path,
                    size_bytes: 3,
                })
                .unwrap(),
            )
            .unwrap();
        let request_body = write_http_e2ee_frame(&encode_binary_encrypted_frame(&encrypted));
        let response = router(protocol, false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("x-termd-device-id", device_id.0.to_string())
                    .header("x-termd-e2ee-public-key", auth.e2ee_public_key.0)
                    .header("x-termd-e2ee-nonce", auth.nonce.0)
                    .header("x-termd-e2ee-timestamp-ms", auth.timestamp_ms.0.to_string())
                    .header("x-termd-e2ee-signature", auth.signature.0)
                    .body(Body::from(request_body))
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        let mut offset = 0;
        let frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let encrypted = decode_binary_encrypted_frame(frame).unwrap();
        let ready: SessionFileHttpUploadReadyPayload =
            serde_json::from_slice(&http_e2ee.decrypt_binary_payload(&encrypted).unwrap()).unwrap();

        assert_eq!(ready.session_id, session_id);
        assert_eq!(ready.size_bytes, 3);
        assert_eq!(ready.offset_bytes, 0);
        assert!(!ready.upload_id.is_empty());
        fs::remove_file(&ready.path).ok();
    }

    #[tokio::test]
    async fn http_file_download_stream_stops_at_advertised_size() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-download-exact-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let file_path = root.join("download.bin");
        fs::write(&file_path, b"abc").unwrap();
        let mut config = test_config("http-file-download-exact-size");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let signing_key = SigningKey::generate(&mut OsRng);
        let device_public_key =
            PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));
        let (device_id, session_id) =
            pair_real_device_and_create_session(protocol.clone(), device_public_key).await;
        let server_id = protocol.lock().await.server_id();
        let daemon_identity = protocol.lock().await.daemon_public_identity().clone();
        let daemon_e2ee_public = protocol.lock().await.e2ee_public_key();
        let http_keypair = E2eeKeyPair::generate();
        let mut http_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &http_keypair,
            daemon_e2ee_public,
            E2eeSessionContext::new(
                server_id,
                device_id,
                daemon_e2ee_public,
                http_keypair.public_key(),
            ),
        )
        .unwrap();
        let path = "/api/files/download";
        let mut auth = HttpE2eeAuthPayload {
            device_id,
            e2ee_public_key: http_keypair.public_key_wire(),
            nonce: termd_proto::Nonce("http-download-exact-nonce".to_owned()),
            timestamp_ms: current_unix_timestamp_millis(),
            method: "POST".to_owned(),
            path: path.to_owned(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        auth.signature = Signature(test_ed25519_wire(
            &signing_key
                .sign(&HttpE2eeSigningInput::from_payload(&auth, &daemon_identity).to_bytes())
                .to_bytes(),
        ));
        let encrypted = http_e2ee
            .encrypt_binary_payload(
                &serde_json::to_vec(&SessionFileHttpDownloadPayload {
                    session_id,
                    path: "download.bin".to_owned(),
                    offset_bytes: 0,
                })
                .unwrap(),
            )
            .unwrap();
        let request_body = write_http_e2ee_frame(&encode_binary_encrypted_frame(&encrypted));
        let response = router(protocol, false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("x-termd-device-id", device_id.0.to_string())
                    .header("x-termd-e2ee-public-key", auth.e2ee_public_key.0)
                    .header("x-termd-e2ee-nonce", auth.nonce.0)
                    .header("x-termd-e2ee-timestamp-ms", auth.timestamp_ms.0.to_string())
                    .header("x-termd-e2ee-signature", auth.signature.0)
                    .body(Body::from(request_body))
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        // 中文注释：handler 签发 ready 后文件继续增长时，本次 HTTP 响应仍只能发送
        // ready 中声明的大小，不能把后续新增字节混入当前下载。
        let mut appended = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .unwrap();
        appended.write_all(b"extra").unwrap();
        drop(appended);

        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        let mut offset = 0;
        let ready_frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let ready_encrypted = decode_binary_encrypted_frame(ready_frame).unwrap();
        let ready: SessionFileDownloadStreamReadyPayload =
            serde_json::from_slice(&http_e2ee.decrypt_binary_payload(&ready_encrypted).unwrap())
                .unwrap();
        let data_frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let data_encrypted = decode_binary_encrypted_frame(data_frame).unwrap();
        let bytes = http_e2ee.decrypt_binary_payload(&data_encrypted).unwrap();

        assert_eq!(ready.size_bytes, 3);
        assert_eq!(bytes, b"abc");
        assert_eq!(offset, response_body.len());
    }

    #[tokio::test]
    async fn http_file_download_business_error_is_e2ee_encrypted() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-download-encrypted-error-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-download-encrypted-error");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let signing_key = SigningKey::generate(&mut OsRng);
        let device_public_key =
            PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));
        let (device_id, session_id) =
            pair_real_device_and_create_session(protocol.clone(), device_public_key).await;
        let (request, mut http_e2ee) = signed_http_e2ee_request(
            &protocol,
            &signing_key,
            device_id,
            "/api/files/download",
            "http-download-encrypted-business-error",
            vec![
                serde_json::to_vec(&SessionFileHttpDownloadPayload {
                    session_id,
                    path: "missing.bin".to_owned(),
                    offset_bytes: 0,
                })
                .unwrap(),
            ],
        )
        .await;

        let response = router(protocol, false)
            .oneshot(request)
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        assert!(
            serde_json::from_slice::<ErrorPayload>(&response_body).is_err(),
            "post-auth HTTP E2EE business errors must not be plaintext JSON"
        );
        let mut offset = 0;
        let frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let encrypted = decode_binary_encrypted_frame(frame).unwrap();
        let error: ErrorPayload =
            serde_json::from_slice(&http_e2ee.decrypt_binary_payload(&encrypted).unwrap()).unwrap();

        assert!(
            !error.code.is_empty(),
            "客户端应能解开业务错误，具体错误码由协议层按失败原因决定"
        );
        assert_eq!(offset, response_body.len());
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_upload_stream_accepts_split_frames_after_out_of_order_chunk() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-upload-split-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-upload-split");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));
        let (device_id, session_id) =
            pair_real_device_and_create_session(protocol.clone(), public_key).await;

        let upload_init_path = "/api/files/upload/init";
        let (request, mut upload_init_e2ee) = signed_http_e2ee_request(
            &protocol,
            &signing_key,
            device_id,
            upload_init_path,
            "http-upload-split-init",
            vec![
                serde_json::to_vec(&SessionFileUploadPayload {
                    session_id,
                    path: "split.bin".to_owned(),
                    size_bytes: 6,
                })
                .unwrap(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        let mut offset = 0;
        let ready_frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let ready_encrypted = decode_binary_encrypted_frame(ready_frame).unwrap();
        let ready: SessionFileHttpUploadReadyPayload = serde_json::from_slice(
            &upload_init_e2ee
                .decrypt_binary_payload(&ready_encrypted)
                .unwrap(),
        )
        .unwrap();

        for (nonce, offset_bytes, frames) in [
            (
                "http-upload-split-tail",
                3_u64,
                vec![b"d".to_vec(), b"ef".to_vec()],
            ),
            (
                "http-upload-split-head",
                0_u64,
                vec![b"a".to_vec(), b"bc".to_vec()],
            ),
        ] {
            let mut plaintext_frames = vec![
                serde_json::to_vec(&SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: "split.bin".to_owned(),
                    upload_id: ready.upload_id.clone(),
                    size_bytes: 6,
                    offset_bytes,
                })
                .unwrap(),
            ];
            plaintext_frames.extend(frames);
            let (request, _upload_e2ee) = signed_http_e2ee_request(
                &protocol,
                &signing_key,
                device_id,
                "/api/files/upload",
                nonce,
                plaintext_frames,
            )
            .await;
            let response = router(protocol.clone(), false)
                .oneshot(request)
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::OK);
            let _ = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
                .await
                .unwrap();
        }

        assert_eq!(fs::read(root.join("split.bin")).unwrap(), b"abcdef");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_upload_stream_error_does_not_abort_active_upload() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-upload-stale-error-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-upload-stale-error");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));
        let (device_id, session_id) =
            pair_real_device_and_create_session(protocol.clone(), public_key).await;

        let (request, mut upload_init_e2ee) = signed_http_e2ee_request(
            &protocol,
            &signing_key,
            device_id,
            "/api/files/upload/init",
            "http-upload-stale-error-init",
            vec![
                serde_json::to_vec(&SessionFileUploadPayload {
                    session_id,
                    path: "stale-error.bin".to_owned(),
                    size_bytes: 6,
                })
                .unwrap(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        let mut offset = 0;
        let ready_frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let ready_encrypted = decode_binary_encrypted_frame(ready_frame).unwrap();
        let ready: SessionFileHttpUploadReadyPayload = serde_json::from_slice(
            &upload_init_e2ee
                .decrypt_binary_payload(&ready_encrypted)
                .unwrap(),
        )
        .unwrap();

        for (nonce, offset_bytes, bytes, expected_status) in [
            (
                "http-upload-stale-error-head",
                0_u64,
                b"abc".to_vec(),
                StatusCode::OK,
            ),
            (
                "http-upload-stale-error-duplicate",
                0_u64,
                b"xxx".to_vec(),
                StatusCode::BAD_REQUEST,
            ),
            (
                "http-upload-stale-error-tail",
                3_u64,
                b"def".to_vec(),
                StatusCode::OK,
            ),
        ] {
            let (request, _upload_e2ee) = signed_http_e2ee_request(
                &protocol,
                &signing_key,
                device_id,
                "/api/files/upload",
                nonce,
                vec![
                    serde_json::to_vec(&SessionFileHttpUploadStreamPayload {
                        session_id,
                        path: "stale-error.bin".to_owned(),
                        upload_id: ready.upload_id.clone(),
                        size_bytes: 6,
                        offset_bytes,
                    })
                    .unwrap(),
                    bytes,
                ],
            )
            .await;
            let response = router(protocol.clone(), false)
                .oneshot(request)
                .await
                .expect("router should respond");
            assert_eq!(response.status(), expected_status);
            let _ = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
                .await
                .unwrap();
        }

        assert_eq!(fs::read(root.join("stale-error.bin")).unwrap(), b"abcdef");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_upload_cancel_guard_releases_reserved_range() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-upload-cancel-guard-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-upload-cancel-guard");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let creator_signing_key = SigningKey::generate(&mut OsRng);
        let creator_public_key = PublicKey(test_ed25519_wire(
            creator_signing_key.verifying_key().as_bytes(),
        ));
        let (_, session_id) =
            pair_real_device_and_create_session(protocol.clone(), creator_public_key).await;
        let http_signing_key = SigningKey::generate(&mut OsRng);
        let http_public_key = PublicKey(test_ed25519_wire(
            http_signing_key.verifying_key().as_bytes(),
        ));
        let http_device_id = pair_real_device(protocol.clone(), http_public_key).await;
        let mut connection = ProtocolConnection::authenticated_http(http_device_id);
        let ready = {
            let mut protocol_guard = protocol.lock().await;
            protocol_guard
                .attach_session(
                    &mut connection,
                    SessionAttachPayload {
                        session_id,
                        watch_updates: false,
                        last_terminal_seq: None,
                    },
                )
                .unwrap();
            protocol_guard
                .prepare_session_file_http_upload(
                    &connection,
                    SessionFileUploadPayload {
                        session_id,
                        path: "cancel-guard.bin".to_owned(),
                        size_bytes: 3,
                    },
                    http_device_id,
                )
                .unwrap()
        };
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: "cancel-guard.bin".to_owned(),
            upload_id: ready.upload_id,
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let reserved_range = {
            let mut protocol_guard = protocol.lock().await;
            match protocol_guard
                .begin_session_file_http_upload_write(&connection, meta.clone(), http_device_id, 3)
                .unwrap()
            {
                SessionFileHttpUploadBegin::Write(plan) => plan.reserved_range,
                SessionFileHttpUploadBegin::Complete(_) => {
                    panic!("upload should still be active")
                }
            }
        };

        drop(HttpUploadInflightGuard::new(
            protocol.clone(),
            meta.clone(),
            reserved_range,
        ));
        // 中文注释：Drop 里的释放任务是异步执行的；等它拿到 protocol lock 后，
        // 同 offset 的重试才能重新预约并完成写入。
        tokio::time::sleep(Duration::from_millis(25)).await;

        let progress = write_http_file_upload_chunks_without_protocol_io_lock(
            protocol.clone(),
            &connection,
            meta,
            http_device_id,
            vec![b"abc".to_vec()],
        )
        .await
        .unwrap();

        assert!(progress.eof);
        assert_eq!(fs::read(root.join("cancel-guard.bin")).unwrap(), b"abc");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_upload_drop_after_file_write_commits_before_retry() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-upload-drop-commit-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-upload-drop-commit");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let creator_signing_key = SigningKey::generate(&mut OsRng);
        let creator_public_key = PublicKey(test_ed25519_wire(
            creator_signing_key.verifying_key().as_bytes(),
        ));
        let (_, session_id) =
            pair_real_device_and_create_session(protocol.clone(), creator_public_key).await;
        let http_signing_key = SigningKey::generate(&mut OsRng);
        let http_public_key = PublicKey(test_ed25519_wire(
            http_signing_key.verifying_key().as_bytes(),
        ));
        let http_device_id = pair_real_device(protocol.clone(), http_public_key).await;
        let mut connection = ProtocolConnection::authenticated_http(http_device_id);
        let ready = {
            let mut protocol_guard = protocol.lock().await;
            protocol_guard
                .attach_session(
                    &mut connection,
                    SessionAttachPayload {
                        session_id,
                        watch_updates: false,
                        last_terminal_seq: None,
                    },
                )
                .unwrap();
            protocol_guard
                .prepare_session_file_http_upload(
                    &connection,
                    SessionFileUploadPayload {
                        session_id,
                        path: "drop-commit.bin".to_owned(),
                        size_bytes: 3,
                    },
                    http_device_id,
                )
                .unwrap()
        };
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: "drop-commit.bin".to_owned(),
            upload_id: ready.upload_id,
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let (reserved_range, file_result) = {
            let mut protocol_guard = protocol.lock().await;
            let plan = match protocol_guard
                .begin_session_file_http_upload_write(&connection, meta.clone(), http_device_id, 3)
                .unwrap()
            {
                SessionFileHttpUploadBegin::Write(plan) => plan,
                SessionFileHttpUploadBegin::Complete(_) => panic!("upload should still be active"),
            };
            let reserved_range = plan.reserved_range;
            drop(protocol_guard);
            let file_result = write_session_file_http_upload_files(plan, vec![b"abc".to_vec()])
                .expect("file write should succeed before handler drop");
            (reserved_range, file_result)
        };
        let mut inflight_guard =
            HttpUploadInflightGuard::new(protocol.clone(), meta.clone(), reserved_range);
        inflight_guard.mark_written(file_result);
        drop(inflight_guard);
        // 中文注释：模拟 handler 在文件已经落盘、commit response 前被取消；
        // Drop 必须补 commit，后续 retry 不能用不同内容覆盖同一区间。
        tokio::time::sleep(Duration::from_millis(25)).await;

        let retry = write_http_file_upload_chunks_without_protocol_io_lock(
            protocol.clone(),
            &connection,
            meta,
            http_device_id,
            vec![b"xxx".to_vec()],
        )
        .await
        .unwrap();

        assert!(retry.eof);
        assert_eq!(fs::read(root.join("drop-commit.bin")).unwrap(), b"abc");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_upload_connection_guard_detaches_on_drop() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-upload-connection-drop-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-upload-connection-drop");
        config.default_working_directory = Some(root.clone());
        let protocol = default_protocol(config);
        let creator_signing_key = SigningKey::generate(&mut OsRng);
        let creator_public_key = PublicKey(test_ed25519_wire(
            creator_signing_key.verifying_key().as_bytes(),
        ));
        let (_, session_id) =
            pair_real_device_and_create_session(protocol.clone(), creator_public_key).await;
        let http_signing_key = SigningKey::generate(&mut OsRng);
        let http_public_key = PublicKey(test_ed25519_wire(
            http_signing_key.verifying_key().as_bytes(),
        ));
        let http_device_id = pair_real_device(protocol.clone(), http_public_key).await;
        let mut connection = ProtocolConnection::authenticated_http(http_device_id);
        {
            let mut protocol_guard = protocol.lock().await;
            protocol_guard
                .attach_session(
                    &mut connection,
                    SessionAttachPayload {
                        session_id,
                        watch_updates: false,
                        last_terminal_seq: None,
                    },
                )
                .unwrap();
        }
        drop(HttpConnectionCloseGuard::new(protocol.clone(), connection));
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert_http_device_detached(&protocol, session_id, http_device_id).await;
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn http_file_handlers_detach_temporary_runtime_connection() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-file-detach-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-detach");
        config.default_working_directory = Some(root);
        let protocol = default_protocol(config);
        let creator_signing_key = SigningKey::generate(&mut OsRng);
        let creator_public_key = PublicKey(test_ed25519_wire(
            creator_signing_key.verifying_key().as_bytes(),
        ));
        let (_, session_id) =
            pair_real_device_and_create_session(protocol.clone(), creator_public_key).await;
        let http_signing_key = SigningKey::generate(&mut OsRng);
        let http_public_key = PublicKey(test_ed25519_wire(
            http_signing_key.verifying_key().as_bytes(),
        ));
        let http_device_id = pair_real_device(protocol.clone(), http_public_key).await;
        assert_http_device_detached(&protocol, session_id, http_device_id).await;

        let upload_init_path = "/api/files/upload/init";
        let (request, mut upload_init_e2ee) = signed_http_e2ee_request(
            &protocol,
            &http_signing_key,
            http_device_id,
            upload_init_path,
            "http-file-detach-upload-init",
            vec![
                serde_json::to_vec(&SessionFileUploadPayload {
                    session_id,
                    path: "detach.bin".to_owned(),
                    size_bytes: 3,
                })
                .unwrap(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        let mut offset = 0;
        let ready_frame = read_http_e2ee_frame(&response_body, &mut offset).unwrap();
        let ready_encrypted = decode_binary_encrypted_frame(ready_frame).unwrap();
        let ready: SessionFileHttpUploadReadyPayload = serde_json::from_slice(
            &upload_init_e2ee
                .decrypt_binary_payload(&ready_encrypted)
                .unwrap(),
        )
        .unwrap();
        assert_http_device_detached(&protocol, session_id, http_device_id).await;

        let upload_path = "/api/files/upload";
        let (request, _upload_e2ee) = signed_http_e2ee_request(
            &protocol,
            &http_signing_key,
            http_device_id,
            upload_path,
            "http-file-detach-upload-stream",
            vec![
                serde_json::to_vec(&SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: "detach.bin".to_owned(),
                    upload_id: ready.upload_id,
                    size_bytes: 3,
                    offset_bytes: 0,
                })
                .unwrap(),
                b"abc".to_vec(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        assert_http_device_detached(&protocol, session_id, http_device_id).await;

        let download_path = "/api/files/download";
        let (request, _download_e2ee) = signed_http_e2ee_request(
            &protocol,
            &http_signing_key,
            http_device_id,
            download_path,
            "http-file-detach-download",
            vec![
                serde_json::to_vec(&SessionFileHttpDownloadPayload {
                    session_id,
                    path: "detach.bin".to_owned(),
                    offset_bytes: 0,
                })
                .unwrap(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();
        assert_http_device_detached(&protocol, session_id, http_device_id).await;
    }

    #[tokio::test]
    async fn http_file_handlers_do_not_decrement_active_client_history_counts() {
        let root = std::env::temp_dir().join(format!(
            "termd-http-file-history-{}-{}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        fs::create_dir_all(&root).unwrap();
        let mut config = test_config("http-file-history");
        config.default_working_directory = Some(root);
        let protocol = default_protocol(config);
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(test_ed25519_wire(signing_key.verifying_key().as_bytes()));

        let (device_id, session_id, mut live_connection, live_device_session) = {
            let mut protocol_guard = protocol.lock().await;
            let (mut connection, _) = protocol_guard.start_connection();
            let device_id = DeviceId::new();
            let device_keypair = E2eeKeyPair::generate();
            let mut device_session = open_test_e2ee(
                &mut protocol_guard,
                &mut connection,
                device_id,
                &device_keypair,
            );
            let pair_request = envelope_value(
                MessageType::PairRequest,
                PairRequestPayload {
                    device_id,
                    device_public_key: public_key.clone(),
                    token: protocol_guard
                        .issue_pairing_token(current_unix_timestamp_millis())
                        .unwrap()
                        .token()
                        .clone(),
                    nonce: termd_proto::Nonce("http-file-history-pair".to_owned()),
                    timestamp_ms: current_unix_timestamp_millis(),
                },
            )
            .unwrap();
            let frame = device_session.encrypt_json_payload(&pair_request).unwrap();
            let pair_responses = connection.handle_wire_envelope(
                &mut protocol_guard,
                envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
            );
            let response_frame =
                encrypted_frame_from_envelope(pair_responses.into_iter().next().unwrap()).unwrap();
            let pair_accept = device_session
                .decrypt_json_payload::<JsonEnvelope>(&response_frame)
                .unwrap();
            assert_eq!(pair_accept.kind, MessageType::PairAccept);

            let create_request = envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::default(),
                },
            )
            .unwrap();
            let create_frame = device_session
                .encrypt_json_payload(&create_request)
                .unwrap();
            let create_responses = connection.handle_wire_envelope(
                &mut protocol_guard,
                envelope_value(MessageType::EncryptedFrame, create_frame).unwrap(),
            );
            let created_frame = encrypted_frame_from_envelope(
                create_responses
                    .into_iter()
                    .next()
                    .expect("session create should return a response"),
            )
            .unwrap();
            let created_envelope = device_session
                .decrypt_json_payload::<JsonEnvelope>(&created_frame)
                .unwrap();
            let created_payload: SessionCreatedPayload =
                decode_payload(created_envelope.payload).unwrap();
            let session_id = created_payload.session_id;
            assert_eq!(
                protocol_guard
                    .client_history_active_connection_count_for_test(device_id)
                    .unwrap(),
                Some(1)
            );
            (device_id, session_id, connection, device_session)
        };

        let upload_init_path = "/api/files/upload/init";
        let (request, _http_e2ee) = signed_http_e2ee_request(
            &protocol,
            &signing_key,
            device_id,
            upload_init_path,
            "http-file-history-upload-init",
            vec![
                serde_json::to_vec(&SessionFileUploadPayload {
                    session_id,
                    path: "history.bin".to_owned(),
                    size_bytes: 0,
                })
                .unwrap(),
            ],
        )
        .await;
        let response = router(protocol.clone(), false)
            .oneshot(request)
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), HTTP_E2EE_INIT_MAX_BYTES)
            .await
            .unwrap();

        let protocol_guard = protocol.lock().await;
        assert_eq!(
            protocol_guard
                .client_history_active_connection_count_for_test(device_id)
                .unwrap(),
            Some(1)
        );
        drop(protocol_guard);
        drop(live_device_session);
        let mut protocol_guard = protocol.lock().await;
        live_connection.close(&mut protocol_guard);
    }

    #[tokio::test]
    async fn http_e2ee_short_body_read_times_out() {
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());

        assert!(matches!(
            read_http_e2ee_short_body(body).await,
            Err(ProtocolError::InvalidEnvelope)
        ));
    }

    #[tokio::test]
    async fn http_e2ee_upload_metadata_frame_read_times_out() {
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let mut e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon_keypair,
            device_keypair.public_key(),
            E2eeSessionContext::new(
                ServerId::new(),
                DeviceId::new(),
                daemon_keypair.public_key(),
                device_keypair.public_key(),
            ),
        )
        .unwrap();
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
        let mut stream = HttpE2eeBodyFrameStream::new(body);

        assert!(matches!(
            read_http_e2ee_metadata_frame(&mut stream, &mut e2ee).await,
            Err(ProtocolError::InvalidEnvelope)
        ));
    }

    #[tokio::test]
    async fn http_e2ee_body_stream_rejects_oversized_pending_before_buffering() {
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let mut e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon_keypair,
            device_keypair.public_key(),
            E2eeSessionContext::new(
                ServerId::new(),
                DeviceId::new(),
                daemon_keypair.public_key(),
                device_keypair.public_key(),
            ),
        )
        .unwrap();
        let raw = vec![0_u8; HTTP_E2EE_MAX_PENDING_BYTES + 1];
        let mut stream = HttpE2eeBodyFrameStream::new(Body::from(raw));

        assert!(matches!(
            stream.next_plaintext(&mut e2ee).await,
            Err(ProtocolError::InvalidEnvelope)
        ));
    }

    #[tokio::test]
    async fn http_e2ee_body_stream_accepts_coalesced_valid_frames() {
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let mut daemon_e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon_keypair,
            device_keypair.public_key(),
            E2eeSessionContext::new(
                server_id,
                device_id,
                daemon_keypair.public_key(),
                device_keypair.public_key(),
            ),
        )
        .unwrap();
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            daemon_keypair.public_key(),
            E2eeSessionContext::new(
                server_id,
                device_id,
                daemon_keypair.public_key(),
                device_keypair.public_key(),
            ),
        )
        .unwrap();
        let mut raw = Vec::new();
        let frame_count = 10_u8;
        for value in 0..frame_count {
            append_http_e2ee_binary_frame(&mut device_e2ee, &mut raw, &vec![value; 256 * 1024])
                .unwrap();
        }
        assert!(raw.len() > HTTP_E2EE_MAX_PENDING_BYTES);
        let mut stream = HttpE2eeBodyFrameStream::new(Body::from(raw));

        for value in 0..frame_count {
            let plaintext = stream
                .next_plaintext(&mut daemon_e2ee)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(plaintext, vec![value; 256 * 1024]);
        }
        assert!(
            stream
                .next_plaintext(&mut daemon_e2ee)
                .await
                .unwrap()
                .is_none()
        );
    }

    struct RawHttpResponse {
        status: u16,
        headers: String,
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
    async fn local_pairing_token_endpoint_does_not_expose_cors_headers() {
        let protocol = test_protocol("local-pairing-token-no-cors");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });
        let response = tokio::task::spawn_blocking(move || {
            post_pairing_token_with_origin(addr, "http://evil.example")
        })
        .await
        .unwrap();
        server.abort();

        assert_eq!(response.status, 200);
        // 中文注释：浏览器能否读取跨源响应，关键看真实 POST 响应里有没有 ACAO；
        // 这里只要不回该头，恶意网页就拿不到 pairing token 明文。
        assert!(
            !response
                .headers
                .to_ascii_lowercase()
                .contains("access-control-allow-origin")
        );
        let payload: PairingTokenResponse = serde_json::from_str(&response.body).unwrap();
        assert!(payload.token.starts_with("termd-pair-"));
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
    async fn websocket_writer_is_aborted_when_client_context_closes() {
        let protocol = test_protocol("websocket-writer-abort-on-close");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let server_id = protocol.lock().await.server_id();
        send_ws_route_hello(&mut socket, server_id).await;
        let _hello = timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("hello should arrive")
            .expect("websocket should stay open")
            .expect("hello frame should decode");

        socket.close(None).await.unwrap();

        // 中文注释：客户端关闭后 daemon 必须取消该 client 的 writer context。
        // 如果继续 drain 旧队列，连接清理会卡在 socket write 上并产生 Sending-after-closing 日志。
        timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("server should finish websocket promptly after client close");

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_pushes_session_output_without_client_pull_frame() {
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
        assert_eq!(
            created.kind,
            MessageType::SessionCreated,
            "unexpected session create response: {created:?}"
        );
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        // 这里不再向 WebSocket 发送 ping 或任意业务帧；PTY 后续输出必须由 daemon 主动推送。
        // 等待窗口需要覆盖 CI 或本地 workspace 并发测试时的 PTY 进程启动抖动，
        // 这个值不是产品 WebSocket 的超时语义。
        let mut pushed_output = Vec::new();
        let push_deadline = Instant::now() + Duration::from_secs(8);
        while !pushed_output
            .windows(b"pushed-output".len())
            .any(|window| window == b"pushed-output")
        {
            let remaining = push_deadline.saturating_duration_since(Instant::now());
            let pushed = timeout(
                remaining,
                read_encrypted_ws(&mut socket, &mut device_session),
            )
            .await
            .expect("daemon should push PTY output without client pull frames");
            if pushed.kind != MessageType::SessionData {
                continue;
            }
            let payload: SessionDataPayload = decode_payload(pushed.payload).unwrap();
            assert_eq!(payload.session_id, created_payload.session_id);
            pushed_output.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap(),
            );
        }

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
        assert_eq!(WEBSOCKET_MAX_FRAME_SIZE, 16 * 1024 * 1024);
        assert_eq!(WEBSOCKET_MAX_MESSAGE_SIZE, 16 * 1024 * 1024);
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
        pairing_token_request(addr, None)
    }

    fn post_pairing_token_with_origin(addr: SocketAddr, origin: &str) -> RawHttpResponse {
        pairing_token_request(addr, Some(origin))
    }

    fn pairing_token_request(addr: SocketAddr, origin: Option<&str>) -> RawHttpResponse {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let origin_header = origin
            .map(|value| format!("Origin: {value}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST /local/pairing-token HTTP/1.1\r\nHost: {addr}\r\n{origin_header}Content-Length: 0\r\nConnection: close\r\n\r\n"
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
            headers: head.to_owned(),
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
                client_id: None,
                data_token: None,
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

    async fn signed_http_e2ee_request(
        protocol: &SharedDaemonProtocol,
        signing_key: &SigningKey,
        device_id: DeviceId,
        path: &str,
        nonce: &str,
        plaintext_frames: Vec<Vec<u8>>,
    ) -> (Request<Body>, E2eeSession) {
        let protocol_guard = protocol.lock().await;
        let server_id = protocol_guard.server_id();
        let daemon_identity = protocol_guard.daemon_public_identity().clone();
        let daemon_e2ee_public = protocol_guard.e2ee_public_key();
        drop(protocol_guard);

        let http_keypair = E2eeKeyPair::generate();
        let mut http_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &http_keypair,
            daemon_e2ee_public,
            E2eeSessionContext::new(
                server_id,
                device_id,
                daemon_e2ee_public,
                http_keypair.public_key(),
            ),
        )
        .unwrap();
        let mut auth = HttpE2eeAuthPayload {
            device_id,
            e2ee_public_key: http_keypair.public_key_wire(),
            nonce: termd_proto::Nonce(nonce.to_owned()),
            timestamp_ms: current_unix_timestamp_millis(),
            method: "POST".to_owned(),
            path: path.to_owned(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        auth.signature = Signature(test_ed25519_wire(
            &signing_key
                .sign(&HttpE2eeSigningInput::from_payload(&auth, &daemon_identity).to_bytes())
                .to_bytes(),
        ));

        let mut request_body = Vec::new();
        for plaintext in plaintext_frames {
            let encrypted = http_e2ee.encrypt_binary_payload(&plaintext).unwrap();
            request_body.extend(write_http_e2ee_frame(&encode_binary_encrypted_frame(
                &encrypted,
            )));
        }

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/octet-stream")
            .header("x-termd-device-id", device_id.0.to_string())
            .header("x-termd-e2ee-public-key", auth.e2ee_public_key.0)
            .header("x-termd-e2ee-nonce", auth.nonce.0)
            .header("x-termd-e2ee-timestamp-ms", auth.timestamp_ms.0.to_string())
            .header("x-termd-e2ee-signature", auth.signature.0)
            .body(Body::from(request_body))
            .expect("test request should build");

        (request, http_e2ee)
    }

    async fn assert_http_device_detached(
        protocol: &SharedDaemonProtocol,
        session_id: SessionId,
        device_id: DeviceId,
    ) {
        let mut protocol = protocol.lock().await;
        assert!(matches!(
            protocol.runtime_write_input_as_device_for_test(session_id, device_id, b""),
            Err(ProtocolError::RuntimeFailed)
        ));
    }

    async fn pair_real_device(
        protocol: SharedDaemonProtocol,
        device_public_key: PublicKey,
    ) -> DeviceId {
        let mut protocol = protocol.lock().await;
        let pairing_token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let mut device_session =
            open_test_e2ee(&mut protocol, &mut connection, device_id, &device_keypair);
        let pair_request = envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id,
                device_public_key,
                token: pairing_token,
                nonce: termd_proto::Nonce("http-real-device-pair-only".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let frame = device_session.encrypt_json_payload(&pair_request).unwrap();
        let pair_responses = connection.handle_wire_envelope(
            &mut protocol,
            envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
        );
        let response_frame =
            encrypted_frame_from_envelope(pair_responses.into_iter().next().unwrap()).unwrap();
        let pair_accept = device_session
            .decrypt_json_payload::<JsonEnvelope>(&response_frame)
            .unwrap();
        assert_eq!(pair_accept.kind, MessageType::PairAccept);
        connection.close(&mut protocol);
        device_id
    }

    async fn pair_real_device_and_create_session(
        protocol: SharedDaemonProtocol,
        device_public_key: PublicKey,
    ) -> (DeviceId, SessionId) {
        let mut protocol = protocol.lock().await;
        let pairing_token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let mut device_session =
            open_test_e2ee(&mut protocol, &mut connection, device_id, &device_keypair);
        let pair_request = envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id,
                device_public_key,
                token: pairing_token,
                nonce: termd_proto::Nonce("http-real-device-pair".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let frame = device_session.encrypt_json_payload(&pair_request).unwrap();
        let pair_responses = connection.handle_wire_envelope(
            &mut protocol,
            envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
        );
        let response_frame =
            encrypted_frame_from_envelope(pair_responses.into_iter().next().unwrap()).unwrap();
        let pair_accept = device_session
            .decrypt_json_payload::<JsonEnvelope>(&response_frame)
            .unwrap();
        assert_eq!(pair_accept.kind, MessageType::PairAccept);

        let create = envelope_value(
            MessageType::SessionCreate,
            SessionCreatePayload {
                command: vec!["sh".to_owned(), "-lc".to_owned(), "cat".to_owned()],
                size: TerminalSize::default(),
            },
        )
        .unwrap();
        let frame = device_session.encrypt_json_payload(&create).unwrap();
        let create_responses = connection.handle_wire_envelope(
            &mut protocol,
            envelope_value(MessageType::EncryptedFrame, frame).unwrap(),
        );
        let response_frame =
            encrypted_frame_from_envelope(create_responses.into_iter().next().unwrap()).unwrap();
        let created = device_session
            .decrypt_json_payload::<JsonEnvelope>(&response_frame)
            .unwrap();
        assert_eq!(
            created.kind,
            MessageType::SessionCreated,
            "unexpected session create response: {created:?}"
        );
        let payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        (device_id, payload.session_id)
    }

    fn test_ed25519_wire(bytes: &[u8]) -> String {
        format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
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
