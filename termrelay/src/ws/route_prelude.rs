use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt as _;
use termd_proto::{Envelope, ErrorPayload, MessageType, RouteHelloPayload, RouteReadyPayload};
use tokio::time::timeout;

use super::policy::{WEBSOCKET_PONG_DEADLINE, WEBSOCKET_SEND_DEADLINE, reject_oversized_frame};
use super::{ConnectionRole, RoutePrelude, RoutePreludeError};

pub(super) async fn read_route_prelude(
    socket: &mut WebSocket,
) -> Result<RoutePrelude, RoutePreludeError> {
    loop {
        let Some(message) = socket.next().await else {
            return Err(RoutePreludeError::Closed);
        };
        let message = message.map_err(RoutePreludeError::Receive)?;

        match message {
            Message::Text(raw) => {
                reject_oversized_frame(raw.len()).map_err(RoutePreludeError::TooLarge)?;
                return decode_route_prelude_from_str(&raw);
            }
            Message::Binary(raw) => {
                reject_oversized_frame(raw.len()).map_err(RoutePreludeError::TooLarge)?;
                return decode_route_prelude_from_slice(&raw);
            }
            Message::Ping(payload) => {
                timeout(WEBSOCKET_PONG_DEADLINE, socket.send(Message::Pong(payload)))
                    .await
                    .map_err(|_| RoutePreludeError::PongTimeout)?
                    .map_err(RoutePreludeError::Send)?
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(RoutePreludeError::Closed),
        }
    }
}

fn decode_route_prelude_from_str(raw: &str) -> Result<RoutePrelude, RoutePreludeError> {
    let envelope = serde_json::from_str::<Envelope<RouteHelloPayload>>(raw)?;
    decode_route_prelude(envelope)
}

fn decode_route_prelude_from_slice(raw: &[u8]) -> Result<RoutePrelude, RoutePreludeError> {
    let envelope = serde_json::from_slice::<Envelope<RouteHelloPayload>>(raw)?;
    decode_route_prelude(envelope)
}

fn decode_route_prelude(
    envelope: Envelope<RouteHelloPayload>,
) -> Result<RoutePrelude, RoutePreludeError> {
    if envelope.kind != MessageType::RouteHello {
        return Err(RoutePreludeError::UnexpectedType(envelope.kind));
    }

    // protocol_version, nonce, and timestamp_ms are carried for the protocol edge;
    // relay 只使用公开的 route 生命周期字段：server_id、role、route_generation、
    // client_id、data_token。relay 不解析任何业务密文。
    let route_role = envelope.payload.role;
    let connection_role = ConnectionRole::from_route_role(route_role)?;
    Ok(RoutePrelude {
        server_id: envelope.payload.server_id,
        route_role,
        connection_role,
        route_generation: envelope.payload.route_generation,
        client_id: envelope.payload.client_id,
        data_token: envelope.payload.data_token,
    })
}

pub(super) async fn send_route_ready(
    socket: &mut WebSocket,
    prelude: &RoutePrelude,
) -> Result<(), RoutePreludeError> {
    let ready = Envelope::new(
        MessageType::RouteReady,
        RouteReadyPayload {
            server_id: prelude.server_id,
            role: prelude.route_role,
        },
    );
    let raw = serde_json::to_string(&ready)?;
    socket
        .send(Message::Text(raw))
        .await
        .map_err(RoutePreludeError::Send)
}

pub(super) async fn send_route_error(
    socket: &mut WebSocket,
    code: &'static str,
    message: &'static str,
) -> Result<(), RoutePreludeError> {
    let error = Envelope::new(
        MessageType::Error,
        ErrorPayload {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    );
    let raw = serde_json::to_string(&error)?;
    timeout(WEBSOCKET_SEND_DEADLINE, socket.send(Message::Text(raw)))
        .await
        .map_err(|_| RoutePreludeError::SendTimeout)?
        .map_err(RoutePreludeError::Send)
}
