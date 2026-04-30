//! Axum HTTP/WebSocket 适配层。
//!
//! 这里只把 socket 字节流接到 `protocol` 状态机；pairing、auth、session 和 E2EE
//! 规则都由协议核心执行，避免网络框架层夹带业务判断。

use std::net::{AddrParseError, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
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

pub fn router(protocol: SharedDaemonProtocol) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/local/pairing-token", post(local_pairing_token))
        .route("/ws", get(ws_handler))
        .with_state(protocol)
}

pub async fn serve(
    config: DaemonConfig,
    protocol: SharedDaemonProtocol,
) -> Result<(), ServerError> {
    let addr: SocketAddr = format!("{}:{}", config.listen_host, config.listen_port).parse()?;
    let listener = TcpListener::bind(addr).await.map_err(ServerError::Bind)?;

    serve_listener(listener, protocol).await
}

/// 使用调用方已经绑定好的 listener 启动 daemon HTTP 服务。
///
/// 该函数只服务网络启动边界，方便集成测试使用随机端口；auth、session 和 E2EE 语义仍全部
/// 留在 `DaemonProtocol` 中，避免为了测试放宽生产协议。
pub async fn serve_listener(
    listener: TcpListener,
    protocol: SharedDaemonProtocol,
) -> Result<(), ServerError> {
    axum::serve(
        listener,
        router(protocol).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(ServerError::Serve)
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
    use std::io::{Read, Write};
    use termd_proto::{
        DeviceId, E2eeKeyExchangePayload, PairAcceptPayload, PairRequestPayload, PublicKey,
        UnixTimestampMillis,
    };

    use crate::auth::current_unix_timestamp_millis;
    use crate::net::protocol::{
        ProtocolConnection, decode_payload, encrypted_frame_from_envelope, envelope_value,
    };
    use crate::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};

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
        let _router = router(protocol);
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
            let _ = serve_listener(listener, server_protocol).await;
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
