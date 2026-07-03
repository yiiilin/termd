use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use termd_proto::{
    HTTP_FILE_TUNNEL_PATHS, RelayAdmissionPayload, ServerId, is_http_tunnel_path_allowed,
};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::ws::{RelayState, WEBSOCKET_MAX_FRAME_SIZE, WEBSOCKET_MAX_MESSAGE_SIZE, handle_socket};

pub fn router(state: RelayState, web_enabled: bool, http_tunnel_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(relay_ws))
        .merge(http_api_tunnel_router(http_tunnel_enabled));

    // 中文注释：所有 API namespace 都要在 Web fallback 前截止，未知 API 不能返回 SPA index。
    let router = router
        .route("/api", any(api_not_found))
        .route("/api/", any(api_not_found))
        .route("/api/*path", any(api_not_found))
        .with_state(state);

    if web_enabled {
        router.fallback(termweb::embedded_web_handler)
    } else {
        router
    }
}

fn http_api_tunnel_router(http_tunnel_enabled: bool) -> Router<RelayState> {
    let mut router = Router::new();
    // 中文注释：HTTP control plane 已是当前 Web/relay 主路径，必须默认可用；
    // relay 只做 tunnel 转发，不参与 bearer/session scope 业务判断。
    router = router.route("/api/control/*path", post(relay_http_tunnel));
    for path in HTTP_FILE_TUNNEL_PATHS {
        router = if http_tunnel_enabled {
            router.route(path, post(relay_http_tunnel))
        } else {
            // 中文注释：默认显式挡住文件 API，避免 Web fallback 把禁用的 tunnel 当作前端路由。
            router.route(path, any(relay_http_tunnel_disabled))
        };
    }

    // 中文注释：跨源预检只挂在 relay HTTP API tunnel 上；真正鉴权仍在 daemon bearer/scope token。
    router.route_layer(http_api_tunnel_cors_layer())
}

fn http_api_tunnel_cors_layer() -> CorsLayer {
    // 中文注释：relay 透明转发 HTTP control/file tunnel，不解密也不解析业务内容；
    // 浏览器跨源访问时需要放开 control 与 file API 共用的认证头。
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            HeaderName::from_static("authorization"),
            HeaderName::from_static("x-termd-server-id"),
            HeaderName::from_static("x-termd-device-id"),
            HeaderName::from_static("x-termd-session-scope"),
            HeaderName::from_static("x-termd-e2ee-public-key"),
            HeaderName::from_static("x-termd-e2ee-nonce"),
            HeaderName::from_static("x-termd-e2ee-timestamp-ms"),
            HeaderName::from_static("x-termd-e2ee-signature"),
            HeaderName::from_static("x-termd-relay-admission"),
        ])
}

async fn relay_http_tunnel_disabled() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "relay HTTP tunnel is disabled; start termrelay with --http-tunnel to enable compatibility file APIs\n",
    )
}

async fn api_not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "relay API path not found\n")
}

async fn relay_http_tunnel(
    State(state): State<RelayState>,
    Query(auth): Query<RelayAuthQuery>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !is_http_api_tunnel_path_allowed(method.as_str(), uri.path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.authorizes(auth.relay_token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(server_id) = headers
        .get("x-termd-server-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(ServerId)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let forwarded_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect::<Vec<_>>();
    let admission = match relay_admission_from_headers(&headers) {
        Ok(admission) => admission,
        Err(status) => return status.into_response(),
    };
    match state
        .http_tunnel(
            server_id,
            method.as_str().to_owned(),
            uri.path().to_owned(),
            forwarded_headers,
            admission,
            body.into_data_stream(),
        )
        .await
    {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

fn relay_admission_from_headers(
    headers: &HeaderMap,
) -> Result<Option<RelayAdmissionPayload>, StatusCode> {
    let Some(value) = headers.get("x-termd-relay-admission") else {
        return Ok(None);
    };
    let raw = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
    // 中文注释：relay 只解析 admission 外壳，业务 auth/session 仍由 daemon 最终校验。
    serde_json::from_str(raw)
        .map(Some)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

fn is_http_api_tunnel_path_allowed(method: &str, path: &str) -> bool {
    // 中文注释：relay 只做 tunnel 前置路由，实际 bearer/E2EE/session scope 都由 daemon 校验。
    // 路由白名单复用 proto 共享函数，避免 relay 和 daemon 的外层协议面漂移。
    is_http_tunnel_path_allowed(method, path)
}

#[derive(Debug, Serialize)]
struct HealthzPayload {
    status: &'static str,
    rooms: usize,
    trusted_admission: bool,
}

async fn healthz(State(state): State<RelayState>) -> Json<HealthzPayload> {
    Json(HealthzPayload {
        status: "ok",
        rooms: state.room_count(),
        trusted_admission: state.trusted_admission_enabled(),
    })
}

async fn relay_ws(
    State(state): State<RelayState>,
    Query(auth): Query<RelayAuthQuery>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_response(state, auth, websocket)
}

#[derive(Debug, Default, Deserialize)]
struct RelayAuthQuery {
    relay_token: Option<String>,
}

fn ws_response(
    state: RelayState,
    auth: RelayAuthQuery,
    websocket: WebSocketUpgrade,
) -> axum::response::Response {
    if !state.authorizes(auth.relay_token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

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
    use axum::http::Request;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use termd_proto::{
        Envelope, MessageType, Nonce, ProtocolVersion, RelayClientId, RelayControlEnvelope,
        RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
    };
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
    use tower::ServiceExt as _;

    #[test]
    fn router_can_be_constructed() {
        let _router = router(RelayState::default(), false, false);
    }

    #[tokio::test]
    async fn router_disables_http_file_tunnel_by_default() {
        for path in [
            "/api/files/upload/init",
            "/api/files/upload",
            "/api/files/upload/abort",
            "/api/files/download",
        ] {
            let response = router(RelayState::default(), true, false)
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

            // 中文注释：即使启用了 Web fallback，文件 tunnel 默认也不能落到兼容路径或 SPA。
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("disabled response body should be readable");
            let body = std::str::from_utf8(&body).expect("disabled response should be UTF-8");
            assert!(body.contains("--http-tunnel"));
        }
    }

    #[tokio::test]
    async fn router_mounts_http_control_tunnel_even_when_file_tunnel_is_disabled() {
        for path in [
            "/api/control/session/list",
            "/api/control/session/reorder",
            "/api/control/daemon/client_forget",
        ] {
            let response = router(RelayState::default(), true, false)
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
        let response = router(RelayState::default(), true, false)
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
            let response = router(
                RelayState::new(Some("relay-secret-1".to_owned())),
                true,
                false,
            )
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
            (Method::POST, "/api/files/download/extra"),
        ] {
            let response = router(RelayState::default(), true, false)
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

        let slash_variant = router(RelayState::default(), true, false)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/files/upload/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert!(
            !slash_variant.status().is_success(),
            "POST /api/files/upload/ must not be served by Web fallback"
        );
    }

    #[tokio::test]
    async fn router_mounts_http_file_tunnel_only_when_explicitly_enabled() {
        let response = router(RelayState::default(), true, true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/files/download")
                    .header("x-termd-server-id", ServerId::new().0.to_string())
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        // 中文注释：启用后应进入 tunnel 兼容路径；没有 daemon 在线时会返回 503，而不是禁用提示。
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn router_answers_cors_preflight_for_http_file_tunnel() {
        let response = router(RelayState::default(), true, true)
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
    async fn router_answers_cors_preflight_for_http_control_tunnel() {
        let response = router(RelayState::default(), true, false)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/control/session/list")
                    .header("origin", "http://127.0.0.1:4173")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type,x-termd-server-id,x-termd-device-id,x-termd-session-scope,x-termd-e2ee-public-key,x-termd-e2ee-nonce,x-termd-e2ee-timestamp-ms,x-termd-e2ee-signature",
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
        let response = router(RelayState::default(), true, true)
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
        let disabled_response = router(RelayState::default(), false, false)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(disabled_response.status(), StatusCode::NOT_FOUND);

        let enabled_response = router(RelayState::default(), true, false)
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

    #[test]
    fn auth_state_accepts_only_matching_relay_token_when_configured() {
        let state = RelayState::new(Some("relay-secret-1".to_owned()));

        assert!(state.authorizes(Some("relay-secret-1")));
        assert!(!state.authorizes(None));
        assert!(!state.authorizes(Some("wrong-secret")));
    }

    #[tokio::test]
    async fn old_path_based_websocket_routes_are_removed() {
        let server_id = ServerId::new();

        for path in [
            format!("/ws/{}/daemon", server_id.0),
            format!("/ws/{}/daemon-mux", server_id.0),
            format!("/ws/{}/client", server_id.0),
        ] {
            let response = router(RelayState::default(), false, false)
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = ServerId::new();
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;
        let (mut client, mut daemon_data, _) =
            register_client_data_pipe(&url, server_id, &mut daemon_control).await;

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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false, false))
                .await
                .unwrap();
        });
        let server_id = ServerId::new();
        let url = format!("ws://{addr}/ws");
        let (mut daemon_control, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_route(&mut daemon_control, server_id, RouteRole::DaemonControl).await;
        let (mut target_client, mut target_data, _) =
            register_client_data_pipe(&url, server_id, &mut daemon_control).await;
        let (mut flood_client, _flood_data, _) =
            register_client_data_pipe(&url, server_id, &mut daemon_control).await;

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

    async fn register_route(socket: &mut TestSocket, server_id: ServerId, role: RouteRole) {
        let hello = route_hello(server_id, role, None, None);
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
        url: &str,
        server_id: ServerId,
        daemon_control: &mut TestSocket,
    ) -> (TestSocket, TestSocket, RelayClientId) {
        let (mut client, _client_response) = connect_async(url).await.unwrap();
        client
            .send(ClientMessage::Text(
                serde_json::to_string(&route_hello(server_id, RouteRole::Client, None, None))
                    .unwrap(),
            ))
            .await
            .unwrap();
        let (client_id, data_token) = expect_open_data(daemon_control).await;

        let (mut daemon_data, _data_response) = connect_async(url).await.unwrap();
        let data_hello = route_hello(
            server_id,
            RouteRole::DaemonData,
            Some(client_id),
            Some(data_token),
        );
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
        let client_ready: Envelope<RouteReadyPayload> =
            serde_json::from_str(&next_text(&mut client).await).unwrap();
        assert_eq!(client_ready.kind, MessageType::RouteReady);
        assert_eq!(client_ready.payload.role, RouteRole::Client);

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
            match socket.next().await.unwrap().unwrap() {
                ClientMessage::Text(text) => {
                    match serde_json::from_str::<RelayControlEnvelope>(&text)
                        .expect("relay control envelope should decode")
                    {
                        RelayControlEnvelope::OpenData {
                            client_id,
                            data_token,
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
            match socket.next().await.unwrap().unwrap() {
                ClientMessage::Text(text) => return text,
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                other => panic!("expected text frame, got {other:?}"),
            }
        }
    }

    async fn next_data_frame(socket: &mut TestSocket) -> Option<ClientMessage> {
        loop {
            match socket.next().await?.unwrap() {
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => continue,
                frame => return Some(frame),
            }
        }
    }
}
