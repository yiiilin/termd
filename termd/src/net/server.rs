//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth、session 和 E2EE
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

use std::net::{AddrParseError, IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    ErrorPayload, MessageType, PairingToken, ProtocolVersion, ServerId, UnixTimestampMillis,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{debug, warn};

use crate::auth::current_unix_timestamp_millis;
use crate::config::DaemonConfig;

use super::protocol::{DaemonProtocol, JsonEnvelope, ProtocolError, envelope_value};
use super::pty_bridge::NonBlockingPortablePtyBackend;
use super::signature::Ed25519SignatureVerifier;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;

pub type DefaultDaemonProtocol =
    DaemonProtocol<NonBlockingPortablePtyBackend, Ed25519SignatureVerifier>;
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
}

/// 构造生产默认协议状态。pairing token 入口仍留给后续本地 CLI 接入。
pub fn default_protocol(config: DaemonConfig) -> SharedDaemonProtocol {
    Arc::new(Mutex::new(DaemonProtocol::new(
        config,
        NonBlockingPortablePtyBackend::new(),
        Ed25519SignatureVerifier,
    )))
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
    let protocol = protocol.lock().expect("daemon protocol mutex poisoned");

    Json(HealthzPayload {
        status: "ok",
        protocol_version: ProtocolVersion::default(),
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
    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
    let ttl_ms = protocol.config().pairing_token_ttl_ms;
    let server_id = protocol.server_id();
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
        }),
    )
        .into_response()
}

fn is_loopback_peer(peer_addr: SocketAddr) -> bool {
    peer_addr.ip().is_loopback()
}

async fn ws_handler(
    websocket: WebSocketUpgrade,
    State(protocol): State<SharedDaemonProtocol>,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| handle_socket(socket, protocol))
}

async fn handle_socket(socket: WebSocket, protocol: SharedDaemonProtocol) {
    let (mut sender, mut receiver) = socket.split();
    let (mut connection, initial_messages) = {
        let protocol = protocol.lock().expect("daemon protocol mutex poisoned");
        protocol.start_connection()
    };

    for envelope in initial_messages {
        if send_envelope(&mut sender, envelope).await.is_err() {
            return;
        }
    }

    while let Some(message) = receiver.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "websocket receive failed");
                break;
            }
        };

        match message {
            Message::Ping(payload) => {
                let _ = sender.send(Message::Pong(payload)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            other => {
                let Some(envelope) = (match message_to_envelope(other) {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        let _ = send_envelope(&mut sender, plaintext_error(error)).await;
                        continue;
                    }
                }) else {
                    break;
                };

                let responses = {
                    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                    connection.handle_wire_envelope(&mut protocol, envelope)
                };

                let mut send_failed = false;
                for response in responses {
                    if send_envelope(&mut sender, response).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
                if send_failed {
                    break;
                }

                let output_responses = {
                    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                    connection
                        .read_attached_outputs(&mut protocol, OUTPUT_FLUSH_MAX_BYTES_PER_SESSION)
                };

                // 入站帧处理后做一次最小输出 flush；持续后台推送留在后续优化中接入。
                for response in output_responses {
                    if send_envelope(&mut sender, response).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
                if send_failed {
                    break;
                }
            }
        }
    }

    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
    connection.close(&mut protocol);
    debug!("websocket connection closed and detached");
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

async fn send_envelope(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    envelope: JsonEnvelope,
) -> Result<(), ()> {
    let raw = serde_json::to_string(&envelope).map_err(|error| {
        warn!(%error, "failed to serialize websocket envelope");
    })?;

    sender.send(Message::Text(raw)).await.map_err(|error| {
        warn!(%error, "failed to send websocket envelope");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use termd_proto::{
        DeviceId, E2eeKeyExchangePayload, PairAcceptPayload, PairRequestPayload, PublicKey,
        UnixTimestampMillis,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::auth::current_unix_timestamp_millis;
    use crate::net::protocol::{
        ProtocolConnection, decode_payload, encrypted_frame_from_envelope, envelope_value,
    };
    use crate::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    #[derive(Debug, Deserialize)]
    struct PairingTokenResponse {
        token: String,
        expires_at_ms: UnixTimestampMillis,
        ttl_ms: u64,
        server_id: ServerId,
    }

    #[test]
    fn router_exposes_healthz_and_ws_routes() {
        let protocol = default_protocol(DaemonConfig::default());
        let _router = router(protocol, false);
    }

    #[tokio::test]
    async fn web_fallback_is_opt_in() {
        let protocol = default_protocol(DaemonConfig::default());
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
        let protocol = default_protocol(DaemonConfig::default());
        let server_id = {
            protocol
                .lock()
                .expect("daemon protocol mutex poisoned")
                .server_id()
        };
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
        assert!(!response.body.contains("server_private_key"));
        assert!(!response.body.contains("terminal sentinel"));

        let pair_accept = pair_device_with_http_token(protocol, payload.token);
        assert_eq!(pair_accept.server_id, server_id);
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
        let protocol = default_protocol(DaemonConfig::default());
        let server_id = {
            protocol
                .lock()
                .expect("daemon protocol mutex poisoned")
                .server_id()
        };
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

    fn pair_device_with_http_token(
        protocol: SharedDaemonProtocol,
        token: String,
    ) -> PairAcceptPayload {
        let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
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
            E2eeKeyExchangePayload {
                server_id: protocol.server_id(),
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: termd_proto::Nonce("nonce-e2ee-test".to_owned()),
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        )
        .unwrap();

        let responses = connection.handle_wire_envelope(protocol, handshake);
        assert!(responses.is_empty());
        device_session
    }
}
