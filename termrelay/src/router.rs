use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::ws::{RelayState, handle_socket};

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
        .on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use futures_util::{SinkExt, StreamExt};
    use termd_proto::{
        Envelope, MessageType, Nonce, ProtocolVersion, RelayMuxEnvelope, RelayOpaqueFrame,
        RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
    };
    use tokio::net::TcpListener;
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
    async fn websocket_route_prelude_forwards_non_json_text_and_binary() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
                .await
                .unwrap();
        });
        let server_id = ServerId::new();

        let url = format!("ws://{addr}/ws");
        let (mut daemon_mux, _daemon_response) = connect_async(url.clone()).await.unwrap();
        register_route(&mut daemon_mux, server_id, RouteRole::DaemonMux).await;
        let (mut client, _client_response) = connect_async(url).await.unwrap();
        register_route(&mut client, server_id, RouteRole::Client).await;
        let client_id = match next_mux(&mut daemon_mux).await {
            RelayMuxEnvelope::ClientConnected { client_id } => client_id,
            other => panic!("expected client_connected envelope, got {other:?}"),
        };

        // The post-prelude frame is intentionally invalid JSON; relay must keep it opaque.
        client
            .send(ClientMessage::Text("{not-json".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            next_mux(&mut daemon_mux).await,
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: RelayOpaqueFrame::Text {
                    data: "{not-json".to_owned(),
                },
            }
        );

        let response = RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AAECAw==".to_owned(),
            },
        };
        daemon_mux
            .send(ClientMessage::Text(
                serde_json::to_string(&response).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            ClientMessage::Binary(vec![0, 1, 2, 3])
        );
    }

    type TestSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn register_route(socket: &mut TestSocket, server_id: ServerId, role: RouteRole) {
        let hello = Envelope::new(
            MessageType::RouteHello,
            RouteHelloPayload {
                server_id,
                role,
                protocol_version: ProtocolVersion::default(),
                nonce: Nonce("route-test-nonce".to_owned()),
                timestamp_ms: termd_proto::UnixTimestampMillis(1_710_000_000_000),
            },
        );
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

    async fn next_text(socket: &mut TestSocket) -> String {
        match socket.next().await.unwrap().unwrap() {
            ClientMessage::Text(text) => text,
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    async fn next_mux(socket: &mut TestSocket) -> RelayMuxEnvelope {
        serde_json::from_str(&next_text(socket).await).unwrap()
    }
}
