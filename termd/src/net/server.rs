//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth 和 session
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

mod recovery;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, SeekFrom};
use std::net::{AddrParseError, IpAddr, SocketAddr};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, OriginalUri, Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HeaderName,
    HeaderValue, SET_COOKIE,
};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, options, post, put};
use axum::{Json, Router};
use base64::{Engine as _, engine::general_purpose};
#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use termd_proto::{
    DeviceId, ErrorPayload, PROTOCOL_PACKET_VERSION, ProtocolVersion, ServerId,
    SessionFileDownloadPreparePayload, SessionFileUploadPayload, SessionId, SessionState,
    UnixTimestampMillis, is_http_control_tunnel_path_allowed, is_http_tunnel_path_allowed,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tower::ServiceExt as _;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tracing::warn;

use crate::auth::current_unix_timestamp_millis;
use crate::browser::{
    BrowserCreateRequest, BrowserError, BrowserSession, BrowserSessionId, BrowserWorkspace,
};
use crate::config::DaemonConfig;
#[cfg(test)]
use crate::file_offer::FILE_OFFER_LIMIT;
use crate::file_offer::{FileOfferError, inspect_file_offer};
use crate::notifications::{PushNotificationCoordinator, PushNotificationError};
use crate::pty::PtyRestoreInfo;
use crate::pty::supervisor::SupervisorPtyBackend;
use crate::state::web_push::{PushNotificationLocale, PushNotificationMode, PushSubscription};
use crate::state::{StateError, StateStore};

use super::client_diagnostics;
#[cfg(test)]
use super::protocol::V070TerminalOpen;
use super::protocol::{
    DaemonProtocol, FileOfferDownloadError, ProtocolConnection, ProtocolError,
    cleanup_persisted_session_file_http_uploads, file_offer_download_cookie_name,
    parse_v070_terminal_open,
};
use super::signature::Ed25519SignatureVerifier;
use recovery::warn_about_orphaned_supervisors;

const HTTP_JSON_MAX_BYTES: usize = 1024 * 1024;
const PUSH_SUBSCRIPTION_JSON_MAX_BYTES: usize = 8 * 1024;
const PUSH_ENDPOINT_MAX_BYTES: usize = 2 * 1024;
const PUSH_P256DH_MAX_BYTES: usize = 128;
const PUSH_AUTH_MAX_BYTES: usize = 64;
const V070_FILE_CHUNK_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_METADATA_TIMESTAMP_MS: u64 = 9_007_199_254_740_991;
const BROWSER_DOWNLOAD_SCAN_INTERVAL: Duration = Duration::from_millis(500);

pub type DefaultDaemonProtocol = DaemonProtocol<SupervisorPtyBackend, Ed25519SignatureVerifier>;
/// daemon 的协议核心仍是单线程语义，但等待这把锁必须让出 Tokio worker。
///
/// 直连 WebSocket 和 relay mux 共用同一个协议状态；如果使用 `std::sync::Mutex`，
/// 快速切换大输出 session 时多个任务会在 worker 线程上阻塞等待锁，连心跳、输入和
/// relay 主干读写都会一起迟滞。`tokio::sync::Mutex` 保持串行临界区，同时让等待者挂起。
pub struct DaemonSharedState {
    protocol: Mutex<DefaultDaemonProtocol>,
    browser: BrowserWorkspace,
}

impl DaemonSharedState {
    fn new(protocol: DefaultDaemonProtocol, state_path: &std::path::Path) -> Self {
        Self {
            protocol: Mutex::new(protocol),
            browser: BrowserWorkspace::for_state_path(state_path),
        }
    }

    pub fn browser(&self) -> &BrowserWorkspace {
        &self.browser
    }
}

impl Deref for DaemonSharedState {
    type Target = Mutex<DefaultDaemonProtocol>;

    fn deref(&self) -> &Self::Target {
        &self.protocol
    }
}

pub type SharedDaemonProtocol = Arc<DaemonSharedState>;

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
    daemon_public_key: termd_proto::PublicKey,
}

#[derive(Debug, Serialize)]
struct LocalPairingTokenPayload {
    token: String,
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
    // 中文注释：生产路径现在只接受 supervisor Unix socket restore_info；旧阶段遗留的
    // live supervisor 仍只做孤儿告警，不能再被默认启动路径自动接回运行态。
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
    Ok(Arc::new(DaemonSharedState::new(
        protocol,
        &config.state_path,
    )))
}

/// 测试与旧调用点使用的便捷构造器；生产启动路径使用 `try_default_protocol` 返回结构化错误。
pub fn default_protocol(config: DaemonConfig) -> SharedDaemonProtocol {
    try_default_protocol(config).expect("default daemon protocol should initialize")
}

pub fn router(protocol: SharedDaemonProtocol, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version_endpoint))
        .route("/local/pairing-token", post(local_pairing_token))
        .merge(auth_api_router())
        .merge(http_control_api_router())
        .merge(http_file_api_router())
        .merge(push_api_router())
        .merge(browser_api_router())
        .merge(update_api_router())
        .route("/ws/metadata", get(metadata_ws_handler))
        .route("/ws/terminal", get(terminal_ws_handler))
        .route("/ws/browser/:browser_id", get(browser_ws_handler))
        .method_not_allowed_fallback(api_method_not_allowed)
        .with_state(protocol);

    if web_enabled {
        router.fallback(web_or_api_fallback)
    } else {
        router.fallback(api_or_plain_not_found)
    }
}

/// 版本检查与一键更新路由。更新端点只接受已认证设备的 Bearer access token；
/// relay 更新由 daemon 用其 admission token 委托给 relay 的受限 `/update` 端点。
fn update_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route("/api/update/check", post(update_check))
        .route("/api/update/apply", post(update_apply))
        .route("/api/update/relay", post(update_relay))
}

/// Local daemon-control routes. This router is served only over the Unix socket.
pub fn daemon_control_router(protocol: SharedDaemonProtocol) -> Router {
    Router::new()
        .route("/v1/file-offers", post(control_file_offer_create))
        .route(
            "/v1/browser/sessions",
            get(control_browser_sessions_list).merge(post(control_browser_session_create)),
        )
        .route(
            "/v1/browser/sessions/:browser_id",
            delete(control_browser_session_close),
        )
        .route(
            "/v1/browser/sessions/:browser_id/navigate",
            post(control_browser_navigate),
        )
        .route(
            "/v1/browser/sessions/:browser_id/snapshot",
            get(control_browser_snapshot),
        )
        .route(
            "/v1/browser/sessions/:browser_id/click",
            post(control_browser_click),
        )
        .route(
            "/v1/browser/sessions/:browser_id/fill",
            post(control_browser_fill),
        )
        .route(
            "/v1/browser/sessions/:browser_id/wait-download",
            post(control_browser_wait_download),
        )
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(|| async {
            api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "daemon control route was not found",
                false,
            )
        })
        .with_state(protocol)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserNavigateRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserClickRequest {
    selector: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserFillRequest {
    selector: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserWaitDownloadRequest {
    #[serde(default = "default_browser_download_wait_ms")]
    timeout_ms: u64,
}

const fn default_browser_download_wait_ms() -> u64 {
    30_000
}

async fn control_browser_sessions_list(State(protocol): State<SharedDaemonProtocol>) -> Response {
    match protocol.browser().list().await {
        Ok(sessions) => {
            (StatusCode::OK, Json(BrowserSessionsResponse { sessions })).into_response()
        }
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_session_create(
    State(protocol): State<SharedDaemonProtocol>,
    request: Result<Json<BrowserCreateRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol.browser().create(request).await {
        Ok(session) => (StatusCode::CREATED, Json(session)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_session_close(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
) -> Response {
    match protocol.browser().close(browser_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_navigate(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    request: Result<Json<BrowserNavigateRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol.browser().navigate(browser_id, &request.url).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_snapshot(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
) -> Response {
    match protocol.browser().snapshot(browser_id).await {
        Ok(snapshot) => (StatusCode::OK, Json(snapshot)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_click(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    request: Result<Json<BrowserClickRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol
        .browser()
        .click(browser_id, &request.selector)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_fill(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    request: Result<Json<BrowserFillRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol
        .browser()
        .fill(browser_id, &request.selector, &request.value)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn control_browser_wait_download(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    request: Result<Json<BrowserWaitDownloadRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol
        .browser()
        .wait_download(browser_id, request.timeout_ms)
        .await
    {
        Ok(download) => (StatusCode::OK, Json(download)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn api_or_plain_not_found(uri: OriginalUri) -> Response {
    if is_api_fallback_path(uri.0.path()) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "application route was not found",
            false,
        );
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn web_or_api_fallback(method: Method, uri: OriginalUri, headers: HeaderMap) -> Response {
    if is_api_fallback_path(uri.0.path()) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "application route was not found",
            false,
        );
    }

    termweb::embedded_web_handler_with_headers(method, uri, headers).await
}

#[derive(Debug, Serialize)]
struct ApplicationErrorBody {
    error: ApplicationError,
}

#[derive(Debug, Serialize)]
struct ApplicationError {
    code: &'static str,
    message: &'static str,
    retryable: bool,
}

fn api_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retryable: bool,
) -> Response {
    (
        status,
        Json(ApplicationErrorBody {
            error: ApplicationError {
                code,
                message,
                retryable,
            },
        }),
    )
        .into_response()
}

async fn api_method_not_allowed() -> Response {
    api_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this route",
        false,
    )
}

fn auth_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route("/api/auth/pair", post(auth_pair))
        .route("/api/auth/challenge", post(auth_challenge))
        .route("/api/auth/access-token", post(auth_access_token))
        .route(
            "/api/auth/device-certificate/migrate",
            post(auth_device_certificate_migrate),
        )
        .route(
            "/api/auth/device-certificate/migrate/challenge",
            post(auth_device_certificate_migration_challenge),
        )
}

fn push_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route(
            "/api/push/config",
            get(push_config).merge(options(v070_preflight)),
        )
        .route(
            "/api/push/subscription",
            put(push_subscription_upsert)
                .merge(delete(push_subscription_delete))
                .merge(options(v070_preflight)),
        )
        .route_layer(push_api_cors_layer())
}

fn browser_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route(
            "/api/browser/sessions",
            get(browser_sessions_list)
                .merge(post(browser_session_create))
                .merge(options(v070_preflight)),
        )
        .route(
            "/api/browser/sessions/:browser_id",
            delete(browser_session_close).merge(options(v070_preflight)),
        )
        .route_layer(browser_api_cors_layer())
}

#[derive(Debug, Serialize)]
struct BrowserSessionsResponse {
    sessions: Vec<BrowserSession>,
}

async fn browser_sessions_list(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_v070_http_device(&protocol, &headers).await {
        return response;
    }
    match protocol.browser().list().await {
        Ok(sessions) => {
            (StatusCode::OK, Json(BrowserSessionsResponse { sessions })).into_response()
        }
        Err(error) => browser_operation_error(error),
    }
}

async fn browser_session_create(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    request: Result<Json<BrowserCreateRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = authorize_v070_http_device(&protocol, &headers).await {
        return response;
    }
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    match protocol.browser().create(request).await {
        Ok(session) => (StatusCode::CREATED, Json(session)).into_response(),
        Err(error) => browser_operation_error(error),
    }
}

async fn browser_session_close(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_v070_http_device(&protocol, &headers).await {
        return response;
    }
    match protocol.browser().close(browser_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => browser_operation_error(error),
    }
}

fn browser_operation_error(error: BrowserError) -> Response {
    let status = match error {
        BrowserError::InvalidUrl
        | BrowserError::InvalidViewport
        | BrowserError::AutomationRequestInvalid => StatusCode::BAD_REQUEST,
        BrowserError::CapacityExceeded => StatusCode::TOO_MANY_REQUESTS,
        BrowserError::SessionNotFound | BrowserError::AutomationTargetNotFound => {
            StatusCode::NOT_FOUND
        }
        BrowserError::SessionNotRunning | BrowserError::AutomationBusy => StatusCode::CONFLICT,
        BrowserError::AutomationTimeout => StatusCode::GATEWAY_TIMEOUT,
        BrowserError::StorageUnavailable
        | BrowserError::StateInvalid
        | BrowserError::StateWriteFailed => StatusCode::INTERNAL_SERVER_ERROR,
        BrowserError::RuntimeUnavailable
        | BrowserError::UnsupportedArchitecture
        | BrowserError::RuntimeDownloadFailed
        | BrowserError::RuntimeManifestInvalid
        | BrowserError::RuntimeArchiveInvalid
        | BrowserError::RuntimeInstallFailed
        | BrowserError::ChromiumUnavailable
        | BrowserError::SupervisorArgumentsInvalid
        | BrowserError::SupervisorStartFailed
        | BrowserError::SupervisorStartTimeout
        | BrowserError::SupervisorStopFailed
        | BrowserError::RfbUnavailable
        | BrowserError::AutomationUnavailable
        | BrowserError::AutomationFailed => StatusCode::SERVICE_UNAVAILABLE,
    };
    api_error(
        status,
        error.code(),
        error.safe_message(),
        error.retryable(),
    )
}

#[derive(Debug, Serialize)]
struct PushConfigResponse {
    server_id: ServerId,
    application_server_key: String,
    subscribed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushSubscriptionRequest {
    endpoint: String,
    keys: PushSubscriptionKeys,
    #[serde(default, rename = "mode")]
    _mode: Option<PushNotificationMode>,
    locale: PushNotificationLocale,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushSubscriptionKeys {
    p256dh: String,
    auth: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushSubscriptionDeleteRequest {
    endpoint: String,
}

#[derive(Debug, Deserialize)]
struct PairDeviceRequest {
    device_id: DeviceId,
    device_public_key: termd_proto::PublicKey,
}

#[derive(Debug, Serialize)]
struct PairDeviceResponse {
    server_id: ServerId,
    device_id: DeviceId,
    device_certificate: String,
}

#[derive(Debug, Deserialize)]
struct DeviceChallengeRequest {
    device_id: DeviceId,
}

#[derive(Debug, Serialize)]
struct AccessTokenResponse {
    access_token: String,
    token_type: &'static str,
    issued_at_ms: UnixTimestampMillis,
    expires_at_ms: UnixTimestampMillis,
    refresh_at_ms: UnixTimestampMillis,
}

#[derive(Debug, Serialize)]
struct DeviceCertificateResponse {
    device_certificate: String,
}

// Returning a complete rejection response keeps parsing errors at this HTTP boundary.
#[allow(clippy::result_large_err)]
fn authorization_credential<'a>(
    headers: &'a HeaderMap,
    expected_scheme: &str,
) -> Result<&'a str, Response> {
    let raw = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "authorization_required",
                "valid authorization credentials are required",
                false,
            )
        })?;
    let (scheme, credential) = raw.split_once(' ').ok_or_else(|| {
        api_error(
            StatusCode::UNAUTHORIZED,
            "authorization_invalid",
            "authorization credentials are invalid",
            false,
        )
    })?;
    if scheme != expected_scheme
        || credential.is_empty()
        || credential.contains(char::is_whitespace)
    {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "authorization_invalid",
            "authorization credentials are invalid",
            false,
        ));
    }
    Ok(credential)
}

async fn auth_pair(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    request: Result<Json<PairDeviceRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    let ticket = match authorization_credential(&headers, "TermdPair") {
        Ok(ticket) => ticket,
        Err(response) => return response,
    };
    let now_ms = current_unix_timestamp_millis();
    let mut protocol = protocol.lock().await;
    match protocol.pair_device_certificate(
        ticket,
        request.device_id,
        request.device_public_key,
        now_ms,
    ) {
        Ok(device_certificate) => (
            StatusCode::OK,
            Json(PairDeviceResponse {
                server_id: protocol.server_id(),
                device_id: request.device_id,
                device_certificate,
            }),
        )
            .into_response(),
        Err(_) => api_error(
            StatusCode::UNAUTHORIZED,
            "pair_ticket_invalid",
            "pair ticket is invalid or expired",
            false,
        ),
    }
}

async fn auth_challenge(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    request: Result<Json<DeviceChallengeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    let certificate = match authorization_credential(&headers, "TermdDevice") {
        Ok(certificate) => certificate,
        Err(response) => return response,
    };
    let mut protocol = protocol.lock().await;
    match protocol.issue_access_token_challenge(
        certificate,
        request.device_id,
        current_unix_timestamp_millis(),
    ) {
        Ok(challenge) => (StatusCode::OK, Json(challenge)).into_response(),
        Err(_) => api_error(
            StatusCode::UNAUTHORIZED,
            "device_certificate_invalid",
            "device certificate is invalid or revoked",
            false,
        ),
    }
}

async fn auth_access_token(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    payload: Result<Json<termd_proto::AuthPayload>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_json_error(),
    };
    let certificate = match authorization_credential(&headers, "TermdDevice") {
        Ok(certificate) => certificate,
        Err(response) => return response,
    };
    let now_ms = current_unix_timestamp_millis();
    let mut protocol = protocol.lock().await;
    match protocol.exchange_access_token(certificate, payload, now_ms) {
        Ok((access_token, expires_at_ms)) => (
            StatusCode::OK,
            Json(AccessTokenResponse {
                access_token,
                token_type: "Bearer",
                issued_at_ms: now_ms,
                expires_at_ms,
                refresh_at_ms: UnixTimestampMillis(expires_at_ms.0.saturating_sub(60_000)),
            }),
        )
            .into_response(),
        Err(_) => api_error(
            StatusCode::UNAUTHORIZED,
            "device_proof_invalid",
            "device private-key proof is invalid",
            false,
        ),
    }
}

async fn auth_device_certificate_migrate(
    State(protocol): State<SharedDaemonProtocol>,
    payload: Result<Json<termd_proto::AuthPayload>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_json_error(),
    };
    let mut protocol = protocol.lock().await;
    match protocol.migrate_device_certificate(payload, current_unix_timestamp_millis()) {
        Ok(device_certificate) => (
            StatusCode::OK,
            Json(DeviceCertificateResponse { device_certificate }),
        )
            .into_response(),
        Err(_) => api_error(
            StatusCode::UNAUTHORIZED,
            "device_migration_proof_invalid",
            "device migration proof is invalid or expired",
            false,
        ),
    }
}

async fn auth_device_certificate_migration_challenge(
    State(protocol): State<SharedDaemonProtocol>,
    request: Result<Json<DeviceChallengeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_json_error(),
    };
    let mut protocol = protocol.lock().await;
    match protocol.issue_device_certificate_migration_challenge(
        request.device_id,
        current_unix_timestamp_millis(),
    ) {
        Ok(challenge) => (StatusCode::OK, Json(challenge)).into_response(),
        Err(_) => api_error(
            StatusCode::UNAUTHORIZED,
            "device_migration_not_allowed",
            "device is not eligible for credential migration",
            false,
        ),
    }
}

fn invalid_json_error() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_json",
        "request body must be valid JSON",
        false,
    )
}

fn websocket_access_token<'a>(headers: &'a HeaderMap, protocol: &str) -> Option<&'a str> {
    let mut protocols = headers
        .get("sec-websocket-protocol")?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim);
    if protocols.next()? != protocol {
        return None;
    }
    protocols
        .next()
        .filter(|token| token.split('.').count() == 3 && !token.contains(char::is_whitespace))
}

async fn authorize_workspace_websocket(
    protocol: &SharedDaemonProtocol,
    headers: &HeaderMap,
    websocket_protocol: &str,
) -> Result<DeviceId, Response> {
    let token = websocket_access_token(headers, websocket_protocol).ok_or_else(|| {
        api_error(
            StatusCode::UNAUTHORIZED,
            "access_token_required",
            "a valid access token is required",
            false,
        )
    })?;
    protocol
        .lock()
        .await
        .verify_access_token_credential(token, current_unix_timestamp_millis())
        .map_err(|_| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "access_token_invalid",
                "access token is invalid or expired",
                false,
            )
        })
}

async fn metadata_ws_handler(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let device_id = match authorize_workspace_websocket(&protocol, &headers, "termd.v0.7").await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    websocket
        .protocols(["termd.v0.7"])
        .on_upgrade(move |socket| run_metadata_websocket(socket, protocol, device_id))
        .into_response()
}

async fn run_metadata_websocket(
    mut socket: WebSocket,
    protocol: SharedDaemonProtocol,
    device_id: DeviceId,
) {
    let mut revision = 1_u64;
    let (mut changes, mut file_offers, mut previous) = {
        let mut guard = protocol.lock().await;
        let changes = guard.v070_metadata_signal();
        let file_offers = guard.file_offer_events();
        let payload = match guard.v070_metadata_payload(device_id) {
            Ok(payload) => payload,
            Err(_) => return,
        };
        (changes, file_offers, payload)
    };
    if send_v070_json(
        &mut socket,
        "metadata.snapshot",
        serde_json::json!({"revision": revision, "state": previous}),
    )
    .await
    .is_err()
    {
        return;
    }
    let mut client_diagnostics_budget = client_diagnostics::ClientDiagnosticsBudget::new();
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(raw))) => {
                    let _ = client_diagnostics::handle_client_diagnostics_message(
                        &device_id,
                        &raw,
                        &mut client_diagnostics_budget,
                    );
                    let timestamp_ms = serde_json::from_str::<Value>(&raw).ok()
                        .filter(|value| value.get("type").and_then(Value::as_str) == Some("metadata.ping"))
                        .and_then(|value| value.get("payload")?.get("timestamp_ms")?.as_u64())
                        .filter(|timestamp_ms| *timestamp_ms <= MAX_METADATA_TIMESTAMP_MS);
                    if let Some(timestamp_ms) = timestamp_ms {
                        let _ = send_v070_json(&mut socket, "metadata.pong", serde_json::json!({
                            "timestamp_ms": timestamp_ms
                        })).await;
                    }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            changed = changes.changed() => {
                if changed.is_err() { break; }
                let current = match protocol.lock().await.v070_metadata_payload(device_id) {
                    Ok(payload) => payload,
                    Err(_) => break,
                };
                if current != previous {
                    revision = revision.saturating_add(1);
                    previous = current.clone();
                    if send_v070_json(
                        &mut socket,
                        "metadata.update",
                        serde_json::json!({"revision": revision, "state": current}),
                    ).await.is_err() { break; }
                }
            },
            offer = file_offers.recv() => match offer {
                Ok(offer) => {
                    if send_v070_json(&mut socket, "file.offer", offer).await.is_err() {
                        break;
                    }
                }
                // create_file_offer applies backpressure before this can occur. If the
                // invariant is ever broken, close instead of silently losing a one-shot event.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

async fn terminal_ws_handler(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let device_id = match authorize_workspace_websocket(&protocol, &headers, "termd.v0.7").await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    websocket
        .protocols(["termd.v0.7"])
        .on_upgrade(move |socket| run_terminal_websocket(socket, protocol, device_id))
        .into_response()
}

async fn browser_ws_handler(
    State(protocol): State<SharedDaemonProtocol>,
    Path(browser_id): Path<BrowserSessionId>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    if let Err(response) = authorize_workspace_websocket(&protocol, &headers, "termd.rfb.v1").await
    {
        return response;
    }
    let rfb = match protocol.browser().connect_rfb(browser_id).await {
        Ok(rfb) => rfb,
        Err(error) => return browser_operation_error(error),
    };
    websocket
        .protocols(["termd.rfb.v1"])
        .on_upgrade(move |socket| run_browser_websocket(socket, rfb))
        .into_response()
}

async fn run_browser_websocket(mut socket: WebSocket, mut rfb: tokio::net::UnixStream) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if rfb.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: close_code::UNSUPPORTED,
                        reason: "binary RFB frames are required".into(),
                    }))).await;
                    break;
                }
            },
            read = rfb.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if socket
                        .send(Message::Binary(buffer[..read].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            },
        }
    }
}

async fn run_terminal_websocket(
    mut socket: WebSocket,
    protocol: SharedDaemonProtocol,
    device_id: DeviceId,
) {
    let first = match tokio::time::timeout(Duration::from_secs(30), socket.recv()).await {
        Ok(Some(Ok(Message::Text(raw)))) => raw,
        _ => return,
    };
    let command: Value = match serde_json::from_str(&first) {
        Ok(command) => command,
        Err(_) => return,
    };
    let open = parse_v070_terminal_open(command);
    let Ok(open) = open else {
        let _ = send_v070_socket_error(
            &mut socket,
            "invalid_terminal_open",
            "terminal open command is invalid",
        )
        .await;
        return;
    };
    let mut connection = ProtocolConnection::authenticated_v070_terminal(device_id);
    let opened = {
        let mut guard = protocol.lock().await;
        match guard.open_v070_terminal(&mut connection, open) {
            Ok(opened) => opened,
            Err(error) => {
                drop(guard);
                let _ =
                    send_v070_socket_error(&mut socket, error.code(), error.safe_message()).await;
                return;
            }
        }
    };
    let session_id = opened.snapshot.session_id;
    let send_open = if let Some(created) = opened.created {
        send_v070_json(&mut socket, "terminal.created", created).await
    } else if let Some(attached) = opened.attached {
        send_v070_json(&mut socket, "terminal.attached", attached).await
    } else {
        return;
    };
    if send_open.is_err()
        || send_v070_json(&mut socket, "terminal.snapshot", opened.snapshot)
            .await
            .is_err()
        || flush_v070_terminal_frames(&mut socket, &protocol, &mut connection, session_id)
            .await
            .is_err()
    {
        close_v070_terminal_connection(&protocol, &mut connection).await;
        return;
    }
    let mut output = tokio::time::interval(Duration::from_millis(16));
    output.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    let result = {
                        let mut guard = protocol.lock().await;
                        connection.write_v070_terminal_frame(&mut *guard, session_id, &bytes)
                    };
                    if result.is_err() { break; }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(Message::Text(_))) => {
                    let _ = send_v070_socket_error(&mut socket, "terminal_binary_required", "terminal stream commands must use binary supervisor frames").await;
                }
                _ => {}
            },
            _ = output.tick() => {
                if flush_v070_terminal_frames(&mut socket, &protocol, &mut connection, session_id).await.is_err() {
                    break;
                }
            }
        }
    }
    close_v070_terminal_connection(&protocol, &mut connection).await;
}

async fn flush_v070_terminal_frames(
    socket: &mut WebSocket,
    protocol: &SharedDaemonProtocol,
    connection: &mut ProtocolConnection,
    session_id: SessionId,
) -> Result<(), ()> {
    let frames = {
        let mut guard = protocol.lock().await;
        connection
            .drain_v070_terminal_frames(&mut *guard, session_id)
            .map_err(|_| ())?
    };
    for frame in frames {
        socket.send(Message::Binary(frame)).await.map_err(|_| ())?;
    }
    Ok(())
}

async fn close_v070_terminal_connection(
    protocol: &SharedDaemonProtocol,
    connection: &mut ProtocolConnection,
) {
    let mut guard = protocol.lock().await;
    connection.close(&mut *guard);
}

async fn send_v070_json<T: Serialize>(
    socket: &mut WebSocket,
    kind: &'static str,
    payload: T,
) -> Result<(), axum::Error> {
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({"type": kind, "payload": payload}))
                .map_err(axum::Error::new)?,
        ))
        .await
}

async fn send_v070_socket_error(
    socket: &mut WebSocket,
    code: &'static str,
    message: &'static str,
) -> Result<(), axum::Error> {
    send_v070_json(
        socket,
        "error",
        serde_json::json!({
            "code": code,
            "message": message,
            "retryable": false,
        }),
    )
    .await
}

fn is_api_fallback_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

fn http_control_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route("/api/control/*path", post(http_control_request))
        .route_layer(http_control_api_cors_layer())
}

fn http_file_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route(
            "/api/files/uploads",
            post(v070_file_upload_create).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/uploads/:id/chunks",
            put(v070_file_upload_chunk).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/uploads/:id/commit",
            post(v070_file_upload_commit).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/uploads/:id/abort",
            post(v070_file_upload_abort).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/downloads",
            post(v070_file_download_create).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/downloads/:id",
            get(v070_file_download_read).merge(options(v070_preflight)),
        )
        .route_layer(http_file_api_cors_layer())
        .merge(file_offer_api_router())
}

fn file_offer_api_router() -> Router<SharedDaemonProtocol> {
    Router::new()
        .route(
            "/api/files/offers/:id",
            get(file_offer_resolve).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/offers/:id/downloads",
            post(file_offer_download_create).merge(options(v070_preflight)),
        )
        .route(
            "/api/files/offer-downloads/:id",
            get(file_offer_download_read)
                .head(file_offer_download_head)
                .merge(options(v070_preflight)),
        )
        .route_layer(file_offer_api_cors_layer())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateFileOfferRequest {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyFileOfferDownloadRequest {}

async fn control_file_offer_create(
    State(protocol): State<SharedDaemonProtocol>,
    body: Body,
) -> Response {
    let request: CreateFileOfferRequest = match read_v070_json_body(body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.path.as_os_str().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "offered path must not be empty",
            false,
        );
    }
    let inspected =
        match tokio::task::spawn_blocking(move || inspect_file_offer(request.path)).await {
            Ok(Ok(inspected)) => inspected,
            Ok(Err(error)) => return file_offer_error(error),
            Err(_) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "file_offer_failed",
                    "file offer could not be created",
                    true,
                );
            }
        };
    match protocol
        .lock()
        .await
        .register_file_offer(inspected, current_unix_timestamp_millis())
    {
        Ok(offer) => (StatusCode::CREATED, Json(offer)).into_response(),
        Err(error) => file_offer_error(error),
    }
}

async fn file_offer_resolve(
    State(protocol): State<SharedDaemonProtocol>,
    Path(offer_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_v070_http_device(&protocol, &headers).await {
        return response;
    }
    let offer_id = match uuid::Uuid::parse_str(&offer_id) {
        Ok(offer_id) => offer_id,
        Err(_) => return file_offer_error(FileOfferError::OfferNotFound),
    };
    match protocol
        .lock()
        .await
        .resolve_file_offer(offer_id, current_unix_timestamp_millis())
    {
        Ok(offer) => (StatusCode::OK, Json(offer)).into_response(),
        Err(error) => file_offer_error(error),
    }
}

async fn file_offer_download_create(
    State(protocol): State<SharedDaemonProtocol>,
    Path(offer_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    if read_v070_json_body::<EmptyFileOfferDownloadRequest>(body)
        .await
        .is_err()
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "file offer download request must be an empty JSON object",
            false,
        );
    }
    let offer_id = match uuid::Uuid::parse_str(&offer_id) {
        Ok(offer_id) => offer_id,
        Err(_) => return file_offer_download_error(FileOfferDownloadError::InvalidGrant),
    };
    let prepared = match protocol.lock().await.prepare_file_offer_download(
        device_id,
        offer_id,
        current_unix_timestamp_millis(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => return file_offer_download_error(error),
    };
    let secure = request_uses_https(&headers);
    let cookie = file_offer_download_cookie(
        &prepared.cookie_name,
        &prepared.cookie_secret,
        prepared.ready.download_id,
        secure,
    );
    let mut response = (StatusCode::CREATED, Json(prepared.ready)).into_response();
    if let Ok(cookie) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(SET_COOKIE, cookie);
    } else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_offer_download_failed",
            "file offer download could not be prepared",
            false,
        );
    }
    response
}

async fn file_offer_download_read(
    State(protocol): State<SharedDaemonProtocol>,
    Path(download_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let download_id = match uuid::Uuid::parse_str(&download_id) {
        Ok(download_id) => download_id,
        Err(_) => return file_offer_download_error(FileOfferDownloadError::InvalidGrant),
    };
    let cookie_name = file_offer_download_cookie_name(download_id);
    let Some(cookie_secret) = request_cookie(&headers, &cookie_name) else {
        return with_cleared_file_offer_cookie(
            file_offer_download_error(FileOfferDownloadError::InvalidGrant),
            &cookie_name,
            download_id,
            request_uses_https(&headers),
        );
    };
    let grant = match protocol.lock().await.consume_file_offer_download(
        download_id,
        &cookie_secret,
        current_unix_timestamp_millis(),
    ) {
        Ok(grant) => grant,
        Err(error) => {
            return with_cleared_file_offer_cookie(
                file_offer_download_error(error),
                &cookie_name,
                download_id,
                request_uses_https(&headers),
            );
        }
    };
    let secure = request_uses_https(&headers);
    let size_bytes = grant.payload.size_bytes;
    let snapshot = match copy_then_validate_file_offer(
        &protocol,
        &grant.payload,
        grant.file,
        grant.content_sha256,
        || {},
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(FileOfferSnapshotError::Invalidated) => {
            return with_cleared_file_offer_cookie(
                file_offer_error(FileOfferError::Invalidated),
                &cookie_name,
                download_id,
                secure,
            );
        }
        Err(FileOfferSnapshotError::Io) => {
            return with_cleared_file_offer_cookie(
                file_offer_download_snapshot_error(),
                &cookie_name,
                download_id,
                secure,
            );
        }
    };
    let stream = futures_util::stream::unfold(
        (snapshot, size_bytes),
        |(mut file, mut remaining)| async move {
            if remaining == 0 {
                return None;
            }
            let mut chunk = vec![0_u8; (remaining as usize).min(256 * 1024)];
            match file.read(&mut chunk).await {
                Ok(0) => Some((
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "offered file ended early",
                    )),
                    (file, 0),
                )),
                Ok(read) => {
                    chunk.truncate(read);
                    remaining = remaining.saturating_sub(read as u64);
                    Some((
                        Ok::<Bytes, io::Error>(Bytes::from(chunk)),
                        (file, remaining),
                    ))
                }
                Err(error) => Some((Err(error), (file, 0))),
            }
        },
    );
    let mut response = Body::from_stream(stream).into_response();
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(length) = HeaderValue::from_str(&size_bytes.to_string()) {
        response.headers_mut().insert(CONTENT_LENGTH, length);
    }
    if let Ok(disposition) = HeaderValue::from_str(&content_disposition(&grant.payload.name)) {
        response
            .headers_mut()
            .insert(CONTENT_DISPOSITION, disposition);
    }
    with_cleared_file_offer_cookie(response, &cookie_name, download_id, secure)
}

async fn file_offer_download_head() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOfferSnapshotError {
    Invalidated,
    Io,
}

async fn copy_then_validate_file_offer<F>(
    protocol: &SharedDaemonProtocol,
    payload: &termd_proto::FileOfferPayload,
    file: fs::File,
    expected_sha256: [u8; 32],
    after_first_chunk: F,
) -> Result<tokio::fs::File, FileOfferSnapshotError>
where
    F: FnOnce() + Send,
{
    let validation_file = file.try_clone().map_err(|_| FileOfferSnapshotError::Io)?;
    let snapshot = tempfile::tempfile().map_err(|_| FileOfferSnapshotError::Io)?;
    let mut source = tokio::fs::File::from_std(file);
    let mut snapshot = tokio::fs::File::from_std(snapshot);
    let mut remaining = payload.size_bytes;
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut hasher = Sha256::new();
    let mut after_first_chunk = Some(after_first_chunk);
    while remaining > 0 {
        let limit = (remaining as usize).min(buffer.len());
        let read = source
            .read(&mut buffer[..limit])
            .await
            .map_err(|_| FileOfferSnapshotError::Io)?;
        if read == 0 {
            return Err(FileOfferSnapshotError::Invalidated);
        }
        snapshot
            .write_all(&buffer[..read])
            .await
            .map_err(|_| FileOfferSnapshotError::Io)?;
        hasher.update(&buffer[..read]);
        remaining = remaining.saturating_sub(read as u64);
        if let Some(after_first_chunk) = after_first_chunk.take() {
            after_first_chunk();
        }
    }
    if let Some(after_first_chunk) = after_first_chunk.take() {
        after_first_chunk();
    }
    let mut extra = [0_u8; 1];
    if source
        .read(&mut extra)
        .await
        .map_err(|_| FileOfferSnapshotError::Io)?
        != 0
        || <[u8; 32]>::from(hasher.finalize()) != expected_sha256
    {
        return Err(FileOfferSnapshotError::Invalidated);
    }
    protocol
        .lock()
        .await
        .validate_file_offer_download_snapshot(
            payload,
            &validation_file,
            current_unix_timestamp_millis(),
        )
        .map_err(|_| FileOfferSnapshotError::Invalidated)?;
    snapshot
        .seek(SeekFrom::Start(0))
        .await
        .map_err(|_| FileOfferSnapshotError::Io)?;
    Ok(snapshot)
}

fn file_offer_error(error: FileOfferError) -> Response {
    let status = match error {
        FileOfferError::NotFound | FileOfferError::OfferNotFound => StatusCode::NOT_FOUND,
        FileOfferError::NotRegular => StatusCode::BAD_REQUEST,
        FileOfferError::Unreadable => StatusCode::FORBIDDEN,
        FileOfferError::Invalidated => StatusCode::GONE,
        FileOfferError::DeliveryBusy => StatusCode::TOO_MANY_REQUESTS,
    };
    api_error(
        status,
        error.code(),
        error.safe_message(),
        matches!(error, FileOfferError::DeliveryBusy),
    )
}

fn file_offer_download_error(error: FileOfferDownloadError) -> Response {
    let status = match error {
        FileOfferDownloadError::Offer(error) => {
            return file_offer_error(error);
        }
        FileOfferDownloadError::InvalidGrant => StatusCode::UNAUTHORIZED,
        FileOfferDownloadError::Capacity => StatusCode::TOO_MANY_REQUESTS,
    };
    api_error(status, error.code(), error.safe_message(), false)
}

fn file_offer_download_snapshot_error() -> Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "file_offer_download_failed",
        "file offer download could not be read",
        true,
    )
}

fn request_cookie(headers: &HeaderMap, expected_name: &str) -> Option<String> {
    headers.get_all(COOKIE).iter().find_map(|header| {
        header.to_str().ok()?.split(';').find_map(|cookie| {
            let (name, value) = cookie.trim().split_once('=')?;
            (name == expected_name && !value.is_empty()).then(|| value.to_owned())
        })
    })
}

fn file_offer_download_cookie(
    name: &str,
    value: &str,
    download_id: uuid::Uuid,
    secure: bool,
) -> String {
    format!(
        "{name}={value}; Path=/api/files/offer-downloads/{download_id}; Max-Age=60; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

fn with_cleared_file_offer_cookie(
    mut response: Response,
    name: &str,
    download_id: uuid::Uuid,
    secure: bool,
) -> Response {
    let value = format!(
        "{name}=; Path=/api/files/offer-downloads/{download_id}; Max-Age=0; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

fn request_uses_https(headers: &HeaderMap) -> bool {
    headers
        .get("origin")
        .or_else(|| headers.get("referer"))
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("https://"))
        || headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

fn content_disposition(name: &str) -> String {
    let fallback = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let fallback = if fallback.is_empty() {
        "download".to_owned()
    } else {
        fallback
    };
    let encoded = name
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'-' | b'_') {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

async fn v070_preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn authorize_v070_http_device(
    protocol: &SharedDaemonProtocol,
    headers: &HeaderMap,
) -> Result<DeviceId, Response> {
    let access_token = authorization_credential(headers, "Bearer")?;
    protocol
        .lock()
        .await
        .verify_access_token_credential(access_token, current_unix_timestamp_millis())
        .map_err(|_| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "access_token_invalid",
                "access token is invalid or expired",
                false,
            )
        })
}

async fn push_config(State(protocol): State<SharedDaemonProtocol>, headers: HeaderMap) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let coordinator = protocol.lock().await.push_notifications();
    let application_server_key = match coordinator.application_server_key() {
        Ok(key) => key,
        Err(error) => return push_operation_error(error),
    };
    let subscribed = match coordinator.is_subscribed(device_id) {
        Ok(subscribed) => subscribed,
        Err(error) => return push_operation_error(error),
    };
    (
        StatusCode::OK,
        Json(PushConfigResponse {
            server_id: coordinator.server_id(),
            application_server_key,
            subscribed,
        }),
    )
        .into_response()
}

async fn push_subscription_upsert(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let request = match read_push_subscription_body(body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let subscription = match validated_push_subscription(request) {
        Ok(subscription) => subscription,
        Err(response) => return response,
    };
    let coordinator = protocol.lock().await.push_notifications();
    match coordinator.upsert_subscription(device_id, subscription) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => push_operation_error(error),
    }
}

async fn push_subscription_delete(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let request = match read_push_subscription_delete_body(body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_push_endpoint(&request.endpoint) {
        return response;
    }
    let coordinator = protocol.lock().await.push_notifications();
    match coordinator.remove_subscription_if_endpoint(device_id, &request.endpoint) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => push_operation_error(error),
    }
}

async fn read_push_subscription_body(body: Body) -> Result<PushSubscriptionRequest, Response> {
    let bytes = to_bytes(body, PUSH_SUBSCRIPTION_JSON_MAX_BYTES)
        .await
        .map_err(|_| {
            api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "push_subscription_too_large",
                "Push subscription request is too large",
                false,
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_json_error())
}

async fn read_push_subscription_delete_body(
    body: Body,
) -> Result<PushSubscriptionDeleteRequest, Response> {
    let bytes = to_bytes(body, PUSH_SUBSCRIPTION_JSON_MAX_BYTES)
        .await
        .map_err(|_| {
            api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "push_subscription_too_large",
                "Push subscription request is too large",
                false,
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_json_error())
}

#[allow(clippy::result_large_err)]
fn validated_push_subscription(
    request: PushSubscriptionRequest,
) -> Result<PushSubscription, Response> {
    validate_push_endpoint(&request.endpoint)?;
    if request.keys.p256dh.is_empty()
        || request.keys.p256dh.len() > PUSH_P256DH_MAX_BYTES
        || request.keys.auth.is_empty()
        || request.keys.auth.len() > PUSH_AUTH_MAX_BYTES
    {
        return Err(invalid_push_subscription());
    }
    let p256dh = general_purpose::URL_SAFE_NO_PAD
        .decode(&request.keys.p256dh)
        .map_err(|_| invalid_push_subscription())?;
    let auth = general_purpose::URL_SAFE_NO_PAD
        .decode(&request.keys.auth)
        .map_err(|_| invalid_push_subscription())?;
    if p256dh.len() != 65
        || p256dh.first() != Some(&4)
        || auth.len() != 16
        || web_push_native::p256::PublicKey::from_sec1_bytes(&p256dh).is_err()
    {
        return Err(invalid_push_subscription());
    }
    Ok(PushSubscription {
        endpoint: request.endpoint,
        p256dh: request.keys.p256dh,
        auth: request.keys.auth,
        // Persist the legacy column as `all`; notification modes are no longer user-configurable.
        mode: PushNotificationMode::All,
        locale: request.locale,
        updated_at_ms: current_unix_timestamp_millis().0,
    })
}

#[allow(clippy::result_large_err)]
fn validate_push_endpoint(endpoint: &str) -> Result<(), Response> {
    if endpoint.is_empty() || endpoint.len() > PUSH_ENDPOINT_MAX_BYTES {
        return Err(invalid_push_subscription());
    }
    let endpoint = reqwest::Url::parse(endpoint).map_err(|_| invalid_push_subscription())?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(invalid_push_subscription());
    }
    Ok(())
}

fn invalid_push_subscription() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "push_subscription_invalid",
        "Push subscription is invalid",
        false,
    )
}

fn push_operation_error(error: PushNotificationError) -> Response {
    warn!(%error, "Web Push operation failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "push_unavailable",
        "Web Push is temporarily unavailable",
        true,
    )
}

async fn v070_file_upload_create(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let payload: SessionFileUploadPayload = match read_v070_json_body(body).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let mut guard = protocol.lock().await;
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Err(error) = guard.restore_http_control_scope(&mut connection, payload.session_id) {
        return v070_protocol_error(error);
    }
    let response = guard.prepare_session_file_http_upload(&connection, payload, device_id);
    connection.close(&mut guard);
    match response {
        Ok(ready) => (StatusCode::CREATED, Json(ready)).into_response(),
        Err(error) => v070_protocol_error(error),
    }
}

async fn v070_file_upload_chunk(
    State(protocol): State<SharedDaemonProtocol>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let bytes = match to_bytes(body, V070_FILE_CHUNK_MAX_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "file chunk is too large",
                false,
            );
        }
    };
    let (offset_bytes, size_bytes) = match v070_content_range(&headers, bytes.len()) {
        Ok(range) => range,
        Err(response) => return response,
    };
    let mut guard = protocol.lock().await;
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    let payload =
        match guard.v070_session_file_http_upload_payload(&connection, &upload_id, offset_bytes) {
            Ok(payload) if payload.size_bytes == size_bytes => payload,
            Ok(_) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_content_range",
                    "content range does not match upload size",
                    false,
                );
            }
            Err(error) => return v070_protocol_error(error),
        };
    if let Err(error) = guard.restore_http_control_scope(&mut connection, payload.session_id) {
        return v070_protocol_error(error);
    }
    let response = guard.write_session_file_http_upload(
        &connection,
        payload,
        device_id,
        if bytes.is_empty() {
            Vec::new()
        } else {
            vec![bytes.to_vec()]
        },
    );
    connection.close(&mut guard);
    match response {
        Ok(progress) => (StatusCode::OK, Json(progress)).into_response(),
        Err(error) => v070_protocol_error(error),
    }
}

async fn v070_file_upload_commit(
    State(protocol): State<SharedDaemonProtocol>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let mut guard = protocol.lock().await;
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    let payload = match guard.v070_session_file_http_upload_payload(&connection, &upload_id, 0) {
        Ok(payload) => payload,
        Err(error) => return v070_protocol_error(error),
    };
    if let Err(error) = guard.restore_http_control_scope(&mut connection, payload.session_id) {
        return v070_protocol_error(error);
    }
    if payload.size_bytes == 0
        && let Err(error) = guard.write_session_file_http_upload(
            &connection,
            payload,
            device_id,
            Vec::<Vec<u8>>::new(),
        )
    {
        return v070_protocol_error(error);
    }
    let response = guard.v070_session_file_http_upload_progress(&connection, &upload_id);
    connection.close(&mut guard);
    match response {
        Ok(progress) => (StatusCode::OK, Json(progress)).into_response(),
        Err(error) => v070_protocol_error(error),
    }
}

async fn v070_file_upload_abort(
    State(protocol): State<SharedDaemonProtocol>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let mut guard = protocol.lock().await;
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    let payload = match guard.v070_session_file_http_upload_payload(&connection, &upload_id, 0) {
        Ok(payload) => payload,
        Err(error) => return v070_protocol_error(error),
    };
    if let Err(error) = guard.restore_http_control_scope(&mut connection, payload.session_id) {
        return v070_protocol_error(error);
    }
    let response = guard.v070_abort_session_file_http_upload(&connection, &upload_id);
    connection.close(&mut guard);
    match response {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "upload_id": upload_id, "aborted": true })),
        )
            .into_response(),
        Err(error) => v070_protocol_error(error),
    }
}

async fn v070_file_download_create(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let device_id = match authorize_v070_http_device(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    let payload: SessionFileDownloadPreparePayload = match read_v070_json_body(body).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let mut guard = protocol.lock().await;
    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Err(error) = guard.restore_http_control_scope(&mut connection, payload.session_id) {
        return v070_protocol_error(error);
    }
    let response = guard.prepare_v070_session_file_download(&connection, payload);
    connection.close(&mut guard);
    match response {
        Ok(ready) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "download_id": ready.token,
                "session_id": ready.session_id,
                "path": ready.path,
                "size_bytes": ready.size_bytes,
                "modified_at_ms": ready.modified_at_ms,
                "expires_at_ms": ready.expires_at_ms,
            })),
        )
            .into_response(),
        Err(error) => v070_protocol_error(error),
    }
}

async fn v070_file_download_read(
    State(protocol): State<SharedDaemonProtocol>,
    Path(download_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_v070_http_device(&protocol, &headers).await {
        return response;
    }
    let grant = match protocol
        .lock()
        .await
        .consume_session_file_download(&download_id, current_unix_timestamp_millis())
    {
        Ok(grant) => grant,
        Err(error) => return v070_protocol_error(error),
    };
    let file = match fs::File::open(&grant.path) {
        Ok(file) => file,
        Err(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "file_not_found",
                "file was not found",
                false,
            );
        }
    };
    let stream = futures_util::stream::unfold(
        (file, grant.size_bytes),
        |(mut file, mut remaining)| async move {
            if remaining == 0 {
                return None;
            }
            let mut chunk = vec![0_u8; (remaining as usize).min(256 * 1024)];
            match file.read(&mut chunk) {
                Ok(0) => Some((
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file ended early",
                    )),
                    (file, 0),
                )),
                Ok(read) => {
                    chunk.truncate(read);
                    remaining = remaining.saturating_sub(read as u64);
                    Some((
                        Ok::<Bytes, io::Error>(Bytes::from(chunk)),
                        (file, remaining),
                    ))
                }
                Err(error) => Some((Err(error), (file, 0))),
            }
        },
    );
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/octet-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn read_v070_json_body<T: for<'de> Deserialize<'de>>(body: Body) -> Result<T, Response> {
    let bytes = to_bytes(body, HTTP_JSON_MAX_BYTES).await.map_err(|_| {
        api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "JSON request body is too large",
            false,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "request body is invalid JSON",
            false,
        )
    })
}

#[allow(clippy::result_large_err)]
fn v070_content_range(headers: &HeaderMap, body_len: usize) -> Result<(u64, u64), Response> {
    let value = headers
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "content_range_required",
                "Content-Range is required",
                false,
            )
        })?;
    let value = value.strip_prefix("bytes ").ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    if value == "*/0" && body_len == 0 {
        return Ok((0, 0));
    }
    let (range, total) = value.split_once('/').ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    let (start, end) = range.split_once('-').ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    let start = start.parse::<u64>().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    let end = end.parse::<u64>().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    let total = total.parse::<u64>().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        )
    })?;
    if end < start || end.saturating_sub(start).saturating_add(1) != body_len as u64 || end >= total
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_content_range",
            "Content-Range is invalid",
            false,
        ));
    }
    Ok((start, total))
}

fn v070_protocol_error(error: ProtocolError) -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        error.code(),
        error.safe_message(),
        false,
    )
}

fn http_control_api_cors_layer() -> CorsLayer {
    // v0.7 control plane 只允许 bearer JSON 请求和 relay 路由所需的 server id。
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("content-range"),
            CONTENT_TYPE,
            HeaderName::from_static("authorization"),
            HeaderName::from_static("x-termd-server-id"),
        ])
}

fn http_file_api_cors_layer() -> CorsLayer {
    // 文件上传/下载允许 bearer、JSON、range 和 relay 路由所需的 server id。
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("authorization"),
            HeaderName::from_static("content-range"),
            CONTENT_TYPE,
            HeaderName::from_static("x-termd-server-id"),
        ])
}

fn file_offer_api_cors_layer() -> CorsLayer {
    // File Offer prepare 必须允许同站的跨源开发 UI 接收 HttpOnly grant cookie。
    // 业务授权仍由 Bearer token 完成；原生下载 GET 只消费短期一次性 cookie。
    CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("authorization"),
            CONTENT_TYPE,
            HeaderName::from_static("x-termd-server-id"),
        ])
}

fn push_api_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::PUT, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            HeaderName::from_static("authorization"),
            HeaderName::from_static("x-termd-server-id"),
        ])
}

fn browser_api_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            HeaderName::from_static("authorization"),
            HeaderName::from_static("x-termd-server-id"),
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
/// 该函数只服务网络启动边界，方便集成测试使用随机端口；auth 和 session 语义仍全部
/// 留在 `DaemonProtocol` 中，避免为了测试放宽生产协议。
pub async fn serve_listener(
    listener: TcpListener,
    protocol: SharedDaemonProtocol,
    web_enabled: bool,
) -> Result<(), ServerError> {
    let _push_tasks = start_push_notification_tasks(protocol.clone()).await;
    let _browser_download_task = start_browser_download_monitor(protocol.clone());
    let _status_refresh_task = start_v070_status_refresh(protocol.clone());
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
    let _push_tasks = start_push_notification_tasks(protocol.clone()).await;
    let _browser_download_task = start_browser_download_monitor(protocol.clone());

    // TLS 只替换 transport accept 层；router 和协议状态机保持同一套认证与 session 规则。
    serve_rustls_listener(listener, router(protocol, web_enabled), tls_config).await
}

struct PushNotificationTasks {
    observer: Option<JoinHandle<()>>,
    delivery: Option<JoinHandle<()>>,
}

struct BrowserDownloadMonitorTask(JoinHandle<()>);

impl Drop for BrowserDownloadMonitorTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_browser_download_monitor(protocol: SharedDaemonProtocol) -> BrowserDownloadMonitorTask {
    BrowserDownloadMonitorTask(tokio::spawn(async move {
        loop {
            sleep(BROWSER_DOWNLOAD_SCAN_INTERVAL).await;
            if let Err(error) = offer_completed_browser_downloads(&protocol).await {
                tracing::debug!(code = error.code(), "browser download scan failed");
            }
        }
    }))
}

/// v070 metadata 状态采样间隔。UI 状态面板（CPU/内存/网络曲线）依赖周期性
/// `metadata.update`；token 换发不再重连 metadata 后，不能靠连接重建刷新状态，
/// 由 daemon 定期采样并通知所有订阅连接。
const V070_STATUS_REFRESH_INTERVAL_SECS: u64 = 10;

fn start_v070_status_refresh(protocol: SharedDaemonProtocol) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(V070_STATUS_REFRESH_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if protocol.lock().await.v070_metadata_has_subscribers() {
                protocol.lock().await.notify_v070_metadata_changed();
            }
        }
    })
}

async fn offer_completed_browser_downloads(
    protocol: &SharedDaemonProtocol,
) -> Result<usize, BrowserError> {
    let candidates = protocol.browser().completed_downloads().await?;
    let mut offered = 0;
    for candidate in candidates {
        if !protocol.lock().await.file_offer_delivery_available() {
            break;
        }
        let inspected = match protocol.browser().inspect_download(&candidate).await {
            Ok(inspected) => inspected,
            Err(error) => {
                tracing::debug!(
                    code = error.code(),
                    "completed browser download was rejected"
                );
                continue;
            }
        };
        match protocol
            .lock()
            .await
            .register_file_offer_for_delivery(inspected, current_unix_timestamp_millis())
        {
            Ok(_) => {
                protocol.browser().mark_download_handled(candidate).await;
                offered += 1;
            }
            Err(FileOfferError::DeliveryBusy) => break,
            Err(error) => {
                tracing::debug!(
                    code = error.code(),
                    "completed browser download could not be offered"
                );
            }
        }
    }
    Ok(offered)
}

impl Drop for PushNotificationTasks {
    fn drop(&mut self) {
        if let Some(observer) = &self.observer {
            observer.abort();
        }
        if let Some(delivery) = &self.delivery {
            delivery.abort();
        }
    }
}

async fn start_push_notification_tasks(protocol: SharedDaemonProtocol) -> PushNotificationTasks {
    let (coordinator, mut activity_events) = {
        let mut guard = protocol.lock().await;
        (
            guard.push_notifications(),
            guard.take_push_activity_events(),
        )
    };
    if !coordinator.is_available() {
        return PushNotificationTasks {
            observer: None,
            delivery: None,
        };
    }
    if let Some(events) = &mut activity_events {
        let pending_event_count = events.len();
        for _ in 0..pending_event_count {
            if events.try_recv().is_err() {
                break;
            }
        }
    }
    let initial = protocol.lock().await.push_activity_snapshot();
    coordinator.initialize_activity_snapshot(initial);
    let delivery = coordinator.start_delivery_worker();
    let observer = activity_events.map(|events| {
        tokio::spawn(run_push_notification_observer(
            protocol,
            coordinator,
            events,
        ))
    });
    PushNotificationTasks { observer, delivery }
}

async fn run_push_notification_observer(
    protocol: SharedDaemonProtocol,
    coordinator: PushNotificationCoordinator,
    mut activity_events: tokio::sync::mpsc::Receiver<crate::pty::SessionActivityEvent>,
) {
    while let Some(event) = activity_events.recv().await {
        let change = protocol.lock().await.push_activity_event_snapshot(event);
        if let Some(change) = change {
            coordinator.observe_activity_change(change);
        }
    }
}

pub(crate) async fn handle_http_tunnel_stream_request(
    protocol: SharedDaemonProtocol,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Body,
) -> Response {
    if !is_http_tunnel_allowed(&method, &path) {
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

fn is_http_tunnel_allowed(method: &str, path: &str) -> bool {
    // 中文注释：trusted relay 负责 admission/routing，daemon 仍最终校验 tunnel 路径和
    // 后续 auth/session 权限。路由白名单来自 proto 共享函数，避免两侧字符串漂移。
    is_http_tunnel_path_allowed(method, path)
}

fn load_rustls_server_config(tls_paths: &TlsPaths) -> Result<rustls::ServerConfig, ServerError> {
    // 中文注释：库测试和嵌入式调用不会经过 `termd` binary 的 main；
    // TLS server config 自己也要选定 provider，避免 aws-lc/ring 同时存在时 panic。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
        daemon_public_key: protocol.daemon_public_identity().public_key.clone(),
    })
}

fn pairing_ws_url_from_config(config: &DaemonConfig, server_id: ServerId) -> String {
    config
        .default_pairing_ws_url
        .trim()
        .replace("{server_id}", &server_id.0.to_string())
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
    let protocol = protocol.lock().await;
    let ttl_ms = protocol.config().pairing_token_ttl_ms;
    let server_id = protocol.server_id();
    let daemon_public_key = protocol.daemon_public_identity().public_key.clone();
    let ws_url = pairing_ws_url_from_config(protocol.config(), server_id);
    let (token, expires_at_ms) = match protocol.issue_pair_ticket_credential(now_ms) {
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
    drop(protocol);

    (
        StatusCode::OK,
        Json(LocalPairingTokenPayload {
            token,
            expires_at_ms,
            ttl_ms,
            server_id,
            daemon_public_key,
            ws_url,
        }),
    )
        .into_response()
}

async fn http_control_request(
    State(protocol): State<SharedDaemonProtocol>,
    Path(path): Path<String>,
    _http_method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if is_removed_v070_http_control_path(uri.path()) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "application route was not found",
            false,
        );
    }
    if !is_http_control_tunnel_path_allowed(uri.path()) {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "application route was not found",
            false,
        );
    }
    let (method, session_scope_session_id) = match parse_http_control_path(&path) {
        Ok(parsed) => parsed,
        Err(error) => return v070_protocol_error(error),
    };
    handle_v070_json_control_request(protocol, method, session_scope_session_id, headers, body)
        .await
}

/// 只读版本端点：前端用它识别「当前连接的服务组件」。
/// 直连 daemon 返回 `termd`；经 relay 时同一路径由 relay 返回 `termrelay`。
async fn version_endpoint() -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "component": "termd",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

/// 更新端点共用的设备认证：Bearer access token 必须属于已配对设备。
async fn authenticated_device(
    protocol: &SharedDaemonProtocol,
    headers: &HeaderMap,
) -> Result<DeviceId, Response> {
    let access_token = authorization_credential(headers, "Bearer")?;
    protocol
        .lock()
        .await
        .verify_access_token_credential(access_token, current_unix_timestamp_millis())
        .map_err(|_| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "access_token_invalid",
                "access token is invalid or expired",
                false,
            )
        })
}

async fn update_check(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticated_device(&protocol, &headers).await {
        return response;
    }
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let arch = termupdater::host_arch().to_owned();
    let check_current = current.clone();
    let result = tokio::task::spawn_blocking(move || {
        termupdater::check_update("termd", &arch, &check_current)
    })
    .await;
    match result {
        Ok(Ok(info)) => Json(serde_json::json!({
            "current": current,
            "update_available": true,
            "latest": info.latest,
            "release_url": info.release_url,
        }))
        .into_response(),
        Ok(Err(termupdater::UpdateError::NoNewerRelease { .. })) => Json(serde_json::json!({
            "current": current,
            "update_available": false,
        }))
        .into_response(),
        Ok(Err(_)) | Err(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "update_check_failed",
            "failed to query the latest release",
            true,
        ),
    }
}

/// 一键更新 termd：校验设备凭证 → 确认存在新版本 → 后台下载/校验/备份/
/// 原子替换/重启服务。响应先返回（含 `restart_pending`），服务随后重启。
async fn update_apply(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticated_device(&protocol, &headers).await {
        return response;
    }
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let arch = termupdater::host_arch().to_owned();
    let check_current = current.clone();
    let info = match tokio::task::spawn_blocking(move || {
        termupdater::check_update("termd", &arch, &check_current)
    })
    .await
    {
        Ok(Ok(info)) => info,
        Ok(Err(termupdater::UpdateError::NoNewerRelease { .. })) => {
            return Json(serde_json::json!({
                "current": current,
                "update_available": false,
                "applied": false,
            }))
            .into_response();
        }
        Ok(Err(_)) | Err(_) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "update_check_failed",
                "failed to query the latest release",
                true,
            );
        }
    };
    let Ok(binary_path) = std::env::current_exe() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "update_unavailable",
            "cannot resolve the daemon binary path",
            false,
        );
    };
    let service_name = std::env::var("TERMD_SERVICE_NAME").unwrap_or_else(|_| "termd".to_owned());
    let asset_url = info.asset_url.clone();
    let expected = info.latest.clone();
    // 后台执行下载/替换；延迟重启让上面的响应先送达浏览器。
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = tokio::task::spawn_blocking(move || {
            termupdater::apply_update(termupdater::ApplyRequest {
                binary_path,
                service_name,
                expected_version: expected,
                asset_url,
            })
        })
        .await;
    });
    Json(serde_json::json!({
        "current": current,
        "update_available": true,
        "latest": info.latest,
        "release_url": info.release_url,
        "applied": true,
        "restart_pending": true,
    }))
    .into_response()
}

/// 一键更新 relay：daemon 用其 relay admission token 委托给 relay 的受限
/// `/update` 端点（relay 校验 `TermdDaemon` 凭证后自行下载/替换/重启）。
async fn update_relay(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticated_device(&protocol, &headers).await {
        return response;
    }
    let (endpoint, token, server_id) = {
        let guard = protocol.lock().await;
        let config = guard.config();
        let Some(endpoint) = config.relay_endpoints.first() else {
            return api_error(
                StatusCode::BAD_REQUEST,
                "relay_not_configured",
                "daemon has no relay endpoint configured",
                false,
            );
        };
        let Some(token) = config.relay_daemon_token.as_ref() else {
            return api_error(
                StatusCode::BAD_REQUEST,
                "relay_not_configured",
                "daemon has no relay admission token configured",
                false,
            );
        };
        (
            endpoint.clone(),
            token.expose_secret().to_owned(),
            guard.server_id().0.to_string(),
        )
    };
    let Some(base) = relay_http_base(&endpoint) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "relay_not_configured",
            "relay endpoint must be ws(s)://host[:port]",
            false,
        );
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build();
    let Ok(client) = client else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "update_unavailable",
            "failed to build HTTP client",
            false,
        );
    };
    let response = client
        .post(format!("{base}/update"))
        .header("x-termd-server-id", &server_id)
        .header("authorization", format!("TermdDaemon {token}"))
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            (
                status,
                Json(serde_json::json!({
                    "relay_update_requested": status.is_success(),
                    "relay_response": body,
                })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "relay update request failed");
            api_error(
                StatusCode::BAD_GATEWAY,
                "relay_update_failed",
                "failed to reach the relay update endpoint",
                true,
            )
        }
    }
}

/// 把配置的 relay 端点（`wss://host` 或 `ws://host`）转成 HTTP base URL。
fn relay_http_base(endpoint: &str) -> Option<String> {
    if let Some(rest) = endpoint.strip_prefix("wss://") {
        Some(format!("https://{}", rest.trim_end_matches("/ws")))
    } else if let Some(rest) = endpoint.strip_prefix("ws://") {
        Some(format!("http://{}", rest.trim_end_matches("/ws")))
    } else {
        None
    }
}

async fn handle_v070_json_control_request(
    protocol: SharedDaemonProtocol,
    method: String,
    session_id: Option<SessionId>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let access_token = match authorization_credential(&headers, "Bearer") {
        Ok(token) => token,
        Err(response) => return response,
    };
    let body = match to_bytes(body, HTTP_JSON_MAX_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "JSON request body is too large",
                false,
            );
        }
    };
    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(payload @ Value::Object(_)) => payload,
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body must be a JSON object",
                false,
            );
        }
    };

    let mut protocol_guard = protocol.lock().await;
    let device_id = match protocol_guard
        .verify_access_token_credential(access_token, current_unix_timestamp_millis())
    {
        Ok(device_id) => device_id,
        Err(_) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "access_token_invalid",
                "access token is invalid or expired",
                false,
            );
        }
    };
    if let Some(session_id) = session_id {
        payload
            .as_object_mut()
            .expect("validated JSON object")
            .insert("session_id".to_owned(), serde_json::json!(session_id));
    }

    let mut connection = ProtocolConnection::authenticated_http(device_id);
    if let Some(session_id) = session_id
        && let Err(error) = protocol_guard.restore_http_control_scope(&mut connection, session_id)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            error.code(),
            error.safe_message(),
            false,
        );
    }
    let response = connection.dispatch_v070_http_control(&mut protocol_guard, &method, payload);
    connection.close(&mut protocol_guard);
    match response {
        Ok(payload) => (StatusCode::OK, Json(payload)).into_response(),
        Err(error) => api_error(
            StatusCode::BAD_REQUEST,
            error.code(),
            error.safe_message(),
            false,
        ),
    }
}

fn is_removed_v070_http_control_path(path: &str) -> bool {
    path == "/api/control/session/list"
        || path == "/api/control/daemon/clients"
        || path == "/api/control/daemon/status"
        || path.ends_with("/attach")
        || path.ends_with("/cursor")
        || path.ends_with("/resize")
        || path.ends_with("/file_download_prepare")
        || path.ends_with("/file_download_chunk")
}

fn parse_http_control_path(path: &str) -> Result<(String, Option<SessionId>), ProtocolError> {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Err(ProtocolError::InvalidEnvelope);
    }
    if segments.len() >= 3
        && segments[0] == "session"
        && let Ok(session_uuid) = segments[1].parse()
    {
        let action = segments[2..].join(".");
        let method = if action == "control" {
            termd_proto::METHOD_CONTROL_REQUEST.to_owned()
        } else {
            format!("session.{action}")
        };
        return Ok((method, Some(SessionId(session_uuid))));
    }
    Ok((segments.join("."), None))
}

fn is_loopback_peer(peer_addr: SocketAddr) -> bool {
    peer_addr.ip().is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    use serde::Deserialize;
    use std::fs;
    use std::io::{Read, Write};
    use std::ops::{Deref, DerefMut};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use termd_proto::{
        DeviceId, METHOD_SESSION_FILES, Nonce, PublicKey, SessionCreateInSessionCwdPayload,
        SessionCreatePayload, SessionFilesPayload, SessionFilesResultPayload, Signature,
        TerminalSize, UnixTimestampMillis,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::Duration;
    use tokio_tungstenite::tungstenite::Message as ClientWsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use crate::auth::{
        AccessTokenProofInput, CredentialKind, current_unix_timestamp_millis, verify_credential,
    };
    use crate::net::protocol::ProtocolConnection;
    use crate::runtime::SessionRuntime;
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

    static TEST_CONFIG_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestStateDir {
        state_dir: PathBuf,
        state_path: PathBuf,
    }

    impl Drop for TestStateDir {
        fn drop(&mut self) {
            match StateStore::load(&self.state_path) {
                Ok(state) => {
                    let backend = SupervisorPtyBackend::for_state_path(&self.state_path);
                    let mut runtime = SessionRuntime::new(backend);
                    for session in state
                        .sessions
                        .iter()
                        .filter(|session| session.state == SessionState::Running)
                    {
                        let session_id = session.session_id.0.to_string();
                        if let Err(error) = runtime
                            .reconnect_session(session)
                            .and_then(|()| runtime.close(&session_id))
                        {
                            eprintln!(
                                "failed to clean up server test session {session_id} in {}: {error}",
                                self.state_dir.display()
                            );
                        }
                    }
                }
                Err(error) => eprintln!(
                    "failed to load server test state {} during cleanup: {error}",
                    self.state_path.display()
                ),
            }

            if let Err(error) = fs::remove_dir_all(&self.state_dir) {
                eprintln!(
                    "failed to remove server test state directory {}: {error}",
                    self.state_dir.display()
                );
            }
        }
    }

    struct TestConfigFixture {
        config: DaemonConfig,
        state_dir: TestStateDir,
    }

    impl TestConfigFixture {
        fn into_protocol(self) -> TestProtocolFixture {
            TestProtocolFixture {
                protocol: default_protocol(self.config),
                _state_dir: self.state_dir,
            }
        }
    }

    impl Deref for TestConfigFixture {
        type Target = DaemonConfig;

        fn deref(&self) -> &Self::Target {
            &self.config
        }
    }

    impl DerefMut for TestConfigFixture {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.config
        }
    }

    // Fields drop in declaration order: release the protocol before reconnecting for cleanup.
    struct TestProtocolFixture {
        protocol: SharedDaemonProtocol,
        _state_dir: TestStateDir,
    }

    impl Deref for TestProtocolFixture {
        type Target = SharedDaemonProtocol;

        fn deref(&self) -> &Self::Target {
            &self.protocol
        }
    }

    fn test_config(name: &str) -> TestConfigFixture {
        let unique = TEST_CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let state_dir = std::env::temp_dir().join(format!(
            "termd-server-test-{}-{}-{unique}-{name}",
            std::process::id(),
            current_unix_timestamp_millis().0
        ));
        // 中文注释：server 单测仍使用独立目录，避免并发测试或遗留 supervisor socket
        // 影响同一组 daemon 状态。
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("daemon-state.json");
        TestConfigFixture {
            config: DaemonConfig::default_for_state_path(&state_path),
            state_dir: TestStateDir {
                state_dir,
                state_path,
            },
        }
    }

    fn test_protocol(name: &str) -> TestProtocolFixture {
        test_config(name).into_protocol()
    }

    fn wait_for_session_path(
        protocol: &mut DefaultDaemonProtocol,
        connection: &mut ProtocolConnection,
        session_id: SessionId,
        expected: &Path,
    ) {
        for _ in 0..100 {
            let payload = serde_json::to_value(SessionFilesPayload {
                session_id,
                path: None,
            })
            .unwrap();
            if let Ok(result) =
                connection.dispatch_v070_http_control(protocol, METHOD_SESSION_FILES, payload)
            {
                let result: SessionFilesResultPayload = serde_json::from_value(result).unwrap();
                if Path::new(&result.path) == expected {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "session {session_id:?} did not reach expected cwd {}",
            expected.display()
        );
    }

    fn session_storage_counts(state_path: &Path) -> (i64, i64) {
        let connection =
            rusqlite::Connection::open(crate::state::sqlite_state_path_for_state_path(state_path))
                .unwrap();
        let history = connection
            .query_row("SELECT COUNT(*) FROM daemon_sessions", [], |row| row.get(0))
            .unwrap();
        let ownership = connection
            .query_row("SELECT COUNT(*) FROM session_ownership", [], |row| {
                row.get(0)
            })
            .unwrap();
        (history, ownership)
    }

    fn persisted_session_root(state_path: &Path, session_id: SessionId) -> PathBuf {
        let connection =
            rusqlite::Connection::open(crate::state::sqlite_state_path_for_state_path(state_path))
                .unwrap();
        connection
            .query_row(
                "SELECT root_path FROM daemon_sessions WHERE session_id = ?1",
                [session_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(PathBuf::from)
            .unwrap()
    }

    #[test]
    fn v070_terminal_open_parses_legacy_and_cwd_create_commands() {
        let source_session_id = SessionId::new();
        let size = TerminalSize::new(24, 80);

        let legacy = parse_v070_terminal_open(serde_json::json!({
            "type": "terminal.create",
            "payload": { "command": ["sh"], "size": size },
        }))
        .unwrap();
        assert!(matches!(
            legacy,
            V070TerminalOpen::Create(SessionCreatePayload { command, size: parsed_size })
                if command == ["sh"] && parsed_size == size
        ));

        let create_in_cwd = parse_v070_terminal_open(serde_json::json!({
            "type": "terminal.create_in_session_cwd",
            "payload": {
                "source_session_id": source_session_id,
                "command": ["sh"],
                "size": size,
            },
        }))
        .unwrap();
        assert!(matches!(
            create_in_cwd,
            V070TerminalOpen::CreateInSessionCwd(SessionCreateInSessionCwdPayload {
                source_session_id: parsed_source,
                command,
                size: parsed_size,
            }) if parsed_source == source_session_id && command == ["sh"] && parsed_size == size
        ));

        assert!(
            parse_v070_terminal_open(serde_json::json!({
                "type": "terminal.create_somewhere",
                "payload": { "command": ["sh"], "size": size },
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_push_identity_disables_notifications_without_blocking_daemon_startup() {
        let fixture = test_config("invalid-push-identity");
        let first = try_default_protocol(fixture.config.clone()).unwrap();
        drop(first);

        let sqlite_path = crate::state::sqlite_state_path_for_state_path(&fixture.state_path);
        let connection = rusqlite::Connection::open(sqlite_path).unwrap();
        connection
            .execute(
                "UPDATE web_push_identity SET private_key = 'invalid' WHERE singleton = 1",
                [],
            )
            .unwrap();
        drop(connection);

        let restarted = try_default_protocol(fixture.config.clone()).unwrap();
        assert!(!restarted.lock().await.push_notifications().is_available());

        let (_, access_token) = v070_access_token_for_test(&restarted).await;
        let authorization = format!("Bearer {access_token}");
        let app = router(restarted, false);
        for (method, path, body) in [
            (Method::GET, "/api/push/config", Body::empty()),
            (
                Method::PUT,
                "/api/push/subscription",
                Body::from(
                    serde_json::json!({
                        "endpoint": "https://push.example.test/device",
                        "keys": {
                            "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                            "auth": "BTBZMqHH6r4Tts7J_aSIgg"
                        },
                        "mode": "all",
                        "locale": "en-US"
                    })
                    .to_string(),
                ),
            ),
            (
                Method::DELETE,
                "/api/push/subscription",
                Body::from(
                    serde_json::json!({
                        "endpoint": "https://push.example.test/device"
                    })
                    .to_string(),
                ),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", &authorization)
                        .header(CONTENT_TYPE, "application/json")
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path}");
            let body: serde_json::Value = serde_json::from_slice(
                &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            )
            .unwrap();
            assert_eq!(body["error"]["code"], "push_unavailable", "{path}");
        }
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
    fn startup_remains_available_with_quarantined_http_upload_record() {
        let fixture = test_config("startup-http-upload-quarantine");
        let state_path = fixture.state_dir.state_path.clone();
        StateStore::record_http_upload(
            &state_path,
            &crate::state::HttpUploadRecoveryRecord {
                upload_id: "startup-missing-upload".to_owned(),
                target_path: fixture.state_dir.state_dir.join("missing-upload.part"),
                size_bytes: 4,
                dev: 1,
                ino: 1,
                updated_at_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();

        let protocol = fixture.into_protocol();

        let protocol = protocol
            .protocol
            .try_lock()
            .expect("startup must return a usable protocol");
        assert_eq!(
            protocol.server_id(),
            protocol.daemon_public_identity().server_id
        );
    }

    #[test]
    fn router_exposes_healthz_and_ws_routes() {
        let protocol = test_protocol("router");
        let _router = router(protocol.clone(), false);
    }

    #[tokio::test]
    async fn v070_router_exposes_dual_workspace_websockets() {
        let app = router(
            test_protocol("dual-workspace-websockets").protocol.clone(),
            false,
        );
        for path in ["/ws/metadata", "/ws/terminal"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn v070_router_does_not_expose_legacy_runtime_websocket() {
        let response = router(
            test_protocol("legacy-runtime-ws-removed").protocol.clone(),
            false,
        )
        .oneshot(Request::builder().uri("/ws").body(Body::empty()).unwrap())
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn v070_application_http_failures_are_json() {
        let app = router(test_protocol("json-errors").protocol.clone(), true);
        for request in [
            Request::builder()
                .uri("/api/unknown")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/auth/challenge")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/auth/challenge")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert!(response.status().is_client_error());
            assert_eq!(
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/json")
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(value["error"]["code"].is_string(), "{value}");
            assert!(value["error"]["message"].is_string(), "{value}");
            assert!(value["error"]["retryable"].is_boolean(), "{value}");
        }
    }

    #[tokio::test]
    async fn v070_file_transfer_routes_replace_legacy_http_e2ee_paths() {
        let app = router(test_protocol("v070-file-routes").protocol.clone(), false);
        for (method, path) in [
            (Method::POST, "/api/files/uploads"),
            (Method::PUT, "/api/files/uploads/upload-id/chunks"),
            (Method::POST, "/api/files/uploads/upload-id/commit"),
            (Method::POST, "/api/files/uploads/upload-id/abort"),
            (Method::POST, "/api/files/downloads"),
            (Method::GET, "/api/files/downloads/download-id"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "authorization_required", "{path}");
        }
        for path in [
            "/api/files/upload/init",
            "/api/files/upload",
            "/api/files/upload/abort",
            "/api/files/download",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "not_found", "{path}");
        }

        let local_only = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/file-offers")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"path":"/tmp/report.zip"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local_only.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn completed_browser_download_is_stably_scanned_and_broadcast_once() {
        let fixture = test_protocol("browser-download-offer");
        let browser_id = BrowserSessionId::new();
        let download_dir = fixture
            ._state_dir
            .state_dir
            .join("browser-workspaces/downloads")
            .join(browser_id.to_string());
        fs::create_dir(&download_dir).unwrap();
        let file_path = download_dir.join("report.zip");
        fs::write(&file_path, b"browser bytes").unwrap();

        assert_eq!(
            offer_completed_browser_downloads(&fixture.protocol).await,
            Ok(0)
        );
        assert_eq!(
            offer_completed_browser_downloads(&fixture.protocol).await,
            Ok(0),
            "a completed download must wait until a client can receive the one-shot offer"
        );
        let mut events = fixture.protocol.lock().await.file_offer_events();
        assert_eq!(
            offer_completed_browser_downloads(&fixture.protocol).await,
            Ok(1)
        );
        let offer = events.recv().await.unwrap();
        assert_eq!(offer.name, "report.zip");
        assert_eq!(offer.path, file_path.to_string_lossy());
        assert_eq!(offer.size_bytes, 13);
        assert_eq!(
            offer_completed_browser_downloads(&fixture.protocol).await,
            Ok(0)
        );
    }

    #[tokio::test]
    async fn automatic_file_offer_requires_a_live_sink_when_it_is_registered() {
        let fixture = test_protocol("browser-download-live-sink");
        let file_path = fixture._state_dir.state_dir.join("report.zip");
        fs::write(&file_path, b"browser bytes").unwrap();
        let inspected = inspect_file_offer(&file_path).unwrap();
        let dropped_events = fixture.protocol.lock().await.file_offer_events();
        drop(dropped_events);

        let error = fixture
            .protocol
            .lock()
            .await
            .register_file_offer_for_delivery(inspected.clone(), current_unix_timestamp_millis())
            .unwrap_err();
        assert_eq!(error, FileOfferError::DeliveryBusy);

        let mut events = fixture.protocol.lock().await.file_offer_events();
        let offer = fixture
            .protocol
            .lock()
            .await
            .register_file_offer_for_delivery(inspected, current_unix_timestamp_millis())
            .unwrap();
        assert_eq!(events.recv().await.unwrap(), offer);
    }

    #[tokio::test]
    async fn file_offer_is_one_shot_metadata_and_cookie_authorized_native_download() {
        let fixture = test_protocol("file-offer-flow");
        let file_path = fixture._state_dir.state_dir.join("report.zip");
        fs::write(&file_path, b"offered bytes").unwrap();
        let mut first_events = fixture.protocol.lock().await.file_offer_events();
        let mut second_events = fixture.protocol.lock().await.file_offer_events();
        let control = daemon_control_router(fixture.protocol.clone());
        let created = control
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/file-offers")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"path": file_path}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let offer: termd_proto::FileOfferPayload =
            serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(offer.name, "report.zip");
        assert_eq!(offer.size_bytes, 13);
        assert_eq!(first_events.recv().await.unwrap(), offer);
        assert_eq!(second_events.recv().await.unwrap(), offer);
        let mut late_events = fixture.protocol.lock().await.file_offer_events();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), late_events.recv())
                .await
                .is_err(),
            "File Offers must not replay to later metadata subscribers"
        );

        let (device_id, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let authorization = format!("Bearer {access_token}");
        let app = router(fixture.protocol.clone(), false);
        let resolved = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/files/offers/{}", offer.offer_id))
                    .header("authorization", &authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resolved.status(), StatusCode::OK);

        let prepared = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/files/offers/{}/downloads", offer.offer_id))
                    .header("authorization", &authorization)
                    .header("origin", "https://relay.example.test")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prepared.status(), StatusCode::CREATED);
        let set_cookie = prepared
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(set_cookie.contains("Secure"));
        assert_eq!(
            prepared
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://relay.example.test")
        );
        assert_eq!(
            prepared
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let ready: termd_proto::FileOfferDownloadReadyPayload =
            serde_json::from_slice(&to_bytes(prepared.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(ready.download_url.contains(&format!(
            "server_id={}",
            fixture.protocol.lock().await.server_id().0
        )));

        let wrong_cookie = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&ready.download_url)
                    .header(
                        COOKIE,
                        format!("{}=wrong", cookie.split('=').next().unwrap()),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_cookie.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            wrong_cookie.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );

        let head = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri(&ready.download_url)
                    .header(COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );

        let downloaded = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&ready.download_url)
                    .header(COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(
            downloaded.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        assert_eq!(downloaded.headers().get(CONTENT_LENGTH).unwrap(), "13");
        assert!(
            downloaded
                .headers()
                .get(CONTENT_DISPOSITION)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("report.zip")
        );
        assert_eq!(
            &to_bytes(downloaded.into_body(), usize::MAX).await.unwrap()[..],
            b"offered bytes"
        );

        let repeated = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&ready.download_url)
                    .header(COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(device_id, DeviceId(uuid::Uuid::nil()));
    }

    #[tokio::test]
    async fn file_offer_snapshot_rejects_source_changes_before_final_validation() {
        let fixture = test_protocol("file-offer-snapshot-change");
        let file_path = fixture._state_dir.state_dir.join("report.zip");
        let original = vec![b'a'; 512 * 1024];
        let replacement = vec![b'b'; original.len()];
        fs::write(&file_path, &original).unwrap();
        let now = current_unix_timestamp_millis();
        let grant = {
            let mut protocol = fixture.protocol.lock().await;
            let offer = protocol.create_file_offer(&file_path, now).unwrap();
            let device_id = DeviceId(uuid::Uuid::new_v4());
            let prepared = protocol
                .prepare_file_offer_download(device_id, offer.offer_id, now)
                .unwrap();
            protocol
                .consume_file_offer_download(
                    prepared.ready.download_id,
                    &prepared.cookie_secret,
                    now,
                )
                .unwrap()
        };
        let payload = grant.payload;
        let source = grant.file;
        let content_sha256 = grant.content_sha256;
        let changed_path = file_path.clone();
        let result = copy_then_validate_file_offer(
            &fixture.protocol,
            &payload,
            source,
            content_sha256,
            move || {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .open(changed_path)
                    .unwrap();
                file.write_all(&replacement).unwrap();
            },
        )
        .await;

        assert!(matches!(result, Err(FileOfferSnapshotError::Invalidated)));
        assert_eq!(fs::read(file_path).unwrap(), vec![b'b'; original.len()]);
    }

    #[tokio::test]
    async fn file_offer_creation_backpressures_before_a_connected_client_can_lose_an_event() {
        let fixture = test_protocol("file-offer-backpressure");
        let file_path = fixture._state_dir.state_dir.join("report.zip");
        fs::write(&file_path, b"offered bytes").unwrap();
        let stalled_events = {
            let mut protocol = fixture.protocol.lock().await;
            let events = protocol.file_offer_events();
            for offset in 0..FILE_OFFER_LIMIT {
                protocol
                    .create_file_offer(&file_path, UnixTimestampMillis(1_000 + offset as u64))
                    .unwrap();
            }
            events
        };

        let control = daemon_control_router(fixture.protocol.clone());
        let blocked = control
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/file-offers")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"path": file_path}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(blocked.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "file_offer_delivery_busy");
        assert_eq!(body["error"]["retryable"], true);

        drop(stalled_events);
        let retried = control
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/file-offers")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"path": file_path}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retried.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn v070_file_transfer_cors_allows_chunk_upload_and_download() {
        let app = router(test_protocol("v070-file-cors").protocol.clone(), false);
        for (method, path, headers) in [
            (
                "PUT",
                "/api/files/uploads/upload-id/chunks",
                "authorization,content-range,x-termd-server-id",
            ),
            (
                "GET",
                "/api/files/downloads/download-id",
                "authorization,x-termd-server-id",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::OPTIONS)
                        .uri(path)
                        .header("origin", "http://127.0.0.1:4173")
                        .header("access-control-request-method", method)
                        .header("access-control-request-headers", headers)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "{method} {path}: {}",
                response.status(),
            );
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|value| value.to_str().ok()),
                Some("*"),
                "{method} {path}",
            );
        }
    }

    #[tokio::test]
    async fn file_offer_cors_mirrors_origin_and_allows_credentials() {
        let app = router(test_protocol("file-offer-cors").protocol.clone(), false);
        let origin = "http://127.0.0.1:4173";
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/files/offers/00000000-0000-4000-8000-000000000601/downloads")
                    .header("origin", origin)
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type,x-termd-server-id",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some(origin)
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn push_routes_require_bearer_and_keep_subscriptions_device_owned() {
        let fixture = test_protocol("push-http");
        let app = router(fixture.protocol.clone(), false);
        for (method, path) in [
            (Method::GET, "/api/push/config"),
            (Method::PUT, "/api/push/subscription"),
            (Method::DELETE, "/api/push/subscription"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }

        let (_device_a, token_a) = v070_access_token_for_test(&fixture.protocol).await;
        let (_device_b, token_b) = v070_access_token_for_test(&fixture.protocol).await;
        let authorization_a = format!("Bearer {token_a}");
        let authorization_b = format!("Bearer {token_b}");

        let config = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/push/config")
                    .header("authorization", &authorization_a)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(config.status(), StatusCode::OK);
        let config: serde_json::Value =
            serde_json::from_slice(&to_bytes(config.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            config["server_id"],
            fixture.protocol.lock().await.server_id().0.to_string()
        );
        assert_eq!(config["subscribed"], false);
        let application_server_key = config["application_server_key"].as_str().unwrap();
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(application_server_key)
                .unwrap()
                .len(),
            65
        );

        let subscription_body = serde_json::json!({
            "endpoint": "https://push.example.test/device-a",
            "keys": {
                "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "auth": "BTBZMqHH6r4Tts7J_aSIgg"
            },
            "mode": "all",
            "locale": "zh-CN"
        });
        let subscribed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/push/subscription")
                    .header("authorization", &authorization_a)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(subscription_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(subscribed.status(), StatusCode::NO_CONTENT);

        for (authorization, expected) in [(&authorization_a, true), (&authorization_b, false)] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/api/push/config")
                        .header("authorization", authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["subscribed"], expected);
        }

        let delete_other_device = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/push/subscription")
                    .header("authorization", &authorization_b)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "endpoint": "https://push.example.test/device-a"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete_other_device.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/push/config")
                    .header("authorization", &authorization_a)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["subscribed"], true);
    }

    #[tokio::test]
    async fn push_subscription_rejects_invalid_and_oversized_input() {
        let fixture = test_protocol("push-http-validation");
        let app = router(fixture.protocol.clone(), false);
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let authorization = format!("Bearer {access_token}");

        for (endpoint, p256dh, auth) in [
            (
                "http://push.example.test/device",
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "BTBZMqHH6r4Tts7J_aSIgg",
            ),
            (
                "https://user@push.example.test/device",
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "BTBZMqHH6r4Tts7J_aSIgg",
            ),
            (
                "https://push.example.test/device#fragment",
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "BTBZMqHH6r4Tts7J_aSIgg",
            ),
            ("https://push.example.test/device", "invalid", "invalid"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::PUT)
                        .uri("/api/push/subscription")
                        .header("authorization", &authorization)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "endpoint": endpoint,
                                "keys": {"p256dh": p256dh, "auth": auth},
                                "mode": "all",
                                "locale": "en-US"
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let unknown_field = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/push/subscription")
                    .header("authorization", &authorization)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "endpoint": "https://push.example.test/device",
                            "keys": {
                                "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                                "auth": "BTBZMqHH6r4Tts7J_aSIgg"
                            },
                            "mode": "all",
                            "locale": "en-US",
                            "device_id": DeviceId::new()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown_field.status(), StatusCode::BAD_REQUEST);

        let oversized = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/push/subscription")
                    .header("authorization", &authorization)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("x".repeat(PUSH_SUBSCRIPTION_JSON_MAX_BYTES + 1)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(oversized.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "push_subscription_too_large");
    }

    #[tokio::test]
    async fn push_routes_allow_get_put_and_delete_cors_preflight() {
        let app = router(test_protocol("push-http-cors").protocol.clone(), false);
        for (method, path) in [
            ("GET", "/api/push/config"),
            ("PUT", "/api/push/subscription"),
            ("DELETE", "/api/push/subscription"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::OPTIONS)
                        .uri(path)
                        .header("origin", "http://127.0.0.1:4173")
                        .header("access-control-request-method", method)
                        .header(
                            "access-control-request-headers",
                            "authorization,content-type,x-termd-server-id",
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|value| value.to_str().ok()),
                Some("*")
            );
        }
    }

    #[tokio::test]
    async fn v070_file_transfer_uploads_chunks_commits_and_downloads_raw_bytes() {
        let fixture = test_protocol("v070-file-transfer-state-machine");
        let (device_id, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let session_id = {
            let mut protocol = fixture.protocol.lock().await;
            let mut connection = ProtocolConnection::authenticated_v070_terminal(device_id);
            let opened = protocol
                .open_v070_terminal(
                    &mut connection,
                    V070TerminalOpen::Create(SessionCreatePayload {
                        command: vec!["sh".into()],
                        size: TerminalSize::new(24, 80),
                    }),
                )
                .unwrap();
            let session_id = opened.created.unwrap().session_id;
            connection.close(&mut protocol);
            session_id
        };
        let name = format!(".termd-v070-file-test-{}", SessionId::new().0);
        let target = std::env::current_dir().unwrap().join(&name);
        let app = router(fixture.protocol.clone(), false);
        let authorization = format!("Bearer {access_token}");

        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/files/uploads")
                    .header("authorization", &authorization)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "session_id": session_id,
                            "path": name,
                            "size_bytes": 6,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: serde_json::Value =
            serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let upload_id = created["upload_id"].as_str().unwrap();

        let chunk = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/files/uploads/{upload_id}/chunks"))
                    .header("authorization", &authorization)
                    .header("content-range", "bytes 0-5/6")
                    .body(Body::from("abcdef"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(chunk.status(), StatusCode::OK);

        let committed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/files/uploads/{upload_id}/commit"))
                    .header("authorization", &authorization)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(committed.status(), StatusCode::OK);

        let download = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/files/downloads")
                    .header("authorization", &authorization)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "session_id": session_id,
                            "path": name,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(download.status(), StatusCode::CREATED);
        let download: serde_json::Value =
            serde_json::from_slice(&to_bytes(download.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let download_id = download["download_id"].as_str().unwrap();

        let bytes = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/files/downloads/{download_id}"))
                    .header("authorization", &authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bytes.status(), StatusCode::OK);
        assert_eq!(
            &to_bytes(bytes.into_body(), usize::MAX).await.unwrap()[..],
            b"abcdef",
        );
        fs::remove_file(target).ok();
    }

    #[tokio::test]
    async fn v070_removed_http_session_polling_routes_are_not_mounted() {
        let app = router(test_protocol("removed-http-routes").protocol.clone(), false);
        for path in [
            "/api/control/session/list",
            "/api/control/session/00000000-0000-0000-0000-000000000000/attach",
            "/api/control/session/00000000-0000-0000-0000-000000000000/cursor",
            "/api/control/session/00000000-0000-0000-0000-000000000000/resize",
            "/api/control/daemon/clients",
            "/api/control/daemon/status",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn v070_pair_certificate_challenge_and_access_token_chain() {
        let fixture = test_protocol("credential-chain");
        let app = router(fixture.protocol.clone(), false);
        let local = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/local/pairing-token")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local.status(), StatusCode::OK);
        let local: serde_json::Value =
            serde_json::from_slice(&to_bytes(local.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let pair_ticket = local["token"].as_str().unwrap();
        assert_eq!(pair_ticket.split('.').count(), 3);

        let device_key = SigningKey::generate(&mut OsRng);
        let device_id = DeviceId::new();
        let device_public_key = format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(device_key.verifying_key().as_bytes())
        );
        let pair = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/pair")
                    .header("authorization", format!("TermdPair {pair_ticket}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "device_id": device_id,
                            "device_public_key": device_public_key,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pair.status(), StatusCode::OK);
        let pair: serde_json::Value =
            serde_json::from_slice(&to_bytes(pair.into_body(), usize::MAX).await.unwrap()).unwrap();
        let certificate = pair["device_certificate"].as_str().unwrap();

        let migration_challenge = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/device-certificate/migrate/challenge")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"device_id": device_id}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(migration_challenge.status(), StatusCode::OK);
        let migration_challenge: serde_json::Value = serde_json::from_slice(
            &to_bytes(migration_challenge.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let migration_challenge_value = migration_challenge["challenge"].as_str().unwrap();
        let migration_nonce = "device-migration-proof-nonce";
        let migration_timestamp_ms = current_unix_timestamp_millis().0;
        let server_id = local["server_id"].as_str().unwrap();
        let migration_signing_input = format!(
            "termd-access-token-v1\nserver_id={server_id}\ndevice_id={}\nchallenge={migration_challenge_value}\nnonce={migration_nonce}\ntimestamp_ms={migration_timestamp_ms}\n",
            device_id.0,
        );
        let migration_signature = format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(
                device_key
                    .sign(migration_signing_input.as_bytes())
                    .to_bytes()
            )
        );
        let migration = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/device-certificate/migrate")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "device_id": device_id,
                            "challenge": migration_challenge_value,
                            "nonce": migration_nonce,
                            "timestamp_ms": migration_timestamp_ms,
                            "signature": migration_signature,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(migration.status(), StatusCode::OK);
        let migration: serde_json::Value =
            serde_json::from_slice(&to_bytes(migration.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            migration["device_certificate"]
                .as_str()
                .unwrap()
                .split('.')
                .count(),
            3
        );

        let challenge = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/challenge")
                    .header("authorization", format!("TermdDevice {certificate}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"device_id": device_id}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(challenge.status(), StatusCode::OK);
        let challenge: serde_json::Value =
            serde_json::from_slice(&to_bytes(challenge.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let challenge_value = challenge["challenge"].as_str().unwrap();
        let nonce = "access-proof-nonce";
        let timestamp_ms = current_unix_timestamp_millis().0;
        let server_id = local["server_id"].as_str().unwrap();
        let signing_input = format!(
            "termd-access-token-v1\nserver_id={server_id}\ndevice_id={}\nchallenge={challenge_value}\nnonce={nonce}\ntimestamp_ms={timestamp_ms}\n",
            device_id.0,
        );
        let signature = format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD
                .encode(device_key.sign(signing_input.as_bytes()).to_bytes())
        );
        let access = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/auth/access-token")
                    .header("authorization", format!("TermdDevice {certificate}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "device_id": device_id,
                            "challenge": challenge_value,
                            "nonce": nonce,
                            "timestamp_ms": timestamp_ms,
                            "signature": signature,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(access.status(), StatusCode::OK);
        let access: serde_json::Value =
            serde_json::from_slice(&to_bytes(access.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            access["access_token"].as_str().unwrap().split('.').count(),
            3
        );
        assert_eq!(
            access["expires_at_ms"].as_u64().unwrap() - access["issued_at_ms"].as_u64().unwrap(),
            300_000
        );
    }

    async fn v070_access_token_for_test(protocol: &SharedDaemonProtocol) -> (DeviceId, String) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let device_id = DeviceId::new();
        let public_key = PublicKey(format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD
                .encode(signing_key.verifying_key().as_bytes())
        ));
        let now_ms = current_unix_timestamp_millis();
        let mut guard = protocol.lock().await;
        let (ticket, _) = guard.issue_pair_ticket_credential(now_ms).unwrap();
        let certificate = guard
            .pair_device_certificate(&ticket, device_id, public_key, now_ms)
            .unwrap();
        let challenge = guard
            .issue_access_token_challenge(&certificate, device_id, now_ms)
            .unwrap();
        let mut payload = termd_proto::AuthPayload {
            device_id,
            challenge: challenge.challenge,
            nonce: Nonce(format!("v070-ws-test-{}", ServerId::new().0)),
            timestamp_ms: now_ms,
            signature: Signature(String::new()),
        };
        payload.signature = Signature(format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(
                signing_key
                    .sign(
                        &AccessTokenProofInput {
                            server_id: guard.server_id(),
                            payload: &payload,
                        }
                        .to_bytes(),
                    )
                    .to_bytes(),
            )
        ));
        let (token, _) = guard
            .exchange_access_token(&certificate, payload, now_ms)
            .unwrap();
        (device_id, token)
    }

    #[tokio::test]
    async fn browser_session_http_routes_require_bearer_and_preserve_error_contract() {
        let fixture = test_protocol("browser-http-routes");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let app = router(fixture.protocol.clone(), false);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/browser/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let list = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/browser/sessions")
                    .header("authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let list: serde_json::Value =
            serde_json::from_slice(&to_bytes(list.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list["sessions"], serde_json::json!([]));

        let invalid = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/browser/sessions")
                    .header("authorization", format!("Bearer {access_token}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"url": "file:///etc/passwd", "width": 1280, "height": 800})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let invalid: serde_json::Value =
            serde_json::from_slice(&to_bytes(invalid.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(invalid["error"]["code"], "browser_url_invalid");

        let missing = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/browser/sessions/{}", BrowserSessionId::new()))
                    .header("authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn daemon_control_browser_routes_are_local_and_preserve_action_errors() {
        let fixture = test_protocol("browser-control-routes");
        let control = daemon_control_router(fixture.protocol.clone());

        let list = control
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/browser/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let list: serde_json::Value =
            serde_json::from_slice(&to_bytes(list.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list["sessions"], serde_json::json!([]));

        let invalid_open = control
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/browser/sessions")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"url": "file:///etc/passwd", "width": 1280, "height": 800})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_open.status(), StatusCode::BAD_REQUEST);

        let browser_id = BrowserSessionId::new();
        let missing_snapshot = control
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/v1/browser/sessions/{browser_id}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_snapshot.status(), StatusCode::NOT_FOUND);
        let missing_snapshot: serde_json::Value = serde_json::from_slice(
            &to_bytes(missing_snapshot.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(missing_snapshot["error"]["code"], "browser_not_found");

        let invalid_click = control
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/v1/browser/sessions/{browser_id}/click"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"selector":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_click.status(), StatusCode::BAD_REQUEST);
        let invalid_click: serde_json::Value = serde_json::from_slice(
            &to_bytes(invalid_click.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(invalid_click["error"]["code"], "browser_automation_invalid");

        let busy = browser_operation_error(BrowserError::AutomationBusy);
        assert_eq!(busy.status(), StatusCode::CONFLICT);
        let busy: serde_json::Value =
            serde_json::from_slice(&to_bytes(busy.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(busy["error"]["code"], "browser_automation_busy");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_websocket_pipes_binary_rfb_bytes_in_both_directions() {
        let (rfb_server, mut rfb_peer) = tokio::net::UnixStream::pair().unwrap();
        let rfb = Arc::new(Mutex::new(Some(rfb_server)));
        let app = Router::new().route(
            "/ws",
            get({
                let rfb = Arc::clone(&rfb);
                move |websocket: WebSocketUpgrade| {
                    let rfb = Arc::clone(&rfb);
                    async move {
                        let stream = rfb.lock().await.take().unwrap();
                        websocket
                            .protocols(["termd.rfb.v1"])
                            .on_upgrade(move |socket| run_browser_websocket(socket, stream))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "termd.rfb.v1".parse().unwrap());
        let (mut client, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("termd.rfb.v1")
        );

        let client_bytes = vec![0, 1, 2, 0xff, 0, 7];
        client
            .send(ClientWsMessage::Binary(client_bytes.clone()))
            .await
            .unwrap();
        let mut received = vec![0; client_bytes.len()];
        rfb_peer.read_exact(&mut received).await.unwrap();
        assert_eq!(received, client_bytes);

        let server_bytes = vec![0xff, 0, 3, 4, 0, 5];
        rfb_peer.write_all(&server_bytes).await.unwrap();
        let relayed = tokio::time::timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(relayed, ClientWsMessage::Binary(server_bytes));

        let _ = client.close(None).await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v070_direct_metadata_pong_echoes_client_timestamp() {
        let fixture = test_protocol("direct-metadata-pong");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = fixture.protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let mut metadata_request = format!("ws://{addr}/ws/metadata")
            .into_client_request()
            .unwrap();
        metadata_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut metadata, _) = tokio_tungstenite::connect_async(metadata_request)
            .await
            .expect("metadata websocket should upgrade");
        let snapshot = metadata.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()["type"],
            "metadata.snapshot"
        );

        let timestamp_ms = 1_710_000_000_123_u64;
        metadata
            .send(ClientWsMessage::Text(
                serde_json::json!({
                    "type": "metadata.ping",
                    "payload": { "timestamp_ms": timestamp_ms }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        // 周期状态采样 update 可能插队，循环过滤到 pong 为止。
        let pong = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let message = metadata.next().await.unwrap().unwrap().into_text().unwrap();
                let parsed: serde_json::Value = serde_json::from_str(&message).unwrap();
                if parsed["type"] == "metadata.pong" {
                    return parsed;
                }
            }
        })
        .await
        .expect("metadata pong should arrive without polling");
        assert_eq!(pong["type"], "metadata.pong");
        let echoed_timestamp_ms = pong["payload"]["timestamp_ms"].as_u64();

        drop(metadata);
        server.abort();
        assert_eq!(echoed_timestamp_ms, Some(timestamp_ms));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v070_metadata_client_diagnostics_keeps_connection_alive() {
        let fixture = test_protocol("metadata-client-diagnostics");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = fixture.protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let mut metadata_request = format!("ws://{addr}/ws/metadata")
            .into_client_request()
            .unwrap();
        metadata_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut metadata, _) = tokio_tungstenite::connect_async(metadata_request)
            .await
            .expect("metadata websocket should upgrade");
        let snapshot = metadata.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()["type"],
            "metadata.snapshot"
        );

        metadata
            .send(ClientWsMessage::Text(
                serde_json::json!({
                    "type": "client.diagnostics",
                    "payload": {
                        "context_id": "page-test123-abc",
                        "context_started_at": 1_750_000_000_000_u64,
                        "events": [
                            {
                                "t": 12.5,
                                "name": "terminal_writer_sequence_gap",
                                "fields": { "sequenceCursor": 3, "expected": 4 },
                            },
                            {
                                "t": 13.0,
                                "name": "terminal_pane_output_reset",
                                "fields": { "outputResetVersion": 2 },
                            },
                        ],
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // 诊断消息被处理后 metadata 循环仍然存活：ping 依然能收到 pong。
        // 周期状态采样 update 可能插队，循环过滤到 pong 为止。
        let timestamp_ms = 1_710_000_000_123_u64;
        metadata
            .send(ClientWsMessage::Text(
                serde_json::json!({
                    "type": "metadata.ping",
                    "payload": { "timestamp_ms": timestamp_ms }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let message = metadata.next().await.unwrap().unwrap().into_text().unwrap();
                let parsed: serde_json::Value = serde_json::from_str(&message).unwrap();
                if parsed["type"] == "metadata.pong" {
                    return parsed;
                }
            }
        })
        .await
        .expect("metadata pong should still arrive after client.diagnostics");
        assert_eq!(pong["type"], "metadata.pong");
        let echoed_timestamp_ms = pong["payload"]["timestamp_ms"].as_u64();

        drop(metadata);
        server.abort();
        assert_eq!(echoed_timestamp_ms, Some(timestamp_ms));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v070_metadata_status_refresh_pushes_periodic_updates() {
        let fixture = test_protocol("metadata-status-refresh");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = fixture.protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let mut metadata_request = format!("ws://{addr}/ws/metadata")
            .into_client_request()
            .unwrap();
        metadata_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut metadata, _) = tokio_tungstenite::connect_async(metadata_request)
            .await
            .expect("metadata websocket should upgrade");
        let snapshot = metadata.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()["type"],
            "metadata.snapshot"
        );

        // 即使没有任何事件驱动变化，daemon 也会在采样间隔内推送含最新
        // CPU/内存/网络状态的 metadata.update（token 换发不再重连后状态刷新的来源）。
        let update = tokio::time::timeout(Duration::from_secs(15), metadata.next())
            .await
            .expect("periodic metadata.update should arrive within the sampling interval")
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        let update: serde_json::Value = serde_json::from_str(&update).unwrap();
        assert_eq!(update["type"], "metadata.update");
        assert!(update["payload"]["revision"].as_u64().unwrap_or(0) >= 2);
        assert!(update["payload"]["state"]["daemon"].is_object());

        drop(metadata);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v070_workspace_websockets_authenticate_and_stream_snapshots() {
        let fixture = test_protocol("workspace-websocket-upgrade");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_protocol = fixture.protocol.clone();
        let server = tokio::spawn(async move {
            let _ = serve_listener(listener, server_protocol, false).await;
        });

        let mut metadata_request = format!("ws://{addr}/ws/metadata")
            .into_client_request()
            .unwrap();
        metadata_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut metadata, response) = tokio_tungstenite::connect_async(metadata_request)
            .await
            .expect("metadata websocket should upgrade");
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("termd.v0.7")
        );
        let metadata_snapshot = metadata.next().await.unwrap().unwrap().into_text().unwrap();
        let metadata_snapshot: serde_json::Value =
            serde_json::from_str(&metadata_snapshot).unwrap();
        assert_eq!(metadata_snapshot["type"], "metadata.snapshot");
        assert_eq!(metadata_snapshot["payload"]["revision"], 1);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut terminal_request = format!("ws://{addr}/ws/terminal")
            .into_client_request()
            .unwrap();
        terminal_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut terminal, _) = tokio_tungstenite::connect_async(terminal_request)
            .await
            .expect("terminal websocket should upgrade");
        terminal
            .send(ClientWsMessage::Text(
                serde_json::json!({
                    "type": "terminal.create",
                    "payload": {
                        "command": ["sh"],
                        "size": TerminalSize::new(24, 80),
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_str(&terminal.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        assert_eq!(created["type"], "terminal.created");
        let source_session_id = SessionId(
            uuid::Uuid::parse_str(created["payload"]["session_id"].as_str().unwrap()).unwrap(),
        );
        let source_session_root =
            persisted_session_root(&fixture._state_dir.state_path, source_session_id);
        let snapshot: serde_json::Value =
            serde_json::from_str(&terminal.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        assert_eq!(snapshot["type"], "terminal.snapshot");
        assert!(snapshot["payload"]["cursor"]["row"].as_u64().unwrap() >= 1);
        assert!(snapshot["payload"]["cursor"]["col"].as_u64().unwrap() >= 1);

        // 周期状态采样 update 可能插队；循环过滤到「包含新建 session」的 update。
        let metadata_update = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let message = metadata.next().await.unwrap().unwrap().into_text().unwrap();
                let parsed: serde_json::Value = serde_json::from_str(&message).unwrap();
                if parsed["type"] != "metadata.update" {
                    continue;
                }
                let session_count = parsed["payload"]["state"]["sessions"]
                    .as_array()
                    .map(|sessions| sessions.len())
                    .unwrap_or(0);
                if session_count == 1 {
                    return parsed;
                }
            }
        })
        .await
        .expect("session creation should push metadata without a polling delay");
        assert_eq!(metadata_update["type"], "metadata.update");
        // 中文注释：revision 只要求推进——周期状态采样可能插队，具体序号不是契约。
        assert!(metadata_update["payload"]["revision"].as_u64().unwrap_or(0) >= 2);

        let mut cwd_terminal_request = format!("ws://{addr}/ws/terminal")
            .into_client_request()
            .unwrap();
        cwd_terminal_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("termd.v0.7, {access_token}").parse().unwrap(),
        );
        let (mut cwd_terminal, _) = tokio_tungstenite::connect_async(cwd_terminal_request)
            .await
            .expect("cwd terminal websocket should upgrade");
        cwd_terminal
            .send(ClientWsMessage::Text(
                serde_json::json!({
                    "type": "terminal.create_in_session_cwd",
                    "payload": {
                        "source_session_id": source_session_id,
                        "command": ["sh"],
                        "size": TerminalSize::new(24, 80),
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let cwd_created: serde_json::Value = serde_json::from_str(
            &cwd_terminal
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cwd_created["type"], "terminal.created");
        let created_session_id = SessionId(
            uuid::Uuid::parse_str(cwd_created["payload"]["session_id"].as_str().unwrap()).unwrap(),
        );
        let cwd_snapshot: serde_json::Value = serde_json::from_str(
            &cwd_terminal
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cwd_snapshot["type"], "terminal.snapshot");
        let first_terminal_frame =
            tokio::time::timeout(Duration::from_secs(1), cwd_terminal.next())
                .await
                .expect("cwd terminal should stream its initial binary frame")
                .unwrap()
                .unwrap();
        assert!(first_terminal_frame.is_binary());
        assert_eq!(
            persisted_session_root(&fixture._state_dir.state_path, created_session_id),
            source_session_root
        );

        let _ = cwd_terminal.close(None).await;
        let _ = terminal.close(None).await;
        let _ = metadata.close(None).await;
        server.abort();
    }

    #[tokio::test]
    async fn version_endpoint_reports_termd_component() {
        let fixture = test_protocol("version-endpoint");
        let response = router(fixture.protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["component"], "termd");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn update_endpoints_require_device_bearer_authentication() {
        let fixture = test_protocol("update-endpoint-auth");
        let app = router(fixture.protocol.clone(), false);

        for path in [
            "/api/update/check",
            "/api/update/apply",
            "/api/update/relay",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("router should respond");
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{path} must require device authentication"
            );
        }
    }

    #[tokio::test]
    async fn update_check_reports_no_newer_release_for_current_build() {
        let fixture = test_protocol("update-check");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let response = router(fixture.protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/update/check")
                    .header("authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["current"], env!("CARGO_PKG_VERSION"));
        // 测试构建版本与最新 release 一致（或网络不可达）：只断言响应结构稳定，
        // 不依赖外网结果——两种结局（有更新/无更新）都返回 200。
        assert!(body.get("update_available").is_some());
    }

    #[tokio::test]
    async fn update_relay_rejects_when_no_relay_is_configured() {
        let fixture = test_protocol("update-relay-unconfigured");
        let (_, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let response = router(fixture.protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/update/relay")
                    .header("authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router should respond");
        // 未配置 relay 端点 → 400；已认证的调用不会拿到 401。
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn v070_close_session_uses_one_bearer_authenticated_json_request() {
        let fixture = test_protocol("v070-json-close");
        let (device_id, access_token) = v070_access_token_for_test(&fixture.protocol).await;
        let session_id = {
            let mut protocol = fixture.protocol.lock().await;
            let mut connection = ProtocolConnection::authenticated_v070_terminal(device_id);
            let opened = protocol
                .open_v070_terminal(
                    &mut connection,
                    V070TerminalOpen::Create(SessionCreatePayload {
                        command: vec!["sh".into()],
                        size: TerminalSize::new(24, 80),
                    }),
                )
                .unwrap();
            let session_id = opened.created.unwrap().session_id;
            connection.close(&mut protocol);
            session_id
        };

        let response = router(fixture.protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/control/session/{}/close", session_id.0))
                    .header("authorization", format!("Bearer {access_token}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["session_id"], session_id.0.to_string());
    }

    #[tokio::test]
    async fn v070_control_rejects_legacy_session_token_and_e2ee_headers_as_json() {
        let fixture = test_protocol("v070-reject-legacy-http-control");
        let response = router(fixture.protocol.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/control/session/00000000-0000-0000-0000-000000000401/close")
                    .header("authorization", "Bearer legacy-session-token")
                    .header("x-termd-server-id", ServerId::new().0.to_string())
                    .header("x-termd-device-id", DeviceId::new().0.to_string())
                    .header("x-termd-session-scope", "legacy-scope-token")
                    .header("x-termd-e2ee-public-key", "legacy-e2ee-key")
                    .header("x-termd-e2ee-nonce", "legacy-nonce")
                    .header("x-termd-e2ee-timestamp-ms", "1")
                    .header("x-termd-e2ee-signature", "legacy-signature")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "access_token_invalid");
    }

    #[test]
    fn v070_terminal_create_starts_attached_stream_with_one_based_cursor() {
        let fixture = test_protocol("terminal-create-snapshot");
        let mut protocol = fixture.protocol.blocking_lock();
        let device_id = DeviceId::new();
        let mut connection = ProtocolConnection::authenticated_v070_terminal(device_id);
        let opened = protocol
            .open_v070_terminal(
                &mut connection,
                V070TerminalOpen::Create(SessionCreatePayload {
                    command: vec!["sh".into()],
                    size: TerminalSize::new(24, 80),
                }),
            )
            .unwrap();

        assert!(opened.created.is_some());
        assert!((1..=opened.snapshot.size.rows).contains(&opened.snapshot.cursor.row));
        assert!((1..=opened.snapshot.size.cols).contains(&opened.snapshot.cursor.col));
        connection.close(&mut protocol);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn v070_terminal_create_in_session_cwd_uses_live_source_cwd() {
        let fixture = test_protocol("terminal-create-in-session-cwd");
        let source_cwd = fixture._state_dir.state_dir.join("source-cwd");
        fs::create_dir(&source_cwd).unwrap();
        let source_cwd = source_cwd.canonicalize().unwrap();
        let state_path = fixture._state_dir.state_path.clone();
        let mut protocol = fixture.protocol.blocking_lock();
        let device_id = DeviceId::new();
        let size = TerminalSize::new(24, 80);

        let mut source_connection = ProtocolConnection::authenticated_v070_terminal(device_id);
        let source = protocol
            .open_v070_terminal(
                &mut source_connection,
                V070TerminalOpen::Create(SessionCreatePayload {
                    command: vec![
                        "sh".into(),
                        "-c".into(),
                        "cd \"$1\" && exec sleep 60".into(),
                        "termd-test".into(),
                        source_cwd.to_string_lossy().into_owned(),
                    ],
                    size,
                }),
            )
            .unwrap()
            .created
            .unwrap();
        wait_for_session_path(
            &mut protocol,
            &mut source_connection,
            source.session_id,
            &source_cwd,
        );
        source_connection.close(&mut protocol);

        let mut target_connection =
            ProtocolConnection::authenticated_v070_terminal(DeviceId::new());
        let target = protocol
            .open_v070_terminal(
                &mut target_connection,
                V070TerminalOpen::CreateInSessionCwd(SessionCreateInSessionCwdPayload {
                    source_session_id: source.session_id,
                    command: vec!["sh".into(), "-c".into(), "exec sleep 60".into()],
                    size,
                }),
            )
            .unwrap()
            .created
            .unwrap();

        wait_for_session_path(
            &mut protocol,
            &mut target_connection,
            target.session_id,
            &source_cwd,
        );
        assert_eq!(
            persisted_session_root(&state_path, target.session_id),
            source_cwd
        );
        assert_eq!(protocol.snapshot_state().sessions.len(), 2);
        assert_eq!(session_storage_counts(&state_path), (2, 2));
        target_connection.close(&mut protocol);
    }

    #[test]
    fn v070_terminal_create_in_session_cwd_rejects_missing_source_without_side_effects() {
        let fixture = test_protocol("terminal-create-in-missing-session-cwd");
        let state_path = fixture._state_dir.state_path.clone();
        let mut protocol = fixture.protocol.blocking_lock();
        let mut connection = ProtocolConnection::authenticated_v070_terminal(DeviceId::new());

        let error = protocol
            .open_v070_terminal(
                &mut connection,
                V070TerminalOpen::CreateInSessionCwd(SessionCreateInSessionCwdPayload {
                    source_session_id: SessionId::new(),
                    command: vec!["sh".into()],
                    size: TerminalSize::new(24, 80),
                }),
            )
            .unwrap_err();

        assert_eq!(error.code(), "session_not_found");
        assert!(protocol.snapshot_state().sessions.is_empty());
        assert_eq!(session_storage_counts(&state_path), (0, 0));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn v070_terminal_create_in_session_cwd_does_not_fallback_to_deleted_cached_cwd() {
        let fixture = test_protocol("cwd-delete");
        let source_cwd = fixture._state_dir.state_dir.join("cwd");
        fs::create_dir(&source_cwd).unwrap();
        let source_cwd = source_cwd.canonicalize().unwrap();
        let state_path = fixture._state_dir.state_path.clone();
        let mut protocol = fixture.protocol.blocking_lock();
        let device_id = DeviceId::new();
        let size = TerminalSize::new(24, 80);

        let mut source_connection = ProtocolConnection::authenticated_v070_terminal(device_id);
        let source = protocol
            .open_v070_terminal(
                &mut source_connection,
                V070TerminalOpen::Create(SessionCreatePayload {
                    command: vec![
                        "sh".into(),
                        "-c".into(),
                        "cd \"$1\" && exec sleep 60".into(),
                        "termd-test".into(),
                        source_cwd.to_string_lossy().into_owned(),
                    ],
                    size,
                }),
            )
            .unwrap()
            .created
            .unwrap();
        wait_for_session_path(
            &mut protocol,
            &mut source_connection,
            source.session_id,
            &source_cwd,
        );
        source_connection.close(&mut protocol);
        fs::remove_dir(&source_cwd).unwrap();

        let mut target_connection =
            ProtocolConnection::authenticated_v070_terminal(DeviceId::new());
        let error = protocol
            .open_v070_terminal(
                &mut target_connection,
                V070TerminalOpen::CreateInSessionCwd(SessionCreateInSessionCwdPayload {
                    source_session_id: source.session_id,
                    command: vec!["sh".into()],
                    size,
                }),
            )
            .unwrap_err();

        assert_eq!(error.code(), "session_cwd_unavailable");
        assert_eq!(protocol.snapshot_state().sessions.len(), 1);
        assert_eq!(session_storage_counts(&state_path), (1, 1));
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

        let enabled_response = router(protocol.clone(), true)
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
    async fn web_fallback_does_not_handle_api_paths() {
        for path in ["/api", "/api/", "/api/unknown"] {
            let protocol = test_protocol("web-fallback-api");
            let response = router(protocol.clone(), true)
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn web_fallback_forwards_conditional_and_compression_headers() {
        use axum::http::header::{
            ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
            IF_NONE_MATCH, VARY,
        };

        let protocol = test_protocol("web-fallback-headers");
        let app = router(protocol.clone(), true);

        let initial = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        let etag = initial.headers().get(ETAG).cloned().expect("ETag");
        assert_eq!(initial.status(), StatusCode::OK);
        let initial_len = to_bytes(initial.into_body(), usize::MAX)
            .await
            .expect("initial body should be readable")
            .len();
        assert!(initial_len > 0);
        let repeated_len = to_bytes(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond")
                .into_body(),
            usize::MAX,
        )
        .await
        .expect("repeated body should be readable")
        .len();

        let not_modified = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers().get(ETAG), Some(&etag));
        assert_eq!(
            not_modified.headers().get(CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(not_modified.headers().get(VARY).unwrap(), "accept-encoding");
        assert!(not_modified.headers().contains_key(CONTENT_TYPE));
        assert_eq!(
            not_modified
                .headers()
                .get("x-content-type-options")
                .unwrap(),
            "nosniff"
        );
        let not_modified_len = to_bytes(not_modified.into_body(), usize::MAX)
            .await
            .expect("304 body should be readable")
            .len();
        assert_eq!(not_modified_len, 0);
        println!(
            "termd transfer identity: unconditional={} revalidated={} first={} second_304={}",
            initial_len + repeated_len,
            initial_len + not_modified_len,
            initial_len,
            not_modified_len
        );

        for encoding in ["gzip", "br"] {
            let encoded = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(ACCEPT_ENCODING, encoding)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");
            assert_eq!(encoded.headers().get(CONTENT_ENCODING).unwrap(), encoding);
            let encoded_etag = encoded.headers().get(ETAG).cloned().expect("ETag");
            let encoded_len = to_bytes(encoded.into_body(), usize::MAX)
                .await
                .expect("encoded body should be readable")
                .len();
            assert!(encoded_len > 0);
            let repeated_encoded_len = to_bytes(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/")
                            .header(ACCEPT_ENCODING, encoding)
                            .body(Body::empty())
                            .expect("test request should build"),
                    )
                    .await
                    .expect("router should respond")
                    .into_body(),
                usize::MAX,
            )
            .await
            .expect("repeated encoded body should be readable")
            .len();

            let encoded_not_modified = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(ACCEPT_ENCODING, encoding)
                        .header(IF_NONE_MATCH, encoded_etag)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");
            assert_eq!(encoded_not_modified.status(), StatusCode::NOT_MODIFIED);
            let encoded_not_modified_len = to_bytes(encoded_not_modified.into_body(), usize::MAX)
                .await
                .expect("encoded 304 body should be readable")
                .len();
            assert_eq!(encoded_not_modified_len, 0);
            println!(
                "termd transfer {encoding}: unconditional={} revalidated={} first={} second_304={}",
                encoded_len + repeated_encoded_len,
                encoded_len + encoded_not_modified_len,
                encoded_len,
                encoded_not_modified_len
            );

            let head = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::HEAD)
                        .uri("/")
                        .header(ACCEPT_ENCODING, encoding)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");
            assert_eq!(head.headers().get(CONTENT_ENCODING).unwrap(), encoding);
            assert!(head.headers().contains_key(CONTENT_LENGTH));
            assert!(head.headers().contains_key(ETAG));
            assert_eq!(head.headers().get(VARY).unwrap(), "accept-encoding");
            assert!(
                to_bytes(head.into_body(), usize::MAX)
                    .await
                    .expect("HEAD body should be readable")
                    .is_empty()
            );
        }

        let api_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/control/session/list")
                    .header(ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(api_response.status(), StatusCode::NOT_FOUND);
        assert!(api_response.headers().get(CONTENT_ENCODING).is_none());

        let ws_response = app
            .oneshot(
                Request::builder()
                    .uri("/ws")
                    .header(ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_ne!(ws_response.status(), StatusCode::OK);
        assert!(ws_response.headers().get(CONTENT_ENCODING).is_none());
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

        let daemon_public_key = protocol
            .lock()
            .await
            .daemon_public_identity()
            .public_key
            .clone();
        let claims = verify_credential(
            &payload.token,
            &daemon_public_key,
            payload.server_id,
            current_unix_timestamp_millis(),
            CredentialKind::PairTicket,
        )
        .expect("local pairing endpoint should return a signed pair ticket");
        assert_eq!(claims.expires_at_ms, payload.expires_at_ms);
        assert_eq!(payload.ttl_ms, DaemonConfig::default().pairing_token_ttl_ms);
        assert!(payload.expires_at_ms.0 > current_unix_timestamp_millis().0);
        assert_eq!(payload.server_id, server_id);
        assert_eq!(payload.ws_url, "ws://127.0.0.1:8765/ws");
        assert!(!response.body.contains("server_private_key"));
        assert!(!response.body.contains("terminal sentinel"));
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
        let protocol = protocol.lock().await;
        verify_credential(
            &payload.token,
            &protocol.daemon_public_identity().public_key,
            payload.server_id,
            current_unix_timestamp_millis(),
            CredentialKind::PairTicket,
        )
        .expect("local pairing endpoint should return a signed pair ticket");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_pairing_token_endpoint_returns_configured_relay_client_url() {
        let mut config = test_config("local-pairing-token-relay-url");
        config.relay_endpoints = vec!["wss://relay.example/ws".to_owned()];
        config.default_pairing_ws_url = "wss://relay.example/ws".to_owned();
        let protocol = config.into_protocol();
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
        assert!(response.contains("\"daemon_public_key\":\"ed25519-v1:"));
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

    async fn tls_healthz_request(addr: SocketAddr, cert_path: &PathBuf) -> String {
        let mut root_store = rustls::RootCertStore::empty();
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for cert in certs {
            root_store.add(cert).unwrap();
        }
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
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
}
