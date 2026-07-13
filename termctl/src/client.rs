//! termd v0.7 client transport.
//!
//! Authentication and control use JSON HTTP. Metadata and terminal traffic use
//! separate WebSockets authenticated by a short-lived access token.

use std::collections::VecDeque;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use termd::auth::AccessTokenProofInput;
use termd::pty::PtyTerminalFrame;
use termd_proto::{
    AuthChallengePayload, AuthPayload, ControlGrantPayload, DeviceId, PairAcceptPayload, PublicKey,
    ServerId, SessionAttachPayload, SessionAttachedPayload, SessionClosedPayload,
    SessionCreatePayload, SessionCreatedPayload, SessionId, SessionListResultPayload, Signature,
    TerminalSize, UnixTimestampMillis,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::{DeviceState, PairedServerState};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINAL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_SUPERVISOR_FRAME_BYTES: usize = 8 * 1024 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct PairResult {
    pub accepted: PairAcceptPayload,
    pub device_certificate: String,
}

#[derive(Debug, Clone)]
pub struct TerminalStream {
    last_terminal_seq: Option<u64>,
    pending_events: VecDeque<TerminalStreamEvent>,
}

impl TerminalStream {
    fn new() -> Self {
        Self {
            last_terminal_seq: None,
            pending_events: VecDeque::new(),
        }
    }

    pub fn last_terminal_seq(&self) -> Option<u64> {
        self.last_terminal_seq
    }

    pub fn drain_pending_events(&mut self) -> Vec<TerminalStreamEvent> {
        self.pending_events.drain(..).collect()
    }

    pub fn prepend_pending_events(&mut self, events: Vec<TerminalStreamEvent>) {
        for event in events.into_iter().rev() {
            self.pending_events.push_front(event);
        }
    }

    fn record_terminal_seq(&mut self, seq: u64) {
        self.last_terminal_seq = Some(self.last_terminal_seq.unwrap_or(0).max(seq));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStreamEvent {
    Output(Vec<u8>),
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalAttachOptions {
    pub watch_updates: bool,
    pub last_terminal_seq: Option<u64>,
}

pub struct DirectClient {
    url: String,
    server_id: ServerId,
    device_id: DeviceId,
    access_token: String,
    http: reqwest::Client,
    terminal: Option<WsStream>,
}

pub async fn pair_device(
    url: &str,
    server_id: ServerId,
    daemon_public_key: PublicKey,
    device: &DeviceState,
    token: String,
) -> Result<PairResult> {
    #[derive(Deserialize)]
    struct PairResponse {
        server_id: ServerId,
        device_id: DeviceId,
        device_certificate: String,
    }

    let http = reqwest::Client::new();
    let response = timeout(
        REQUEST_TIMEOUT,
        http.post(application_http_url(url, "/api/auth/pair")?)
            .header("authorization", format!("TermdPair {token}"))
            .header("x-termd-server-id", server_id.0.to_string())
            .json(&json!({
                "device_id": device.device_id,
                "device_public_key": device.device_public_key,
            }))
            .send(),
    )
    .await
    .map_err(|_| TermctlError::ConnectFailed)?
    .map_err(|_| TermctlError::ConnectFailed)?;
    let paired: PairResponse = decode_http_response(response).await?;
    if paired.server_id != server_id || paired.device_id != device.device_id {
        return Err(TermctlError::PairingPayloadServerMismatch);
    }
    Ok(PairResult {
        accepted: PairAcceptPayload {
            server_id,
            daemon_public_key,
            device_id: paired.device_id,
            expires_at_ms: UnixTimestampMillis(u64::MAX),
        },
        device_certificate: paired.device_certificate,
    })
}

impl DirectClient {
    pub async fn connect_authenticated(
        url: &str,
        device: &DeviceState,
        server: &PairedServerState,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        let certificate = server.device_certificate.as_deref().ok_or_else(|| {
            protocol_error(
                "device_certificate_required",
                "pair this device again to upgrade its credential",
            )
        })?;
        let http = reqwest::Client::new();
        let headers_server_id = server.server_id.0.to_string();
        let challenge_response = timeout(
            REQUEST_TIMEOUT,
            http.post(application_http_url(url, "/api/auth/challenge")?)
                .header("authorization", format!("TermdDevice {certificate}"))
                .header("x-termd-server-id", &headers_server_id)
                .json(&json!({"device_id": device.device_id}))
                .send(),
        )
        .await
        .map_err(|_| TermctlError::AuthChallengeTimeout)?
        .map_err(|_| TermctlError::ConnectFailed)?;
        let challenge: AuthChallengePayload = decode_http_response(challenge_response).await?;
        if challenge.device_id != device.device_id {
            return Err(TermctlError::InvalidEnvelope);
        }

        let mut proof = AuthPayload {
            device_id: device.device_id,
            challenge: challenge.challenge,
            nonce: crypto::nonce(),
            timestamp_ms: crypto::now_ms(),
            signature: Signature(String::new()),
        };
        proof.signature = crypto::sign_to_wire(
            signing_key,
            &AccessTokenProofInput {
                server_id: server.server_id,
                payload: &proof,
            }
            .to_bytes(),
        );

        #[derive(Deserialize)]
        struct AccessTokenResponse {
            access_token: String,
        }
        let access_response = timeout(
            REQUEST_TIMEOUT,
            http.post(application_http_url(url, "/api/auth/access-token")?)
                .header("authorization", format!("TermdDevice {certificate}"))
                .header("x-termd-server-id", &headers_server_id)
                .json(&proof)
                .send(),
        )
        .await
        .map_err(|_| TermctlError::AuthChallengeTimeout)?
        .map_err(|_| TermctlError::ConnectFailed)?;
        let access: AccessTokenResponse = decode_http_response(access_response).await?;

        Ok(Self {
            url: url.to_owned(),
            server_id: server.server_id,
            device_id: device.device_id,
            access_token: access.access_token,
            http,
            terminal: None,
        })
    }

    pub async fn create_session(
        &mut self,
        command: Vec<String>,
        size: TerminalSize,
    ) -> Result<SessionCreatedPayload> {
        let socket = self.open_terminal_socket().await?;
        self.terminal = Some(socket);
        self.send_terminal_json("terminal.create", SessionCreatePayload { command, size })
            .await?;
        let created = self
            .receive_terminal_open::<SessionCreatedPayload>("terminal.created")
            .await?;
        self.receive_terminal_snapshot().await?;
        Ok(created)
    }

    pub async fn attach_terminal_stream_with_options(
        &mut self,
        session_id: SessionId,
        options: TerminalAttachOptions,
    ) -> Result<(SessionAttachedPayload, TerminalStream)> {
        let socket = self.open_terminal_socket().await?;
        self.terminal = Some(socket);
        self.send_terminal_json(
            "terminal.attach",
            SessionAttachPayload {
                session_id,
                watch_updates: options.watch_updates,
                last_terminal_seq: options.last_terminal_seq,
            },
        )
        .await?;
        let attached = self
            .receive_terminal_open::<SessionAttachedPayload>("terminal.attached")
            .await?;
        self.receive_terminal_snapshot().await?;
        Ok((attached, TerminalStream::new()))
    }

    pub async fn request_control(&mut self, session_id: SessionId) -> Result<ControlGrantPayload> {
        self.post_json(
            &format!("/api/control/session/{}/control", session_id.0),
            &json!({ "device_id": self.device_id }),
        )
        .await
    }

    pub async fn resize_session(
        &mut self,
        _session_id: SessionId,
        size: TerminalSize,
    ) -> Result<()> {
        self.send_supervisor_frame(json!({"type": "resize", "size": size}))
            .await?;
        timeout(REQUEST_TIMEOUT, self.wait_for_resize(size))
            .await
            .map_err(|_| TermctlError::ReceiveFailed)?
    }

    pub async fn list_sessions(&mut self) -> Result<SessionListResultPayload> {
        #[derive(Deserialize)]
        struct MetadataPayload {
            state: MetadataState,
        }
        #[derive(Deserialize)]
        struct MetadataState {
            sessions: Vec<termd_proto::SessionSummaryPayload>,
        }
        let mut metadata = self.open_workspace_socket("metadata").await?;
        loop {
            let message = timeout(REQUEST_TIMEOUT, metadata.next())
                .await
                .map_err(|_| TermctlError::ReceiveFailed)?
                .ok_or(TermctlError::ConnectionClosed)?
                .map_err(|_| TermctlError::ReceiveFailed)?;
            match message {
                Message::Text(raw) => {
                    let envelope: V070Envelope<Value> =
                        serde_json::from_str(&raw).map_err(|_| TermctlError::InvalidEnvelope)?;
                    if envelope.kind == "error" {
                        return Err(socket_error(envelope.payload));
                    }
                    if envelope.kind == "metadata.snapshot" {
                        let payload: MetadataPayload = serde_json::from_value(envelope.payload)
                            .map_err(|_| TermctlError::InvalidEnvelope)?;
                        let _ = metadata.close(None).await;
                        return Ok(SessionListResultPayload {
                            sessions: payload.state.sessions,
                        });
                    }
                }
                Message::Ping(bytes) => metadata
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| TermctlError::SendFailed)?,
                Message::Close(_) => return Err(TermctlError::ConnectionClosed),
                _ => {}
            }
        }
    }

    pub async fn close_session(&mut self, session_id: SessionId) -> Result<SessionClosedPayload> {
        self.post_json(
            &format!("/api/control/session/{}/close", session_id.0),
            &json!({}),
        )
        .await
    }

    pub async fn send_terminal_data(
        &mut self,
        _stream: &mut TerminalStream,
        _session_id: SessionId,
        bytes: &[u8],
    ) -> Result<()> {
        self.send_supervisor_frame(json!({
            "type": "input",
            "data": STANDARD.encode(bytes),
        }))
        .await
    }

    pub async fn cancel_terminal_stream(&mut self, _stream: &TerminalStream) -> Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            terminal
                .close(None)
                .await
                .map_err(|_| TermctlError::SendFailed)?;
        }
        Ok(())
    }

    pub async fn receive_terminal_event(
        &mut self,
        stream: &mut TerminalStream,
    ) -> Result<TerminalStreamEvent> {
        if let Some(event) = stream.pending_events.pop_front() {
            return Ok(event);
        }
        loop {
            let message = self
                .terminal
                .as_mut()
                .ok_or(TermctlError::ConnectionClosed)?
                .next()
                .await
                .ok_or(TermctlError::ConnectionClosed)?
                .map_err(|_| TermctlError::ReceiveFailed)?;
            match message {
                Message::Binary(bytes) => {
                    let frame = decode_supervisor_frame(&bytes)?;
                    if let SupervisorServerFrame::HeartbeatPing { nonce } = &frame {
                        self.send_supervisor_frame(json!({
                            "type": "heartbeat_pong",
                            "nonce": nonce,
                        }))
                        .await?;
                        continue;
                    }
                    queue_supervisor_events(stream, frame)?;
                    if let Some(event) = stream.pending_events.pop_front() {
                        return Ok(event);
                    }
                }
                Message::Text(raw) => {
                    let envelope: V070Envelope<Value> =
                        serde_json::from_str(&raw).map_err(|_| TermctlError::InvalidEnvelope)?;
                    if envelope.kind == "error" {
                        return Err(socket_error(envelope.payload));
                    }
                }
                Message::Ping(bytes) => self
                    .terminal
                    .as_mut()
                    .ok_or(TermctlError::ConnectionClosed)?
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| TermctlError::SendFailed)?,
                Message::Close(_) => return Ok(TerminalStreamEvent::End),
                _ => {}
            }
        }
    }

    async fn open_terminal_socket(&self) -> Result<WsStream> {
        self.open_workspace_socket("terminal").await
    }

    async fn open_workspace_socket(&self, kind: &str) -> Result<WsStream> {
        let url = application_ws_url(&self.url, kind)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| TermctlError::InvalidWsUrl)?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&format!("termd.v0.7, {}", self.access_token))
                .map_err(|_| TermctlError::InvalidEnvelope)?,
        );
        request.headers_mut().insert(
            "x-termd-server-id",
            HeaderValue::from_str(&self.server_id.0.to_string())
                .map_err(|_| TermctlError::InvalidEnvelope)?,
        );
        let (socket, _) = timeout(
            TERMINAL_OPEN_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| TermctlError::ConnectFailed)?
        .map_err(|_| TermctlError::ConnectFailed)?;
        Ok(socket)
    }

    async fn send_terminal_json<T: Serialize>(&mut self, kind: &str, payload: T) -> Result<()> {
        let raw = serde_json::to_string(&json!({"type": kind, "payload": payload}))
            .map_err(|_| TermctlError::InvalidEnvelope)?;
        self.terminal
            .as_mut()
            .ok_or(TermctlError::ConnectionClosed)?
            .send(Message::Text(raw))
            .await
            .map_err(|_| TermctlError::SendFailed)
    }

    async fn receive_terminal_open<T: DeserializeOwned>(&mut self, expected: &str) -> Result<T> {
        loop {
            let message = timeout(
                TERMINAL_OPEN_TIMEOUT,
                self.terminal
                    .as_mut()
                    .ok_or(TermctlError::ConnectionClosed)?
                    .next(),
            )
            .await
            .map_err(|_| TermctlError::ReceiveFailed)?
            .ok_or(TermctlError::ConnectionClosed)?
            .map_err(|_| TermctlError::ReceiveFailed)?;
            if let Message::Text(raw) = message {
                let envelope: V070Envelope<Value> =
                    serde_json::from_str(&raw).map_err(|_| TermctlError::InvalidEnvelope)?;
                if envelope.kind == "error" {
                    return Err(socket_error(envelope.payload));
                }
                if envelope.kind == expected {
                    return serde_json::from_value(envelope.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope);
                }
            }
        }
    }

    async fn receive_terminal_snapshot(&mut self) -> Result<()> {
        let _: Value = self.receive_terminal_open("terminal.snapshot").await?;
        Ok(())
    }

    async fn send_supervisor_frame(&mut self, payload: Value) -> Result<()> {
        let json = serde_json::to_vec(&payload).map_err(|_| TermctlError::InvalidEnvelope)?;
        if json.len() > MAX_SUPERVISOR_FRAME_BYTES {
            return Err(TermctlError::InvalidEnvelope);
        }
        let len = u32::try_from(json.len()).map_err(|_| TermctlError::InvalidEnvelope)?;
        let mut frame = Vec::with_capacity(4 + json.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&json);
        self.terminal
            .as_mut()
            .ok_or(TermctlError::ConnectionClosed)?
            .send(Message::Binary(frame))
            .await
            .map_err(|_| TermctlError::SendFailed)
    }

    async fn wait_for_resize(&mut self, expected: TerminalSize) -> Result<()> {
        loop {
            let message = self
                .terminal
                .as_mut()
                .ok_or(TermctlError::ConnectionClosed)?
                .next()
                .await
                .ok_or(TermctlError::ConnectionClosed)?
                .map_err(|_| TermctlError::ReceiveFailed)?;
            match message {
                Message::Binary(bytes) => {
                    let frame = decode_supervisor_frame(&bytes)?;
                    if supervisor_frame_has_resize(&frame, expected) {
                        return Ok(());
                    }
                    if let SupervisorServerFrame::HeartbeatPing { nonce } = &frame {
                        self.send_supervisor_frame(json!({
                            "type": "heartbeat_pong",
                            "nonce": nonce,
                        }))
                        .await?;
                    }
                    if matches!(frame, SupervisorServerFrame::Close) {
                        return Err(TermctlError::ConnectionClosed);
                    }
                }
                Message::Text(raw) => {
                    let envelope: V070Envelope<Value> =
                        serde_json::from_str(&raw).map_err(|_| TermctlError::InvalidEnvelope)?;
                    if envelope.kind == "error" {
                        return Err(socket_error(envelope.payload));
                    }
                }
                Message::Ping(bytes) => self
                    .terminal
                    .as_mut()
                    .ok_or(TermctlError::ConnectionClosed)?
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| TermctlError::SendFailed)?,
                Message::Close(_) => return Err(TermctlError::ConnectionClosed),
                _ => {}
            }
        }
    }

    async fn post_json<T: DeserializeOwned, P: Serialize>(
        &self,
        path: &str,
        payload: &P,
    ) -> Result<T> {
        let response = timeout(
            REQUEST_TIMEOUT,
            self.http
                .post(application_http_url(&self.url, path)?)
                .bearer_auth(&self.access_token)
                .header("x-termd-server-id", self.server_id.0.to_string())
                .json(payload)
                .send(),
        )
        .await
        .map_err(|_| TermctlError::ConnectFailed)?
        .map_err(|_| TermctlError::ConnectFailed)?;
        decode_http_response(response).await
    }
}

#[derive(Deserialize)]
struct V070Envelope<T> {
    #[serde(rename = "type")]
    kind: String,
    payload: T,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorServerFrame {
    AttachSync {
        base_seq: u64,
        snapshot: SupervisorSnapshot,
        frames: Vec<PtyTerminalFrame>,
    },
    TerminalFrame {
        frame: PtyTerminalFrame,
    },
    HeartbeatPing {
        nonce: String,
    },
    Close,
}

#[derive(Deserialize)]
struct SupervisorSnapshot {
    #[serde(default)]
    retained_output: String,
}

fn decode_supervisor_frame(bytes: &[u8]) -> Result<SupervisorServerFrame> {
    if bytes.len() < 4 {
        return Err(TermctlError::InvalidEnvelope);
    }
    let length = u32::from_le_bytes(bytes[..4].try_into().expect("four-byte prefix")) as usize;
    if length > MAX_SUPERVISOR_FRAME_BYTES || bytes.len() != length + 4 {
        return Err(TermctlError::InvalidEnvelope);
    }
    serde_json::from_slice(&bytes[4..]).map_err(|_| TermctlError::InvalidEnvelope)
}

fn queue_supervisor_events(
    stream: &mut TerminalStream,
    frame: SupervisorServerFrame,
) -> Result<()> {
    match frame {
        SupervisorServerFrame::AttachSync {
            base_seq,
            snapshot,
            frames,
        } => {
            stream.record_terminal_seq(base_seq);
            let retained = STANDARD
                .decode(snapshot.retained_output)
                .map_err(|_| TermctlError::InvalidEnvelope)?;
            if !retained.is_empty() {
                stream
                    .pending_events
                    .push_back(TerminalStreamEvent::Output(retained));
            }
            for frame in frames {
                queue_terminal_frame(stream, frame);
            }
        }
        SupervisorServerFrame::TerminalFrame { frame } => queue_terminal_frame(stream, frame),
        SupervisorServerFrame::HeartbeatPing { .. } => {}
        SupervisorServerFrame::Close => stream.pending_events.push_back(TerminalStreamEvent::End),
    }
    Ok(())
}

fn queue_terminal_frame(stream: &mut TerminalStream, frame: PtyTerminalFrame) {
    match frame {
        PtyTerminalFrame::Snapshot { base_seq, data, .. } => {
            stream.record_terminal_seq(base_seq);
            if !data.is_empty() {
                stream
                    .pending_events
                    .push_back(TerminalStreamEvent::Output(data));
            }
        }
        PtyTerminalFrame::Output { terminal_seq, data } => {
            stream.record_terminal_seq(terminal_seq);
            if !data.is_empty() {
                stream
                    .pending_events
                    .push_back(TerminalStreamEvent::Output(data));
            }
        }
        PtyTerminalFrame::Resize { terminal_seq, .. } => stream.record_terminal_seq(terminal_seq),
        PtyTerminalFrame::Exit { terminal_seq, .. } => {
            stream.record_terminal_seq(terminal_seq);
            stream.pending_events.push_back(TerminalStreamEvent::End);
        }
    }
}

fn supervisor_frame_has_resize(frame: &SupervisorServerFrame, expected: TerminalSize) -> bool {
    let matches_size = |size: termd::pty::PtySize| {
        size.rows == expected.rows
            && size.cols == expected.cols
            && size.pixel_width == expected.pixel_width
            && size.pixel_height == expected.pixel_height
    };
    match frame {
        SupervisorServerFrame::AttachSync { frames, .. } => frames.iter().any(
            |frame| matches!(frame, PtyTerminalFrame::Resize { size, .. } if matches_size(*size)),
        ),
        SupervisorServerFrame::TerminalFrame {
            frame: PtyTerminalFrame::Resize { size, .. },
        } => matches_size(*size),
        _ => false,
    }
}

fn application_http_url(server_url: &str, path: &str) -> Result<Url> {
    let mut parsed = Url::parse(server_url).map_err(|_| TermctlError::InvalidWsUrl)?;
    match parsed.scheme() {
        "ws" => parsed
            .set_scheme("http")
            .map_err(|_| TermctlError::InvalidWsUrl)?,
        "wss" => parsed
            .set_scheme("https")
            .map_err(|_| TermctlError::InvalidWsUrl)?,
        "http" | "https" => {}
        _ => return Err(TermctlError::InvalidWsUrl),
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    let base = strip_workspace_path(parsed.path());
    parsed.set_path(&format!("{base}{path}"));
    Ok(parsed)
}

fn application_ws_url(server_url: &str, kind: &str) -> Result<Url> {
    let mut parsed = Url::parse(server_url).map_err(|_| TermctlError::InvalidWsUrl)?;
    match parsed.scheme() {
        "http" => parsed
            .set_scheme("ws")
            .map_err(|_| TermctlError::InvalidWsUrl)?,
        "https" => parsed
            .set_scheme("wss")
            .map_err(|_| TermctlError::InvalidWsUrl)?,
        "ws" | "wss" => {}
        _ => return Err(TermctlError::InvalidWsUrl),
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    let base = strip_workspace_path(parsed.path());
    parsed.set_path(&format!("{base}/ws/{kind}"));
    Ok(parsed)
}

fn strip_workspace_path(path: &str) -> &str {
    for suffix in ["/ws/metadata", "/ws/terminal", "/ws"] {
        if let Some(base) = path.strip_suffix(suffix) {
            return base;
        }
        if let Some(base) = path.strip_suffix(&format!("{suffix}/")) {
            return base;
        }
    }
    path.trim_end_matches('/')
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: RemoteError,
}

#[derive(Deserialize)]
struct RemoteError {
    code: String,
    message: String,
}

async fn decode_http_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| TermctlError::ReceiveFailed)?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|_| TermctlError::InvalidEnvelope);
    }
    let error = serde_json::from_slice::<ErrorResponse>(&bytes)
        .map(|body| protocol_error(&body.error.code, &body.error.message))
        .unwrap_or_else(|_| http_status_error(status));
    Err(error)
}

fn socket_error(payload: Value) -> TermctlError {
    let code = payload
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("protocol_error");
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request failed");
    protocol_error(code, message)
}

fn protocol_error(code: &str, message: &str) -> TermctlError {
    TermctlError::Protocol {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn http_status_error(status: StatusCode) -> TermctlError {
    protocol_error(
        "http_error",
        &format!("HTTP request failed with status {}", status.as_u16()),
    )
}
