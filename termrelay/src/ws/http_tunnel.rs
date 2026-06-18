use std::io;
use std::time::Duration;

use axum::body::{Body, BodyDataStream, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use termd_proto::{
    RelayHttpTunnelFrame, decode_relay_http_tunnel_frame, encode_relay_http_tunnel_request_body,
    encode_relay_http_tunnel_request_end, encode_relay_http_tunnel_request_head,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::policy::ROUTE_PRELUDE_TIMEOUT;
use super::{
    ConnectionRegistration, ConnectionRole, DATA_CHANNEL_CAPACITY,
    HTTP_TUNNEL_BODY_CHANNEL_CAPACITY, HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT, OpaqueFrame,
    PipePump, PumpDataReceiver, RelayError, RelayOutbound, RelayState, RoutePrelude,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelayHttpTunnelRequestBodyDeadline {
    None,
    FirstChunk(Duration),
}

impl RelayState {
    pub async fn http_tunnel(
        &self,
        server_id: termd_proto::ServerId,
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: BodyDataStream,
    ) -> Result<Response, StatusCode> {
        let request_body_deadline = relay_http_tunnel_request_body_deadline(&method);
        let request_head =
            encode_relay_http_tunnel_request_head(method.clone(), path.clone(), headers)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
        let pipe_pump = PipePump::new(DATA_CHANNEL_CAPACITY);
        let sender = pipe_pump.sender();
        let prelude = RoutePrelude {
            server_id,
            route_role: termd_proto::RouteRole::Client,
            connection_role: ConnectionRole::Client,
            route_generation: None,
            client_id: None,
            data_token: None,
        };
        let registration = self
            .register_route(&prelude, sender)
            .map_err(|error| match error {
                RelayError::DaemonControlOffline => StatusCode::SERVICE_UNAVAILABLE,
                RelayError::DaemonControlBusy | RelayError::PendingClientLimitExceeded => {
                    StatusCode::TOO_MANY_REQUESTS
                }
                _ => StatusCode::BAD_GATEWAY,
            })?;
        self.start_pending_client_pair_deadline(&registration);
        debug!(
            server_id = %server_id.0,
            client_connection_id = registration.id,
            method = %method,
            path = %path,
            "relay HTTP tunnel registered synthetic client"
        );
        let mut data_rx = pipe_pump.into_data_receiver();
        let mut registration_guard =
            RelayHttpTunnelRegistrationGuard::new(self.clone(), registration);
        if timeout(
            ROUTE_PRELUDE_TIMEOUT,
            self.wait_client_data_pair(registration_guard.registration()),
        )
        .await
        .ok()
            != Some(true)
        {
            warn!(
                layer = "relay",
                phase = "http_tunnel_wait_data_pair",
                timeout_code = "route_prelude_timeout",
                server_id = %server_id.0,
                client_connection_id = registration_guard.registration().id,
                method = %method,
                path = %path,
                timeout_ms = ROUTE_PRELUDE_TIMEOUT.as_millis(),
                "relay HTTP tunnel timed out waiting for data pair"
            );
            return Err(StatusCode::GATEWAY_TIMEOUT);
        }
        let report = self
            .forward_from(
                registration_guard.registration(),
                OpaqueFrame::Binary(request_head),
            )
            .await;
        if report.delivered == 0 {
            warn!(
                server_id = %server_id.0,
                client_connection_id = registration_guard.registration().id,
                method = %method,
                path = %path,
                ?report,
                "relay HTTP tunnel failed to forward request head"
            );
            return Err(StatusCode::BAD_GATEWAY);
        }
        debug!(
            server_id = %server_id.0,
            client_connection_id = registration_guard.registration().id,
            method = %method,
            path = %path,
            "relay HTTP tunnel forwarded request head"
        );

        let request_state = self.clone();
        let request_registration = registration_guard.registration().clone();
        let request_method = method.clone();
        let request_path = path.clone();
        let (request_result_tx, mut request_result_rx) =
            tokio::sync::oneshot::channel::<Result<(), StatusCode>>();
        let request_task = tokio::spawn(async move {
            let result = relay_http_tunnel_forward_request_body(
                request_state,
                request_registration,
                body,
                request_body_deadline,
            )
            .await;
            if let Err(status) = result {
                warn!(
                    method = %request_method,
                    path = %request_path,
                    status = status.as_u16(),
                    "relay HTTP tunnel request body forwarding failed"
                );
            }
            let _ = request_result_tx.send(result);
        });
        registration_guard.set_request_task(request_task);

        let mut request_done = false;
        loop {
            tokio::select! {
                biased;

                request_result = &mut request_result_rx, if !request_done => {
                    request_done = true;
                    match request_result {
                        Ok(Ok(())) => {}
                        Ok(Err(status)) => {
                            return Err(status);
                        }
                        Err(_) => {
                            warn!(
                                server_id = %server_id.0,
                                client_connection_id = registration_guard.registration().id,
                                method = %method,
                                path = %path,
                                "relay HTTP tunnel request body task dropped"
                            );
                            return Err(StatusCode::BAD_GATEWAY);
                        }
                    }
                    continue;
                }
                outbound = data_rx.recv() => {
                    let Some(outbound) = outbound else {
                        warn!(
                            server_id = %server_id.0,
                            client_connection_id = registration_guard.registration().id,
                            method = %method,
                            path = %path,
                            "relay HTTP tunnel data pipe closed before response head"
                        );
                        break;
                    };
                    if let RelayOutbound::Frame(OpaqueFrame::Binary(raw)) = outbound
                        && let Some(RelayHttpTunnelFrame::ResponseHead { status }) =
                            decode_relay_http_tunnel_frame(&raw)
                    {
                        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                        debug!(
                            server_id = %server_id.0,
                            client_connection_id = registration_guard.registration().id,
                            method = %method,
                            path = %path,
                            status = status.as_u16(),
                            "relay HTTP tunnel received response head"
                        );
                        let (body_tx, body_rx) =
                            mpsc::channel::<Result<Bytes, io::Error>>(HTTP_TUNNEL_BODY_CHANNEL_CAPACITY);
                        let response_state = self.clone();
                        let response_registration = registration_guard.registration().clone();
                        let request_result_rx = (!request_done).then_some(request_result_rx);
                        let request_task = registration_guard.take_request_task();
                        registration_guard.disarm();
                        tokio::spawn(relay_http_tunnel_forward_response_body(
                            response_state,
                            response_registration,
                            data_rx,
                            body_tx,
                            request_result_rx,
                            request_task,
                        ));
                        let body_stream = futures_util::stream::unfold(body_rx, |mut body_rx| async move {
                            body_rx.recv().await.map(|item| (item, body_rx))
                        });
                        return Ok((status, Body::from_stream(body_stream)).into_response());
                    }
                }
            }
        }
        Err(StatusCode::BAD_GATEWAY)
    }
}

struct RelayHttpTunnelRegistrationGuard {
    state: RelayState,
    registration: Option<ConnectionRegistration>,
    request_task: Option<JoinHandle<()>>,
}

impl RelayHttpTunnelRegistrationGuard {
    fn new(state: RelayState, registration: ConnectionRegistration) -> Self {
        Self {
            state,
            registration: Some(registration),
            request_task: None,
        }
    }

    fn registration(&self) -> &ConnectionRegistration {
        self.registration
            .as_ref()
            .expect("HTTP tunnel registration guard must be armed")
    }

    fn set_request_task(&mut self, task: JoinHandle<()>) {
        self.request_task = Some(task);
    }

    fn take_request_task(&mut self) -> JoinHandle<()> {
        self.request_task
            .take()
            .expect("HTTP tunnel request task must exist after response head")
    }

    fn disarm(&mut self) {
        self.registration = None;
    }
}

impl Drop for RelayHttpTunnelRegistrationGuard {
    fn drop(&mut self) {
        if let Some(task) = self.request_task.take() {
            task.abort();
        }
        if let Some(registration) = self.registration.take() {
            // 中文注释：HTTP handler future 可能在 response head 前被 axum 取消；
            // Drop guard 覆盖这段窗口，确保 synthetic client 不会留在 relay room 中。
            self.state.unregister(&registration);
        }
    }
}

pub(super) async fn relay_http_tunnel_forward_request_body(
    state: RelayState,
    registration: ConnectionRegistration,
    body: BodyDataStream,
    deadline: RelayHttpTunnelRequestBodyDeadline,
) -> Result<(), StatusCode> {
    match deadline {
        RelayHttpTunnelRequestBodyDeadline::FirstChunk(deadline) => {
            relay_http_tunnel_forward_request_body_inner(state, registration, body, Some(deadline))
                .await
        }
        RelayHttpTunnelRequestBodyDeadline::None => {
            relay_http_tunnel_forward_request_body_inner(state, registration, body, None).await
        }
    }
}

async fn relay_http_tunnel_forward_request_body_inner(
    state: RelayState,
    registration: ConnectionRegistration,
    mut body: BodyDataStream,
    first_chunk_deadline: Option<Duration>,
) -> Result<(), StatusCode> {
    let mut first_chunk = true;
    let mut chunk_count = 0_usize;
    let mut forwarded_bytes = 0_usize;
    loop {
        let next = if first_chunk {
            first_chunk = false;
            if let Some(deadline) = first_chunk_deadline {
                timeout(deadline, body.next())
                    .await
                    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            } else {
                body.next().await
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if chunk.is_empty() {
            continue;
        }
        chunk_count = chunk_count.saturating_add(1);
        forwarded_bytes = forwarded_bytes.saturating_add(chunk.len());
        let report = state
            .forward_http_request_from(
                &registration,
                OpaqueFrame::Binary(encode_relay_http_tunnel_request_body(chunk.to_vec())),
            )
            .await;
        if report.delivered == 0 {
            warn!(
                server_id = %registration.server_id.0,
                client_connection_id = registration.id,
                chunk_count,
                forwarded_bytes,
                ?report,
                "relay HTTP tunnel failed to forward request body chunk"
            );
            return Err(StatusCode::BAD_GATEWAY);
        }
    }
    let report = state
        .forward_http_request_from(
            &registration,
            OpaqueFrame::Binary(encode_relay_http_tunnel_request_end()),
        )
        .await;
    if report.delivered == 0 {
        warn!(
            server_id = %registration.server_id.0,
            client_connection_id = registration.id,
            chunk_count,
            forwarded_bytes,
            ?report,
            "relay HTTP tunnel failed to forward request end"
        );
        return Err(StatusCode::BAD_GATEWAY);
    }
    debug!(
        server_id = %registration.server_id.0,
        client_connection_id = registration.id,
        chunk_count,
        forwarded_bytes,
        "relay HTTP tunnel forwarded complete request body"
    );
    Ok(())
}

pub(super) fn relay_http_tunnel_request_body_deadline(
    method: &str,
) -> RelayHttpTunnelRequestBodyDeadline {
    // 中文注释：relay 只按通用 HTTP tunnel 传输规则限时，不识别文件 API path 或业务语义。
    if method.eq_ignore_ascii_case("POST")
        || method.eq_ignore_ascii_case("PUT")
        || method.eq_ignore_ascii_case("PATCH")
    {
        RelayHttpTunnelRequestBodyDeadline::FirstChunk(HTTP_TUNNEL_SHORT_REQUEST_BODY_TIMEOUT)
    } else {
        RelayHttpTunnelRequestBodyDeadline::None
    }
}

async fn relay_http_tunnel_forward_response_body(
    state: RelayState,
    registration: ConnectionRegistration,
    mut data_rx: PumpDataReceiver,
    body_tx: mpsc::Sender<Result<Bytes, io::Error>>,
    mut request_result_rx: Option<tokio::sync::oneshot::Receiver<Result<(), StatusCode>>>,
    request_task: JoinHandle<()>,
) {
    let mut clean_shutdown = false;
    loop {
        tokio::select! {
            biased;

            request_result = async {
                match request_result_rx.as_mut() {
                    Some(rx) => rx.await.ok(),
                    None => None,
                }
            }, if request_result_rx.is_some() => {
                request_result_rx = None;
                if !matches!(request_result, Some(Ok(()))) {
                    let _ = body_tx.send(Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "relay HTTP request body forwarding failed",
                    ))).await;
                    break;
                }
            }
            _ = body_tx.closed() => {
                // 中文注释：浏览器拿到 ResponseHead 后可能立刻关闭 body；不能等 daemon
                // 再发 ResponseBody/End 才清理 synthetic client 和 data pipe。
                break;
            }
            outbound = data_rx.recv() => {
                let Some(outbound) = outbound else {
                    break;
                };
                let RelayOutbound::Frame(OpaqueFrame::Binary(raw)) = outbound else {
                    continue;
                };
                match decode_relay_http_tunnel_frame(&raw) {
                    Some(RelayHttpTunnelFrame::ResponseBody { body }) => {
                        let send_result = body_tx.send(Ok(Bytes::from(body))).await;
                        if send_result.is_err() {
                            break;
                        }
                    }
                    Some(RelayHttpTunnelFrame::ResponseEnd) => {
                        clean_shutdown = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    if !clean_shutdown {
        let _ = body_tx
            .send(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay HTTP response stream ended early",
            )))
            .await;
    }
    request_task.abort();
    state.unregister(&registration);
}
