//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth 和 session
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

mod recovery;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::net::{AddrParseError, IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, OriginalUri, Path, State};
use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, post, put};
use axum::{Json, Router};
#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use termd_proto::{
    DeviceId, ErrorPayload, PROTOCOL_PACKET_VERSION, ProtocolVersion, ServerId,
    SessionFileDownloadPreparePayload, SessionFileUploadPayload, SessionId, SessionState,
    UnixTimestampMillis, is_http_control_tunnel_path_allowed, is_http_tunnel_path_allowed,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

use crate::auth::current_unix_timestamp_millis;
use crate::config::DaemonConfig;
use crate::pty::PtyRestoreInfo;
use crate::pty::supervisor::SupervisorPtyBackend;
use crate::state::{StateError, StateStore};

use super::protocol::{
    DaemonProtocol, ProtocolConnection, ProtocolError, V070TerminalOpen,
    cleanup_persisted_session_file_http_uploads,
};
use super::signature::Ed25519SignatureVerifier;
use recovery::warn_about_orphaned_supervisors;

const HTTP_JSON_MAX_BYTES: usize = 1024 * 1024;
const V070_FILE_CHUNK_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_METADATA_TIMESTAMP_MS: u64 = 9_007_199_254_740_991;

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
        .merge(auth_api_router())
        .merge(http_control_api_router())
        .merge(http_file_api_router())
        .route("/ws/metadata", get(metadata_ws_handler))
        .route("/ws/terminal", get(terminal_ws_handler))
        .method_not_allowed_fallback(api_method_not_allowed)
        .with_state(protocol);

    if web_enabled {
        router.fallback(web_or_api_fallback)
    } else {
        router.fallback(api_or_plain_not_found)
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

fn websocket_access_token(headers: &HeaderMap) -> Option<&str> {
    let mut protocols = headers
        .get("sec-websocket-protocol")?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim);
    if protocols.next()? != "termd.v0.7" {
        return None;
    }
    protocols
        .next()
        .filter(|token| token.split('.').count() == 3 && !token.contains(char::is_whitespace))
}

async fn authorize_workspace_websocket(
    protocol: &SharedDaemonProtocol,
    headers: &HeaderMap,
) -> Result<DeviceId, Response> {
    let token = websocket_access_token(headers).ok_or_else(|| {
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
    let device_id = match authorize_workspace_websocket(&protocol, &headers).await {
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
    let (mut changes, mut previous) = {
        let mut guard = protocol.lock().await;
        let changes = guard.v070_metadata_signal();
        let payload = match guard.v070_metadata_payload(device_id) {
            Ok(payload) => payload,
            Err(_) => return,
        };
        (changes, payload)
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
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(raw))) => {
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
            }
        }
    }
}

async fn terminal_ws_handler(
    State(protocol): State<SharedDaemonProtocol>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let device_id = match authorize_workspace_websocket(&protocol, &headers).await {
        Ok(device_id) => device_id,
        Err(response) => return response,
    };
    websocket
        .protocols(["termd.v0.7"])
        .on_upgrade(move |socket| run_terminal_websocket(socket, protocol, device_id))
        .into_response()
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
    let payload = command.get("payload").cloned().unwrap_or(Value::Null);
    let open: Result<V070TerminalOpen, ()> = match command.get("type").and_then(Value::as_str) {
        Some("terminal.create") => serde_json::from_value(payload)
            .map(V070TerminalOpen::Create)
            .map_err(|_| ()),
        Some("terminal.attach") => serde_json::from_value(payload)
            .map(V070TerminalOpen::Attach)
            .map_err(|_| ()),
        _ => Err(()),
    };
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

    // TLS 只替换 transport accept 层；router 和协议状态机保持同一套认证与 session 规则。
    serve_rustls_listener(listener, router(protocol, web_enabled), tls_config).await
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
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    use serde::Deserialize;
    use std::fs;
    use std::io::{Read, Write};
    use std::ops::{Deref, DerefMut};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use termd_proto::{
        DeviceId, Nonce, PublicKey, SessionCreatePayload, Signature, TerminalSize,
        UnixTimestampMillis,
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
        let pong = tokio::time::timeout(Duration::from_millis(250), metadata.next())
            .await
            .expect("metadata pong should arrive without polling")
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        let pong: serde_json::Value = serde_json::from_str(&pong).unwrap();
        assert_eq!(pong["type"], "metadata.pong");
        let echoed_timestamp_ms = pong["payload"]["timestamp_ms"].as_u64();

        drop(metadata);
        server.abort();
        assert_eq!(echoed_timestamp_ms, Some(timestamp_ms));
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
        let snapshot: serde_json::Value =
            serde_json::from_str(&terminal.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        assert_eq!(snapshot["type"], "terminal.snapshot");
        assert!(snapshot["payload"]["cursor"]["row"].as_u64().unwrap() >= 1);
        assert!(snapshot["payload"]["cursor"]["col"].as_u64().unwrap() >= 1);

        let metadata_update = tokio::time::timeout(Duration::from_millis(250), metadata.next())
            .await
            .expect("session creation should push metadata without a polling delay")
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        let metadata_update: serde_json::Value = serde_json::from_str(&metadata_update).unwrap();
        assert_eq!(metadata_update["type"], "metadata.update");
        assert_eq!(metadata_update["payload"]["revision"], 2);
        assert_eq!(
            metadata_update["payload"]["state"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let _ = terminal.close(None).await;
        let _ = metadata.close(None).await;
        server.abort();
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
}
