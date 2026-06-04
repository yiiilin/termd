use axum::extract::ws::WebSocket;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::policy::{ROUTE_PRELUDE_TIMEOUT, WEBSOCKET_SEND_DEADLINE};
use super::route_prelude::{read_route_prelude, send_route_error, send_route_ready};
use super::{ConnectionRegistration, ConnectionRole, FrameSender, RelayState, RoutePreludeError};

pub(super) struct BoundSocketRoute {
    pub(super) server_id: termd_proto::ServerId,
    pub(super) role: ConnectionRole,
    pub(super) registration: ConnectionRegistration,
}

pub(super) async fn bind_socket_route(
    socket: &mut WebSocket,
    state: &RelayState,
    sender: FrameSender,
) -> Option<BoundSocketRoute> {
    // 中文注释：绑定阶段只做 transport route 生命周期接线，不进入后续 opaque frame 泵送。
    let prelude = match timeout(ROUTE_PRELUDE_TIMEOUT, read_route_prelude(socket)).await {
        Ok(Ok(prelude)) => prelude,
        Ok(Err(error)) => {
            if matches!(error, RoutePreludeError::UnsupportedLegacyDaemonMux) {
                let _ = send_route_error(
                    socket,
                    "relay_legacy_route_rejected",
                    "legacy daemon mux route is no longer accepted; reconnect with daemon control and daemon data routes",
                )
                .await;
            }
            if route_prelude_error_is_noisy_client_disconnect(&error) {
                debug!(%error, "rejecting relay websocket before route registration");
            } else {
                warn!(%error, "rejecting relay websocket before route registration");
            }
            return None;
        }
        Err(_) => {
            warn!(
                timeout_ms = ROUTE_PRELUDE_TIMEOUT.as_millis(),
                "relay route prelude timed out"
            );
            return None;
        }
    };

    let server_id = prelude.server_id;
    let role = prelude.connection_role;
    let registration = match state.register_route(&prelude, sender) {
        Ok(registration) => registration,
        Err(error) => {
            warn!(server_id = %server_id.0, ?role, %error, "rejecting relay websocket");
            let _ = send_route_error(
                socket,
                error.route_error_code(),
                error.route_error_message(),
            )
            .await;
            return None;
        }
    };
    state.start_pending_client_pair_deadline(&registration);

    if role == ConnectionRole::Client {
        debug!(
            server_id = %server_id.0,
            connection_id = registration.id,
            "relay client route accepted before daemon data pipe paired"
        );
    }

    match timeout(WEBSOCKET_SEND_DEADLINE, send_route_ready(socket, &prelude)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(
                server_id = %server_id.0,
                ?role,
                %error,
                "relay websocket route_ready failed"
            );
            state.unregister(&registration);
            return None;
        }
        Err(_) => {
            warn!(
                server_id = %server_id.0,
                ?role,
                timeout_ms = WEBSOCKET_SEND_DEADLINE.as_millis(),
                "relay websocket route_ready timed out"
            );
            state.unregister(&registration);
            return None;
        }
    }

    debug!(
        server_id = %server_id.0,
        ?role,
        connection_id = registration.id,
        "relay websocket registered"
    );

    Some(BoundSocketRoute {
        server_id,
        role,
        registration,
    })
}

pub(super) fn route_prelude_error_is_noisy_client_disconnect(error: &RoutePreludeError) -> bool {
    match error {
        RoutePreludeError::Closed => true,
        RoutePreludeError::Receive(receive_error) => receive_error
            .to_string()
            .contains("Connection reset without closing handshake"),
        _ => false,
    }
}
