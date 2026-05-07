use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use termd_proto::ServerId;
use uuid::Uuid;

use crate::ws::{ConnectionRole, RelayState, handle_socket};

pub fn router(state: RelayState, web_enabled: bool) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws/:server_id/daemon", get(daemon_ws))
        .route("/ws/:server_id/daemon-mux", get(daemon_mux_ws))
        .route("/ws/:server_id/client", get(client_ws))
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

async fn daemon_ws(
    Path(raw_server_id): Path<String>,
    State(state): State<RelayState>,
    Query(auth): Query<RelayAuthQuery>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_response(
        raw_server_id,
        state,
        auth,
        websocket,
        ConnectionRole::Daemon,
    )
}

async fn daemon_mux_ws(
    Path(raw_server_id): Path<String>,
    State(state): State<RelayState>,
    Query(auth): Query<RelayAuthQuery>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_response(
        raw_server_id,
        state,
        auth,
        websocket,
        ConnectionRole::DaemonMux,
    )
}

async fn client_ws(
    Path(raw_server_id): Path<String>,
    State(state): State<RelayState>,
    Query(auth): Query<RelayAuthQuery>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_response(
        raw_server_id,
        state,
        auth,
        websocket,
        ConnectionRole::Client,
    )
}

#[derive(Debug, Default, Deserialize)]
struct RelayAuthQuery {
    relay_token: Option<String>,
}

fn ws_response(
    raw_server_id: String,
    state: RelayState,
    auth: RelayAuthQuery,
    websocket: WebSocketUpgrade,
    role: ConnectionRole,
) -> axum::response::Response {
    if !state.authorizes(auth.relay_token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match parse_server_id(&raw_server_id) {
        Ok(server_id) => websocket
            .on_upgrade(move |socket| handle_socket(socket, state, server_id, role))
            .into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

pub(crate) fn parse_server_id(raw: &str) -> Result<ServerId, uuid::Error> {
    // relay 只把 server_id 当作公开路由键；不从 payload 中挖业务身份。
    Uuid::parse_str(raw).map(ServerId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tower::ServiceExt as _;

    #[test]
    fn parses_uuid_server_id_from_path() {
        let uuid = Uuid::nil();
        let server_id = parse_server_id(&uuid.to_string()).unwrap();

        assert_eq!(server_id, ServerId(uuid));
    }

    #[test]
    fn rejects_invalid_server_id_path() {
        assert!(parse_server_id("not-a-uuid").is_err());
    }

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
    async fn websocket_routes_forward_non_json_text_and_binary() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, router(RelayState::default(), false))
                .await
                .unwrap();
        });
        let server_id = Uuid::new_v4();

        let daemon_url = format!("ws://{addr}/ws/{server_id}/daemon");
        let client_url = format!("ws://{addr}/ws/{server_id}/client");
        let (mut daemon, _daemon_response) = connect_async(daemon_url).await.unwrap();
        let (mut client, _client_response) = connect_async(client_url).await.unwrap();

        // 这里故意发送非法 JSON，证明 relay 不解析业务 payload。
        daemon
            .send(ClientMessage::Text("{not-json".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            ClientMessage::Text("{not-json".to_owned())
        );

        client
            .send(ClientMessage::Binary(vec![0, 1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(
            daemon.next().await.unwrap().unwrap(),
            ClientMessage::Binary(vec![0, 1, 2, 3])
        );
    }
}
