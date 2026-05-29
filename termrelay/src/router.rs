use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::ws::{RelayState, WEBSOCKET_MAX_FRAME_SIZE, WEBSOCKET_MAX_MESSAGE_SIZE, handle_socket};

pub fn router(state: RelayState, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(relay_ws))
        .with_state(state);

    if web_enabled {
        router.fallback(termweb::embedded_web_handler)
    } else {
        router
    }
}

#[derive(Debug, Serialize)]
struct HealthzPayload {
    status: &'static str,
    rooms: usize,
}

async fn healthz(State(state): State<RelayState>) -> Json<HealthzPayload> {
    Json(HealthzPayload {
        status: "ok",
        rooms: state.room_count(),
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
        let _router = router(RelayState::default(), false);
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
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
            axum::serve(listener, router(RelayState::default(), false))
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
        Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role,
                protocol_version: ProtocolVersion::default(),
                nonce: Nonce("route-test-nonce".to_owned()),
                route_generation: None,
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
