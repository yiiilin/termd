use axum::body::Body;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use termd_proto::{RelayRouteKind, ServerId, is_http_tunnel_path_allowed};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::ws::{
    RegisterDaemonError, RelaySignedCredentialKind, RelayState, WEBSOCKET_MAX_FRAME_SIZE,
    WEBSOCKET_MAX_MESSAGE_SIZE, handle_socket, handle_workspace_socket,
};

pub fn router(state: RelayState, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(relay_ws))
        .route("/ws/metadata", get(relay_metadata_ws))
        .route("/ws/terminal", get(relay_terminal_ws))
        .route("/api/relay/daemon/register", post(register_daemon))
        .merge(http_api_tunnel_router())
        .method_not_allowed_fallback(api_method_not_allowed);

    // 中文注释：所有 API namespace 都要在 Web fallback 前截止，未知 API 不能返回 SPA index。
    let router = router
        .route("/api", any(api_not_found))
        .route("/api/", any(api_not_found))
        .route("/api/*path", any(api_not_found))
        .with_state(state);

    if web_enabled {
        router.fallback(termweb::embedded_web_handler_with_headers)
    } else {
        router.fallback(api_not_found)
    }
}

async fn relay_metadata_ws(
    State(state): State<RelayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    relay_workspace_ws(state, headers, websocket, RelayRouteKind::Metadata).await
}

async fn relay_terminal_ws(
    State(state): State<RelayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    relay_workspace_ws(state, headers, websocket, RelayRouteKind::Terminal).await
}

async fn relay_workspace_ws(
    state: RelayState,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
    route_kind: RelayRouteKind,
) -> Response {
    let access_token = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let mut protocols = value.split(',').map(str::trim);
            (protocols.next() == Some("termd.v0.7"))
                .then(|| protocols.next())
                .flatten()
        })
        .filter(|token| token.split('.').count() == 3)
        .map(str::to_owned);
    let Some(access_token) = access_token else {
        return relay_json_error(
            StatusCode::UNAUTHORIZED,
            "access_token_required",
            "a valid access token is required",
        );
    };
    let Ok(server_id) = state.verify_workspace_access_token(&access_token) else {
        return relay_json_error(
            StatusCode::UNAUTHORIZED,
            "access_token_invalid",
            "access token is invalid or expired",
        );
    };
    websocket
        .max_frame_size(WEBSOCKET_MAX_FRAME_SIZE)
        .max_message_size(WEBSOCKET_MAX_MESSAGE_SIZE)
        .protocols(["termd.v0.7"])
        .on_upgrade(move |socket| {
            handle_workspace_socket(socket, state, server_id, route_kind, access_token)
        })
        .into_response()
}

fn relay_json_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"code": code, "message": message, "retryable": false}
        })),
    )
        .into_response()
}

fn http_api_tunnel_router() -> Router<RelayState> {
    // 中文注释：relay 只做 tunnel 转发，不参与 bearer 业务判断。
    let router = Router::new()
        .route(
            "/api/control/*path",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/auth/*path",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/uploads",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/uploads/:id/chunks",
            put(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/uploads/:id/commit",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/uploads/:id/abort",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/downloads",
            post(relay_http_tunnel).options(relay_http_tunnel_preflight),
        )
        .route(
            "/api/files/downloads/:id",
            get(relay_http_tunnel).options(relay_http_tunnel_preflight),
        );

    // 中文注释：跨源预检只挂在 relay HTTP API tunnel 上；真正鉴权仍在 daemon access token。
    router.route_layer(http_api_tunnel_cors_layer())
}

async fn relay_http_tunnel_preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn http_api_tunnel_cors_layer() -> CorsLayer {
    // 中文注释：relay 透明转发 HTTP control/file tunnel，不解析业务内容。
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            HeaderName::from_static("content-range"),
            HeaderName::from_static("authorization"),
            HeaderName::from_static("x-termd-server-id"),
        ])
}

async fn api_not_found() -> impl IntoResponse {
    relay_json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "relay application route was not found",
    )
}

async fn api_method_not_allowed() -> impl IntoResponse {
    relay_json_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this route",
    )
}

async fn relay_http_tunnel(
    State(state): State<RelayState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !is_http_api_tunnel_path_allowed(method.as_str(), uri.path()) {
        return relay_json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "relay application route was not found",
        );
    }
    let Some(server_id) = headers
        .get("x-termd-server-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(ServerId)
    else {
        return relay_json_error(
            StatusCode::BAD_REQUEST,
            "server_id_required",
            "x-termd-server-id must contain a valid server id",
        );
    };
    if let Err(response) = authorize_relay_http_request(&state, server_id, uri.path(), &headers) {
        return response;
    }
    let forwarded_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect::<Vec<_>>();
    match state
        .http_tunnel(
            server_id,
            method.as_str().to_owned(),
            uri.path().to_owned(),
            forwarded_headers,
            body.into_data_stream(),
        )
        .await
    {
        Ok(response) => response,
        Err(status) => relay_json_error(
            status,
            "relay_tunnel_failed",
            "relay could not forward the application request",
        ),
    }
}

#[derive(Debug, Clone, Copy)]
enum RelayHttpAdmissionRequirement {
    Bootstrap,
    Signed {
        scheme: &'static str,
        kind: RelaySignedCredentialKind,
    },
}

// Returning a complete rejection response keeps admission handling at this HTTP boundary.
#[allow(clippy::result_large_err)]
fn authorize_relay_http_request(
    state: &RelayState,
    server_id: ServerId,
    path: &str,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !state.trusted_admission_enabled() {
        return Ok(());
    }
    let Some(requirement) = relay_http_admission_requirement(path) else {
        return Err(relay_json_error(
            StatusCode::FORBIDDEN,
            "relay_admission_policy_missing",
            "relay admission policy does not allow this application route",
        ));
    };
    let RelayHttpAdmissionRequirement::Signed {
        scheme: required_scheme,
        kind,
    } = requirement
    else {
        // Existing-device migration is a bootstrap path: the daemon verifies its
        // challenge proof because no v0.7 signed credential exists yet.
        return Ok(());
    };
    let mut authorization_values = headers.get_all("authorization").iter();
    let Some(authorization) = authorization_values.next() else {
        return Err(relay_json_error(
            StatusCode::UNAUTHORIZED,
            "authorization_required",
            "an authorization credential is required",
        ));
    };
    if authorization_values.next().is_some() {
        return Err(relay_json_error(
            StatusCode::UNAUTHORIZED,
            "authorization_invalid",
            "authorization credentials are invalid",
        ));
    }
    let Some((scheme, credential)) = authorization
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
    else {
        return Err(relay_json_error(
            StatusCode::UNAUTHORIZED,
            "authorization_invalid",
            "authorization credentials are invalid",
        ));
    };
    if scheme != required_scheme
        || credential.is_empty()
        || credential.contains(char::is_whitespace)
    {
        return Err(relay_json_error(
            StatusCode::UNAUTHORIZED,
            "authorization_invalid",
            "authorization credentials are invalid",
        ));
    }
    if state.daemon_public_key(server_id).is_none() {
        return Err(relay_json_error(
            StatusCode::FORBIDDEN,
            "daemon_identity_untrusted",
            "the requested daemon identity is not trusted by this relay",
        ));
    }
    state
        .verify_signed_credential(server_id, credential, kind)
        .map_err(|_| {
            relay_json_error(
                StatusCode::UNAUTHORIZED,
                "credential_invalid",
                "authorization credential is invalid or expired",
            )
        })
}

fn relay_http_admission_requirement(path: &str) -> Option<RelayHttpAdmissionRequirement> {
    match path {
        "/api/auth/pair" => Some(RelayHttpAdmissionRequirement::Signed {
            scheme: "TermdPair",
            kind: RelaySignedCredentialKind::PairTicket,
        }),
        "/api/auth/challenge" | "/api/auth/access-token" => {
            Some(RelayHttpAdmissionRequirement::Signed {
                scheme: "TermdDevice",
                kind: RelaySignedCredentialKind::DeviceCertificate,
            })
        }
        "/api/auth/device-certificate/migrate"
        | "/api/auth/device-certificate/migrate/challenge" => {
            Some(RelayHttpAdmissionRequirement::Bootstrap)
        }
        path if path.starts_with("/api/control/") || path.starts_with("/api/files/") => {
            Some(RelayHttpAdmissionRequirement::Signed {
                scheme: "Bearer",
                kind: RelaySignedCredentialKind::AccessToken,
            })
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct RegisterDaemonRequest {
    server_id: ServerId,
    daemon_token: String,
    #[serde(default)]
    daemon_public_key: Option<termd_proto::PublicKey>,
}

async fn register_daemon(
    State(state): State<RelayState>,
    headers: HeaderMap,
    request: Result<Json<RegisterDaemonRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => {
            return relay_json_error(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body must be valid JSON",
            );
        }
    };
    let setup_token = headers
        .get("x-termd-relay-setup-token")
        .and_then(|value| value.to_str().ok());
    let result = match request.daemon_public_key {
        Some(public_key) => state.register_daemon_identity(
            request.server_id,
            request.daemon_token,
            public_key,
            setup_token,
        ),
        None => state.register_daemon(request.server_id, request.daemon_token, setup_token),
    };
    match result {
        Ok(outcome) => Json(outcome).into_response(),
        Err(RegisterDaemonError::SetupTokenMissing | RegisterDaemonError::SetupTokenRejected) => {
            relay_json_error(
                StatusCode::UNAUTHORIZED,
                "setup_token_invalid",
                "relay setup token is missing or invalid",
            )
        }
        Err(RegisterDaemonError::DaemonTokenTooShort) => relay_json_error(
            StatusCode::BAD_REQUEST,
            "daemon_token_invalid",
            "daemon token does not meet relay requirements",
        ),
        Err(RegisterDaemonError::SetupTokenNotConfigured)
        | Err(RegisterDaemonError::RegistryPathNotConfigured) => relay_json_error(
            StatusCode::NOT_IMPLEMENTED,
            "relay_registration_unavailable",
            "relay daemon registration is not configured",
        ),
        Err(RegisterDaemonError::Poisoned) | Err(RegisterDaemonError::PersistRegistry) => {
            relay_json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "relay_registry_unavailable",
                "relay daemon registry is unavailable",
            )
        }
    }
}

fn is_http_api_tunnel_path_allowed(method: &str, path: &str) -> bool {
    // 中文注释：relay 只做 tunnel 前置路由，实际 bearer 由 daemon 校验。
    // 路由白名单复用 proto 共享函数，避免 relay 和 daemon 的外层协议面漂移。
    is_http_tunnel_path_allowed(method, path)
}

#[derive(Debug, Serialize)]
struct HealthzPayload {
    status: &'static str,
    rooms: usize,
    daemon_controls: usize,
    latest_daemon_control_connection_id: u64,
    trusted_admission: bool,
}

async fn healthz(State(state): State<RelayState>) -> Json<HealthzPayload> {
    let (daemon_controls, latest_daemon_control_connection_id) = state.daemon_control_stats();
    Json(HealthzPayload {
        status: "ok",
        rooms: state.room_count(),
        daemon_controls,
        latest_daemon_control_connection_id,
        trusted_admission: state.trusted_admission_enabled(),
    })
}

async fn relay_ws(
    State(state): State<RelayState>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_response(state, websocket)
}

fn ws_response(state: RelayState, websocket: WebSocketUpgrade) -> axum::response::Response {
    websocket
        .max_frame_size(WEBSOCKET_MAX_FRAME_SIZE)
        .max_message_size(WEBSOCKET_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request};
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use futures_util::{SinkExt, StreamExt};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use termd::auth::{
        AccessTokenProofInput, CredentialService, DaemonIdentity, current_unix_timestamp_millis,
    };
    use termd::config::DaemonConfig;
    use termd::net::relay::{RelayReconnectPolicy, run_relay_mux_with_reconnect};
    use termd::net::server::default_protocol;
    use termd_proto::{
        AuthPayload, DeviceId, Envelope, MessageType, Nonce, ProtocolVersion, PublicKey,
        RelayAdmissionPayload, RelayClientId, RelayControlEnvelope, RouteHelloPayload,
        RouteReadyPayload, RouteRole, ServerId, Signature, UnixTimestampMillis,
    };
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
    use tower::ServiceExt as _;

    type TestWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    struct RelayHttpCredentialFixture {
        state: RelayState,
        identity: DaemonIdentity,
        pair_ticket: String,
        device_certificate: String,
        access_token: String,
        now: UnixTimestampMillis,
    }

    fn relay_http_credential_fixture() -> RelayHttpCredentialFixture {
        let identity = DaemonIdentity::generate();
        let now = current_unix_timestamp_millis();
        let service = CredentialService::new(identity.clone());
        let device_id = DeviceId::new();
        let device_key = SigningKey::from_bytes(&[41; 32]);
        let device_public_key = PublicKey(format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(device_key.verifying_key().as_bytes()),
        ));
        let pair_ticket = service
            .issue_pair_ticket(now, UnixTimestampMillis(now.0.saturating_add(60_000)))
            .expect("test pair ticket should be signed");
        let device_certificate = service
            .issue_device_certificate(device_id, device_public_key, now)
            .expect("test device certificate should be signed");
        let access_token = service
            .issue_access_token(
                device_id,
                now,
                UnixTimestampMillis(now.0.saturating_add(300_000)),
            )
            .expect("test access token should be signed");
        let state = RelayState::new_trusted(vec![
            crate::ws::RelayDaemonCredential::plain_token(
                identity.server_id(),
                "daemon-secret-1".to_owned(),
            )
            .with_public_key(Some(identity.public_key().clone())),
        ]);
        RelayHttpCredentialFixture {
            state,
            identity,
            pair_ticket,
            device_certificate,
            access_token,
            now,
        }
    }

    async fn relay_http_request(
        state: RelayState,
        server_id: ServerId,
        path: &str,
        authorization: Option<&str>,
    ) -> Response {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("x-termd-server-id", server_id.0.to_string());
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        router(state, false)
            .oneshot(
                request
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond")
    }

    async fn relay_error_code(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("relay error body should be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("relay error body should be JSON");
        body["error"]["code"]
            .as_str()
            .expect("relay error should have a code")
            .to_owned()
    }

    #[test]
    fn router_can_be_constructed() {
        let _router = router(RelayState::default(), false);
    }

    #[tokio::test]
    async fn v070_workspace_websocket_routes_are_mounted() {
        let app = router(RelayState::default(), false);
        for path in ["/ws/metadata", "/ws/terminal"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("connection", "upgrade")
                        .header("upgrade", "websocket")
                        .header("sec-websocket-version", "13")
                        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v070_relay_runtime_forwards_metadata_and_terminal_create_attach_streams() {
        let state_dir = PrivateTestStateDir::new("v070-runtime-state");
        let state_path = state_dir.path().join("daemon-state.sqlite");
        let protocol = default_protocol(DaemonConfig::default_for_state_path(&state_path));
        let now = current_unix_timestamp_millis();
        let device_id = termd_proto::DeviceId::new();
        let device_key = SigningKey::from_bytes(&[23; 32]);
        let device_public_key = PublicKey(format!(
            "ed25519-v1:{}",
            base64::engine::general_purpose::STANDARD.encode(device_key.verifying_key().as_bytes()),
        ));
        let (server_id, daemon_public_key, access_token) = {
            let mut daemon = protocol.lock().await;
            let (pair_ticket, _) = daemon.issue_pair_ticket_credential(now).unwrap();
            let certificate = daemon
                .pair_device_certificate(&pair_ticket, device_id, device_public_key, now)
                .unwrap();
            let challenge = daemon
                .issue_access_token_challenge(&certificate, device_id, now)
                .unwrap();
            let mut proof = AuthPayload {
                device_id,
                challenge: challenge.challenge,
                nonce: Nonce(format!("v070-runtime-proof-{}", uuid::Uuid::new_v4())),
                timestamp_ms: now,
                signature: Signature("ed25519-v1:placeholder".to_owned()),
            };
            proof.signature = Signature(format!(
                "ed25519-v1:{}",
                base64::engine::general_purpose::STANDARD.encode(
                    device_key
                        .sign(
                            &AccessTokenProofInput {
                                server_id: daemon.server_id(),
                                payload: &proof,
                            }
                            .to_bytes(),
                        )
                        .to_bytes(),
                ),
            ));
            let (access_token, _) = daemon
                .exchange_access_token(&certificate, proof, now)
                .unwrap();
            (
                daemon.server_id(),
                daemon.daemon_public_identity().public_key.clone(),
                access_token,
            )
        };
        let daemon_token = "v070-runtime-daemon-token";
        let relay_state = RelayState::new_trusted(vec![
            crate::ws::RelayDaemonCredential::plain_token(server_id, daemon_token.to_owned())
                .with_public_key(Some(daemon_public_key)),
        ]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = relay_state.clone();
        let relay_server = tokio::spawn(async move {
            axum::serve(listener, router(server_state, false))
                .await
                .unwrap();
        });
        let relay_url = format!("ws://{addr}");
        let connector_protocol = protocol.clone();
        let connector = tokio::spawn(async move {
            run_relay_mux_with_reconnect(
                &relay_url,
                Some(daemon_token),
                None,
                RelayReconnectPolicy::default(),
                connector_protocol,
            )
            .await
        });
        timeout(Duration::from_secs(3), async {
            while relay_healthz_value(relay_state.clone()).await["daemon_controls"] != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon connector should register with the relay");

        let connect_workspace = |kind: &str| {
            let url = format!("ws://{addr}/ws/{kind}");
            let protocol_header = format!("termd.v0.7, {access_token}");
            async move {
                let mut request = url.into_client_request().unwrap();
                request.headers_mut().insert(
                    "sec-websocket-protocol",
                    HeaderValue::from_str(&protocol_header).unwrap(),
                );
                connect_async(request).await.unwrap().0
            }
        };

        let mut metadata = connect_workspace("metadata").await;
        let metadata_snapshot: serde_json::Value =
            serde_json::from_str(&next_workspace_text(&mut metadata).await).unwrap();
        assert_eq!(metadata_snapshot["type"], "metadata.snapshot");
        assert_eq!(metadata_snapshot["payload"]["revision"], 1);

        let mut terminal = connect_workspace("terminal").await;
        terminal
            .send(ClientMessage::Text(
                serde_json::json!({
                    "type": "terminal.create",
                    "payload": {
                        "command": ["/bin/sh", "-lc", "printf relay-v070-ready; sleep 2"],
                        "size": {"rows": 24, "cols": 80, "pixel_width": 0, "pixel_height": 0}
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_str(&next_workspace_text(&mut terminal).await).unwrap();
        assert_eq!(created["type"], "terminal.created");
        let session_id = created["payload"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let snapshot: serde_json::Value =
            serde_json::from_str(&next_workspace_text(&mut terminal).await).unwrap();
        assert_eq!(snapshot["type"], "terminal.snapshot");
        assert_eq!(snapshot["payload"]["cursor"]["row"], 1);
        assert!(!next_workspace_binary(&mut terminal).await.is_empty());
        terminal.close(None).await.unwrap();

        let mut attached = connect_workspace("terminal").await;
        attached
            .send(ClientMessage::Text(
                serde_json::json!({
                    "type": "terminal.attach",
                    "payload": {"session_id": session_id}
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let attached_response: serde_json::Value =
            serde_json::from_str(&next_workspace_text(&mut attached).await).unwrap();
        assert_eq!(attached_response["type"], "terminal.attached");
        let attached_snapshot: serde_json::Value =
            serde_json::from_str(&next_workspace_text(&mut attached).await).unwrap();
        assert_eq!(attached_snapshot["type"], "terminal.snapshot");
        assert!(!next_workspace_binary(&mut attached).await.is_empty());

        attached.close(None).await.unwrap();
        metadata.close(None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2300)).await;
        connector.abort();
        relay_server.abort();
        drop(protocol);
    }

    async fn next_workspace_text(socket: &mut TestWs) -> String {
        loop {
            match timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                ClientMessage::Text(text) => return text.to_string(),
                ClientMessage::Ping(bytes) => {
                    socket.send(ClientMessage::Pong(bytes)).await.unwrap()
                }
                ClientMessage::Pong(_) => {}
                other => panic!("expected workspace text frame, got {other:?}"),
            }
        }
    }

    async fn next_workspace_binary(socket: &mut TestWs) -> Vec<u8> {
        loop {
            match timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                ClientMessage::Binary(bytes) => return bytes.to_vec(),
                ClientMessage::Ping(bytes) => {
                    socket.send(ClientMessage::Pong(bytes)).await.unwrap()
                }
                ClientMessage::Pong(_) => {}
                other => panic!("expected workspace binary frame, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn healthz_reports_daemon_control_readiness() {
        let server_id = ServerId::new();
        let state = RelayState::default();
        let before = relay_healthz_value(state.clone()).await;
        assert_eq!(before["daemon_controls"], 0);
        assert_eq!(before["latest_daemon_control_connection_id"], 0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(server_state, false))
                .await
                .unwrap();
        });
        let (mut daemon_control, _response) =
            connect_async(format!("ws://{addr}/ws")).await.unwrap();
        register_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;

        let after = relay_healthz_value(state).await;
        // 中文注释：fixture 只读轮询这个字段，不再用真实 client route 消耗一次性配对票据。
        assert_eq!(after["daemon_controls"], 1);
        assert!(
            after["latest_daemon_control_connection_id"]
                .as_u64()
                .is_some_and(|id| id > 0)
        );
    }

    #[tokio::test]
    async fn trusted_relay_forwards_valid_pair_ticket_to_http_tunnel() {
        let fixture = relay_http_credential_fixture();
        let authorization = format!("TermdPair {}", fixture.pair_ticket);
        let response = relay_http_request(
            fixture.state,
            fixture.identity.server_id(),
            "/api/auth/pair",
            Some(&authorization),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "valid pair admission should reach the offline daemon tunnel boundary"
        );
    }

    #[tokio::test]
    async fn trusted_relay_http_routes_accept_only_their_signed_credential_kind() {
        let fixture = relay_http_credential_fixture();
        let server_id = fixture.identity.server_id();
        let cases = [
            (
                "/api/auth/pair",
                Some(format!("TermdPair {}", fixture.pair_ticket)),
            ),
            (
                "/api/auth/challenge",
                Some(format!("TermdDevice {}", fixture.device_certificate)),
            ),
            (
                "/api/auth/access-token",
                Some(format!("TermdDevice {}", fixture.device_certificate)),
            ),
            (
                "/api/control/session/reorder",
                Some(format!("Bearer {}", fixture.access_token)),
            ),
            (
                "/api/files/uploads",
                Some(format!("Bearer {}", fixture.access_token)),
            ),
            ("/api/auth/device-certificate/migrate/challenge", None),
            ("/api/auth/device-certificate/migrate", None),
        ];

        for (path, authorization) in cases {
            let response = relay_http_request(
                fixture.state.clone(),
                server_id,
                path,
                authorization.as_deref(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "valid admission for {path} should reach the offline daemon boundary"
            );
        }
    }

    #[tokio::test]
    async fn trusted_relay_http_routes_reject_missing_mismatched_and_invalid_credentials() {
        let fixture = relay_http_credential_fixture();
        let server_id = fixture.identity.server_id();
        let wrong_signature =
            CredentialService::new(DaemonIdentity::generate_for_server_id(server_id))
                .issue_pair_ticket(
                    fixture.now,
                    UnixTimestampMillis(fixture.now.0.saturating_add(60_000)),
                )
                .unwrap();
        let wrong_issuer = CredentialService::new(DaemonIdentity::generate())
            .issue_pair_ticket(
                fixture.now,
                UnixTimestampMillis(fixture.now.0.saturating_add(60_000)),
            )
            .unwrap();
        let expired = CredentialService::new(fixture.identity.clone())
            .issue_pair_ticket(
                UnixTimestampMillis(fixture.now.0.saturating_sub(2_000)),
                UnixTimestampMillis(fixture.now.0.saturating_sub(1_000)),
            )
            .unwrap();
        let cases = [
            ("/api/auth/pair", None, "authorization_required"),
            (
                "/api/auth/pair?relay_token=query-only",
                None,
                "authorization_required",
            ),
            (
                "/api/auth/pair",
                Some(format!("Bearer {}", fixture.pair_ticket)),
                "authorization_invalid",
            ),
            (
                "/api/auth/challenge",
                Some(format!("TermdDevice {}", fixture.pair_ticket)),
                "credential_invalid",
            ),
            (
                "/api/control/session/reorder",
                Some(format!("Bearer {}", fixture.device_certificate)),
                "credential_invalid",
            ),
            (
                "/api/auth/pair",
                Some(format!("TermdPair {wrong_signature}")),
                "credential_invalid",
            ),
            (
                "/api/auth/pair",
                Some(format!("TermdPair {wrong_issuer}")),
                "credential_invalid",
            ),
            (
                "/api/auth/pair",
                Some(format!("TermdPair {expired}")),
                "credential_invalid",
            ),
        ];

        for (path, authorization, expected_code) in cases {
            let response = relay_http_request(
                fixture.state.clone(),
                server_id,
                path,
                authorization.as_deref(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(relay_error_code(response).await, expected_code, "{path}");
        }

        let unknown_server = ServerId::new();
        let authorization = format!("TermdPair {}", fixture.pair_ticket);
        let response = relay_http_request(
            fixture.state,
            unknown_server,
            "/api/auth/pair",
            Some(&authorization),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            relay_error_code(response).await,
            "daemon_identity_untrusted"
        );
    }

    #[tokio::test]
    async fn router_mounts_all_v070_http_file_tunnel_routes() {
        for (method, path) in [
            (Method::POST, "/api/files/uploads"),
            (Method::PUT, "/api/files/uploads/upload-id/chunks"),
            (Method::POST, "/api/files/uploads/upload-id/commit"),
            (Method::POST, "/api/files/uploads/upload-id/abort"),
            (Method::POST, "/api/files/downloads"),
            (Method::GET, "/api/files/downloads/download-id"),
        ] {
            let response = router(RelayState::default(), true)
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("x-termd-server-id", ServerId::new().0.to_string())
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn router_mounts_http_control_tunnel() {
        for path in [
            "/api/control/session/reorder",
            "/api/control/daemon/client_forget",
        ] {
            let response = router(RelayState::default(), true)
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header("x-termd-server-id", ServerId::new().0.to_string())
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            // 中文注释：control plane 是当前 relay Web 的主链路；没有 daemon 在线时也应进入
            // tunnel 转发路径并返回 503，而不是被当成未知 API 或静态页面。
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        }
    }

    #[tokio::test]
    async fn router_rejects_unknown_http_control_tunnel_path() {
        let response = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/control/auth/verify")
                    .header("x-termd-server-id", ServerId::new().0.to_string())
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        // 中文注释：relay 仍然只暴露当前 Web/relay 实际需要的 control 路径，不能整段放开 namespace。
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_rejects_unknown_http_control_tunnel_path_before_relay_auth() {
        for path in [
            "/api/control/auth/verify",
            "/api/control/session/not-a-uuid/files",
        ] {
            let response = router(RelayState::new(), true)
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header("x-termd-server-id", ServerId::new().0.to_string())
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            // 中文注释：未知或畸形 control 路径必须在 relay token 认证前被路径层拒绝，避免
            // trusted relay 暴露更宽的 API 探测面，也让 direct/tunnel 的失败语义一致。
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn router_does_not_fallback_to_web_for_api_namespace() {
        for (method, path) in [
            (Method::GET, "/api/"),
            (Method::GET, "/api/unknown"),
            (Method::GET, "/api/files/downloads/download-id/extra"),
        ] {
            let response = router(RelayState::default(), true)
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(path)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            // 中文注释：API namespace 必须在 Web fallback 前截止，不能返回 SPA index。
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }

        let slash_variant = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/files/uploads/upload-id/chunks/extra")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert!(
            !slash_variant.status().is_success(),
            "unknown file API must not be served by Web fallback"
        );
    }

    #[tokio::test]
    async fn router_always_mounts_v070_http_file_tunnel() {
        let response = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/files/downloads/download-id")
                    .header("x-termd-server-id", ServerId::new().0.to_string())
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        // 正式 v0.7 文件路由始终进入 tunnel；没有 daemon 在线时返回 503，而不是禁用提示。
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn router_answers_cors_preflight_for_http_file_tunnel() {
        let response = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/files/uploads/upload-id/chunks")
                    .header("origin", "http://127.0.0.1:4173")
                    .header("access-control-request-method", "PUT")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-range,x-termd-server-id",
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
    async fn router_answers_cors_preflight_for_http_control_tunnel() {
        let response = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/control/session/reorder")
                    .header("origin", "http://127.0.0.1:4173")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type,x-termd-server-id",
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
    async fn router_does_not_add_cors_headers_to_non_file_api_routes() {
        let response = router(RelayState::default(), true)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/unknown")
                    .header("origin", "http://127.0.0.1:4173")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }

    #[tokio::test]
    async fn web_fallback_is_opt_in() {
        let disabled_response = router(RelayState::default(), false)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(disabled_response.status(), StatusCode::NOT_FOUND);

        let enabled_response = router(RelayState::default(), true)
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
    async fn web_fallback_forwards_conditional_and_compression_headers() {
        use axum::http::header::{
            ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
            IF_NONE_MATCH, VARY,
        };

        let app = router(RelayState::default(), true);
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
        let initial_len = axum::body::to_bytes(initial.into_body(), usize::MAX)
            .await
            .expect("initial body should be readable")
            .len();
        assert!(initial_len > 0);
        let repeated_len = axum::body::to_bytes(
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
        let not_modified_len = axum::body::to_bytes(not_modified.into_body(), usize::MAX)
            .await
            .expect("304 body should be readable")
            .len();
        assert_eq!(not_modified_len, 0);
        println!(
            "termrelay transfer identity: unconditional={} revalidated={} first={} second_304={}",
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
            let encoded_len = axum::body::to_bytes(encoded.into_body(), usize::MAX)
                .await
                .expect("encoded body should be readable")
                .len();
            assert!(encoded_len > 0);
            let repeated_encoded_len = axum::body::to_bytes(
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
            let encoded_not_modified_len =
                axum::body::to_bytes(encoded_not_modified.into_body(), usize::MAX)
                    .await
                    .expect("encoded 304 body should be readable")
                    .len();
            assert_eq!(encoded_not_modified_len, 0);
            println!(
                "termrelay transfer {encoding}: unconditional={} revalidated={} first={} second_304={}",
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
                axum::body::to_bytes(head.into_body(), usize::MAX)
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
                    .header("x-termd-server-id", ServerId::new().0.to_string())
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

    #[test]
    #[cfg(any())]
    fn auth_state_accepts_only_matching_relay_token_when_configured() {
        let state = RelayState::new(Some("relay-secret-1".to_owned()));

        assert!(state.authorizes(Some("relay-secret-1")));
        assert!(!state.authorizes(None));
        assert!(!state.authorizes(Some("wrong-secret")));
    }

    #[tokio::test]
    async fn register_daemon_api_persists_and_activates_daemon_admission() {
        let server_id = ServerId::new();
        let registry_path = temp_registry_path("register-daemon-api");
        let state = RelayState::new_trusted_with_registry(
            Vec::new(),
            Some("relay-setup-secret-1".to_owned()),
            Some(registry_path.clone()),
        )
        .expect("trusted relay registry should initialize");
        let response = router(state.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/relay/daemon/register")
                    .header("content-type", "application/json")
                    .header("x-termd-relay-setup-token", "relay-setup-secret-1")
                    .body(Body::from(format!(
                        r#"{{"server_id":"{}","daemon_token":"daemon-secret-1"}}"#,
                        server_id.0
                    )))
                    .expect("registration request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = fs::read_to_string(&registry_path)
            .expect("registration should persist daemon registry");
        assert!(body.contains(&server_id.0.to_string()));
        assert!(!body.contains("daemon-secret-1"));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(server_state, false))
                .await
                .unwrap();
        });
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _response) = connect_async(url).await.unwrap();
        register_route_with_admission(
            &mut daemon_control,
            server_id,
            RouteRole::DaemonControl,
            Some(RelayAdmissionPayload::Daemon {
                token: "daemon-secret-1".to_owned(),
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn v070_daemon_registration_persists_only_public_key_for_offline_verification() {
        let registry_path = temp_registry_path("v070-daemon-public-key");
        let state = RelayState::new_trusted_with_registry(
            Vec::new(),
            Some("relay-setup-secret-1".to_owned()),
            Some(registry_path.clone()),
        )
        .unwrap();
        let identity = termd::auth::DaemonIdentity::generate();
        let response = router(state.clone(), false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/relay/daemon/register")
                    .header("x-termd-relay-setup-token", "relay-setup-secret-1")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "server_id": identity.server_id(),
                            "daemon_token": "daemon-secret-for-v070",
                            "daemon_public_key": identity.public_key(),
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.daemon_public_key(identity.server_id()),
            Some(identity.public_key().clone())
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        assert_eq!(
            persisted["daemons"][0]["daemon_public_key"],
            identity.public_key().0
        );
        assert!(persisted.to_string().find("device").is_none());
        let _ = fs::remove_file(registry_path);
    }

    #[tokio::test]
    async fn register_daemon_api_replaces_existing_token_with_setup_token() {
        let server_id = ServerId::new();
        let registry_path = temp_registry_path("replace-daemon-api");
        let state = RelayState::new_trusted_with_registry(
            vec![crate::ws::RelayDaemonCredential::plain_token(
                server_id,
                "old-daemon-secret".to_owned(),
            )],
            Some("relay-setup-secret-1".to_owned()),
            Some(registry_path),
        )
        .expect("trusted relay registry should initialize");

        for token in ["new-daemon-secret", "new-daemon-secret"] {
            let response = router(state.clone(), false)
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/relay/daemon/register")
                        .header("content-type", "application/json")
                        .header("x-termd-relay-setup-token", "relay-setup-secret-1")
                        .body(Body::from(format!(
                            r#"{{"server_id":"{}","daemon_token":"{}"}}"#,
                            server_id.0, token
                        )))
                        .expect("registration request should build"),
                )
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::OK);
        }

        assert!(
            state
                .authorize_test_route(
                    server_id,
                    RouteRole::DaemonControl,
                    RelayAdmissionPayload::Daemon {
                        token: "new-daemon-secret".to_owned(),
                    },
                )
                .is_ok()
        );
        assert!(
            state
                .authorize_test_route(
                    server_id,
                    RouteRole::DaemonControl,
                    RelayAdmissionPayload::Daemon {
                        token: "old-daemon-secret".to_owned(),
                    },
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn trusted_relay_rejects_legacy_client_role_even_with_pair_ticket() {
        let server_id = ServerId::new();
        let state = RelayState::new_trusted(vec![crate::ws::RelayDaemonCredential::plain_token(
            server_id,
            "daemon-secret-1".to_owned(),
        )]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(server_state, false))
                .await
                .unwrap();
        });
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_route_with_admission(
            &mut daemon_control,
            server_id,
            RouteRole::DaemonControl,
            Some(RelayAdmissionPayload::Daemon {
                token: "daemon-secret-1".to_owned(),
            }),
        )
        .await;
        let (mut client, _client_response) = connect_async(url).await.unwrap();
        let mut hello = route_hello(server_id, RouteRole::Client, None, None);
        hello.payload.admission = Some(RelayAdmissionPayload::PairTicket {
            token: termd_proto::PairingToken("termd-pair-test".to_owned()),
        });
        client
            .send(ClientMessage::Text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();

        let raw = next_text(&mut client).await;
        let error: Envelope<termd_proto::ErrorPayload> = serde_json::from_str(&raw).unwrap();
        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error.payload.code, "relay_role_not_supported");
    }

    #[tokio::test]
    async fn trusted_relay_ignores_legacy_query_token_for_client_admission() {
        let server_id = ServerId::new();
        let state = RelayState::new_trusted(vec![crate::ws::RelayDaemonCredential::plain_token(
            server_id,
            "daemon-secret-1".to_owned(),
        )]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(server_state, false))
                .await
                .unwrap();
        });
        let url = format!("ws://{addr}/ws?relay_token=legacy-relay-secret");
        let (mut client, _client_response) = connect_async(url).await.unwrap();

        let hello = route_hello(server_id, RouteRole::Client, None, None);
        client
            .send(ClientMessage::Text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();

        let raw = next_text(&mut client).await;
        let error: Envelope<termd_proto::ErrorPayload> = serde_json::from_str(&raw).unwrap();
        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error.payload.code, "relay_role_not_supported");
    }

    #[tokio::test]
    async fn old_path_based_websocket_routes_are_removed() {
        let server_id = ServerId::new();

        for path in [
            format!("/ws/{}/daemon", server_id.0),
            format!("/ws/{}/daemon-mux", server_id.0),
            format!("/ws/{}/client", server_id.0),
        ] {
            let response = router(RelayState::default(), false)
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("test request should build"),
                )
                .await
                .expect("router should respond");

            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn websocket_route_prelude_forwards_non_json_text_and_binary_raw() {
        let fixture = relay_http_credential_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let relay_state = fixture.state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(relay_state, false))
                .await
                .unwrap();
        });
        let server_id = fixture.identity.server_id();
        let base_url = format!("ws://{addr}");
        let (mut daemon_control, _daemon_response) =
            connect_async(format!("{base_url}/ws")).await.unwrap();
        register_route_with_admission(
            &mut daemon_control,
            server_id,
            RouteRole::DaemonControl,
            Some(RelayAdmissionPayload::Daemon {
                token: "daemon-secret-1".to_owned(),
            }),
        )
        .await;
        let (mut client, mut daemon_data, _) = register_client_data_pipe(
            &base_url,
            server_id,
            &fixture.access_token,
            "daemon-secret-1",
            &mut daemon_control,
        )
        .await;

        // 中文注释：prelude 之后的帧必须保持 opaque，即使它不是 JSON，也不能被 relay 解析。
        client
            .send(ClientMessage::Text("{not-json".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            next_data_frame(&mut daemon_data).await.unwrap(),
            ClientMessage::Text("{not-json".to_owned())
        );

        daemon_data
            .send(ClientMessage::Binary(vec![0, 1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(
            next_data_frame(&mut client).await.unwrap(),
            ClientMessage::Binary(vec![0, 1, 2, 3])
        );
    }

    #[tokio::test]
    async fn independent_data_pipes_keep_forwarding_when_another_client_backpressures() {
        let fixture = relay_http_credential_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let relay_state = fixture.state.clone();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(relay_state, false))
                .await
                .unwrap();
        });
        let server_id = fixture.identity.server_id();
        let base_url = format!("ws://{addr}");
        let (mut daemon_control, _daemon_response) =
            connect_async(format!("{base_url}/ws")).await.unwrap();
        register_route_with_admission(
            &mut daemon_control,
            server_id,
            RouteRole::DaemonControl,
            Some(RelayAdmissionPayload::Daemon {
                token: "daemon-secret-1".to_owned(),
            }),
        )
        .await;
        let (mut target_client, mut target_data, _) = register_client_data_pipe(
            &base_url,
            server_id,
            &fixture.access_token,
            "daemon-secret-1",
            &mut daemon_control,
        )
        .await;
        let (mut flood_client, _flood_data, _) = register_client_data_pipe(
            &base_url,
            server_id,
            &fixture.access_token,
            "daemon-secret-1",
            &mut daemon_control,
        )
        .await;

        let flood_payload = vec![b'x'; 900 * 1024];
        for _ in 0..16 {
            if tokio::time::timeout(
                Duration::from_millis(20),
                flood_client.send(ClientMessage::Binary(flood_payload.clone())),
            )
            .await
            .is_err()
            {
                break;
            }
        }

        target_data
            .send(ClientMessage::Text(
                "daemon-response-while-other-pipe-backpressured".to_owned(),
            ))
            .await
            .unwrap();

        let received = timeout(
            Duration::from_millis(300),
            next_data_frame(&mut target_client),
        )
        .await
        .expect("independent daemon data pipe should keep forwarding")
        .expect("target client websocket should produce a data frame");
        assert_eq!(
            received,
            ClientMessage::Text("daemon-response-while-other-pipe-backpressured".to_owned())
        );
    }

    type TestSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn relay_healthz_value(state: RelayState) -> serde_json::Value {
        let response = router(state, false)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("healthz request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("healthz body should be readable");
        serde_json::from_slice(&body).expect("healthz body should be JSON")
    }

    async fn register_route(socket: &mut TestSocket, server_id: ServerId, role: RouteRole) {
        register_route_with_admission(socket, server_id, role, None).await;
    }

    async fn register_route_with_admission(
        socket: &mut TestSocket,
        server_id: ServerId,
        role: RouteRole,
        admission: Option<RelayAdmissionPayload>,
    ) {
        let mut hello = route_hello(server_id, role, None, None);
        hello.payload.admission = admission;
        socket
            .send(ClientMessage::Text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();

        let ready: Envelope<RouteReadyPayload> =
            serde_json::from_str(&next_text(socket).await).unwrap();
        assert_eq!(ready.kind, MessageType::RouteReady);
        assert_eq!(ready.payload.server_id, server_id);
        assert_eq!(ready.payload.role, role);
    }

    async fn register_client_data_pipe(
        base_url: &str,
        server_id: ServerId,
        access_token: &str,
        daemon_token: &str,
        daemon_control: &mut TestSocket,
    ) -> (TestSocket, TestSocket, RelayClientId) {
        let mut request = format!("{base_url}/ws/terminal")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&format!("termd.v0.7, {access_token}")).unwrap(),
        );
        let (client, _client_response) = connect_async(request).await.unwrap();
        let (client_id, data_token) = expect_open_data(daemon_control).await;

        let (mut daemon_data, _data_response) =
            connect_async(format!("{base_url}/ws")).await.unwrap();
        let mut data_hello = route_hello(
            server_id,
            RouteRole::DaemonData,
            Some(client_id),
            Some(data_token),
        );
        data_hello.payload.admission = Some(RelayAdmissionPayload::Daemon {
            token: daemon_token.to_owned(),
        });
        daemon_data
            .send(ClientMessage::Text(
                serde_json::to_string(&data_hello).unwrap(),
            ))
            .await
            .unwrap();

        let data_ready: Envelope<RouteReadyPayload> =
            serde_json::from_str(&next_text(&mut daemon_data).await).unwrap();
        assert_eq!(data_ready.kind, MessageType::RouteReady);
        assert_eq!(data_ready.payload.role, RouteRole::DaemonData);

        (client, daemon_data, client_id)
    }

    fn route_hello(
        server_id: ServerId,
        role: RouteRole,
        client_id: Option<RelayClientId>,
        data_token: Option<Nonce>,
    ) -> Envelope<RouteHelloPayload> {
        let route_generation = match role {
            RouteRole::DaemonControl | RouteRole::DaemonData => Some(Nonce(format!(
                "router-test-route-generation-{}",
                server_id.0
            ))),
            RouteRole::Client | RouteRole::DaemonMux => None,
        };
        Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role,
                protocol_version: ProtocolVersion::default(),
                nonce: Nonce("route-test-nonce".to_owned()),
                admission: None,
                route_generation,
                client_id,
                data_token,
                timestamp_ms: termd_proto::UnixTimestampMillis(1_710_000_000_000),
            },
        )
    }

    async fn expect_open_data(socket: &mut TestSocket) -> (RelayClientId, Nonce) {
        loop {
            match timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("daemon control should receive open_data")
                .expect("daemon control websocket should stay open")
                .expect("daemon control frame should be valid")
            {
                ClientMessage::Text(text) => {
                    match serde_json::from_str::<RelayControlEnvelope>(&text)
                        .expect("relay control envelope should decode")
                    {
                        RelayControlEnvelope::OpenData {
                            client_id,
                            data_token,
                            ..
                        } => return (client_id, data_token),
                        other => panic!("expected open_data, got {other:?}"),
                    }
                }
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                other => panic!("expected relay control text frame, got {other:?}"),
            }
        }
    }

    async fn next_text(socket: &mut TestSocket) -> String {
        loop {
            match timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("relay should send a text frame")
                .expect("relay websocket should stay open")
                .expect("relay frame should be valid")
            {
                ClientMessage::Text(text) => return text,
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                other => panic!("expected text frame, got {other:?}"),
            }
        }
    }

    async fn next_data_frame(socket: &mut TestSocket) -> Option<ClientMessage> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "relay should send a data frame");
            match timeout(remaining, socket.next())
                .await
                .expect("relay should send a data frame")?
                .expect("relay frame should be valid")
            {
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                frame => return Some(frame),
            }
        }
    }

    fn temp_registry_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "termd-termrelay-{label}-{}-{index}.json",
            std::process::id()
        ))
    }

    struct PrivateTestStateDir {
        path: PathBuf,
    }

    impl PrivateTestStateDir {
        fn new(label: &str) -> Self {
            let path = temp_registry_path(label).with_extension("state");
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            builder
                .create(&path)
                .expect("private test state directory should be created");
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for PrivateTestStateDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
