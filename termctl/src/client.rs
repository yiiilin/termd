//! direct WebSocket termd 客户端。
//!
//! WebSocket 打开后先发送明文 `route_hello` 并等待 `route_ready`，随后通过
//! `hello` 确认 daemon 身份和 binary packet 能力。pair/auth/session/control 业务
//! 统一封装为已认证的明文 `packet`。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt, stream::FuturesUnordered};
use rustls::{ClientConfig, RootCertStore};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd::auth::{AuthSigningInput, DaemonPublicIdentity};
#[cfg(test)]
use termd::auth::{DaemonE2eeSigningInput, SignatureVerifier};
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
#[cfg(test)]
use termd::net::signature::Ed25519SignatureVerifier;
#[cfg(test)]
use termd::net::{E2eeSession, E2eeSessionRole};
use termd_proto::{
    AuthChallengePayload, AuthPayload, BINARY_PROTOCOL_VERSION, ControlGrantPayload,
    ControlRequestPayload, DeviceId, ErrorPayload, HelloPayload, METHOD_AUTH,
    METHOD_AUTH_CHALLENGE, METHOD_CONTROL_REQUEST, METHOD_PAIR_REQUEST, METHOD_PING,
    METHOD_SESSION_CLOSE, METHOD_SESSION_CREATE, METHOD_SESSION_LIST, METHOD_SESSION_RESIZE,
    METHOD_TERMINAL_ATTACH, MessageType, PROTOCOL_PACKET_VERSION, PacketErrorPayload, PacketKind,
    PacketRequestId, PacketStreamId, PairAcceptPayload, PairRequestPayload, PairingToken,
    PingPayload, PongPayload, ProtocolPacket, ProtocolVersion, PublicKey, RelayAdmissionPayload,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId, SessionAttachPayload,
    SessionAttachedPayload, SessionClosePayload, SessionClosedPayload, SessionCreatePayload,
    SessionCreatedPayload, SessionDataPayload, SessionId, SessionListPayload,
    SessionListResultPayload, SessionResizePayload, SessionResizedPayload, TerminalFramePayload,
    TerminalSize, decode_binary_protocol_packet, encode_binary_protocol_packet,
    protocol_packet_from_binary, protocol_packet_to_binary,
};
#[cfg(test)]
use termd_proto::{E2eeKeyExchangePayload, EncryptedFramePayload};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::PairedServerState;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
// 中文注释：trusted relay 会先给 client route_ready，再等待 daemon data pipe 反连。
// 这里要匹配 relay/daemon 的 20s 配对窗口，避免公网慢路径被误报为普通连接关闭。
const DAEMON_HELLO_TIMEOUT: Duration = Duration::from_secs(20);
// 公网 relay 偶发卡在 TCP/TLS/WebSocket open 阶段；一次性 CLI 不应让单次半开握手
// 吃掉数秒。快速失败后重试，最后仍保留多次机会覆盖真实网络抖动。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1200);
const CONNECT_HEDGE_DELAY: Duration = Duration::from_millis(300);
const CONNECT_ATTEMPTS: usize = 4;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(80);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_PACKET_QUEUE_LIMIT: usize = 128;
const TERMINAL_STREAM_INITIAL_CREDIT: u32 = 256 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsResponse = tokio_tungstenite::tungstenite::handshake::client::Response;

#[derive(Debug, Clone)]
pub struct TerminalStream {
    id: PacketStreamId,
    next_send_seq: u64,
    last_recv_seq: u64,
    last_terminal_seq: Option<u64>,
    // 中文注释：一个 terminal stream chunk 现在可能携带 batch；termctl 没有浏览器端
    // renderer 写入队列，所以需要把同一 packet 解出的 Output/End 事件按顺序暂存在本地。
    pending_events: VecDeque<TerminalStreamEvent>,
}

impl TerminalStream {
    fn new() -> Self {
        Self::with_id(PacketStreamId::new())
    }

    fn with_id(id: PacketStreamId) -> Self {
        Self {
            id,
            next_send_seq: 1,
            last_recv_seq: 0,
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

    fn record_terminal_progress(&mut self, next_seq: Option<u64>) {
        if let Some(next_seq) = next_seq {
            self.last_terminal_seq = Some(self.last_terminal_seq.unwrap_or(0).max(next_seq));
        }
    }

    fn data_chunk_packet(
        &mut self,
        session_id: SessionId,
        bytes: &[u8],
    ) -> Result<ProtocolPacket<Value>> {
        let seq = self.next_send_seq;
        self.next_send_seq = self.next_send_seq.saturating_add(1);
        packet_stream_chunk(
            self.id,
            seq,
            SessionDataPayload {
                session_id,
                data_base64: crypto::encode_session_data(bytes),
            },
        )
    }

    fn flow_packet(&self, ack: u64, credit: u32) -> ProtocolPacket<Value> {
        ProtocolPacket::flow(self.id, ack, credit)
    }

    fn cancel_packet(&self) -> ProtocolPacket<Value> {
        ProtocolPacket::cancel_stream(self.id, serde_json::json!({}))
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
    socket: WsStream,
    device_id: DeviceId,
    daemon_identity: DaemonPublicIdentity,
    binary_mode: bool,
    // 中文注释：同一 WebSocket 上会交错出现 request response 与 terminal stream chunk。
    // 等待某个 response 时读到的非目标 packet 不能丢弃，必须留给后续 attach 主循环处理。
    pending_packets: VecDeque<ProtocolPacket<Value>>,
}

pub fn signed_device_relay_admission(
    server_id: ServerId,
    device_id: DeviceId,
    signing_key: &SigningKey,
) -> RelayAdmissionPayload {
    let nonce = crypto::nonce();
    let timestamp_ms = crypto::now_ms();
    let signing_input = relay_admission_signing_input(server_id, device_id, &nonce, timestamp_ms);
    RelayAdmissionPayload::Device {
        device_id,
        nonce,
        timestamp_ms,
        signature: crypto::sign_to_wire(signing_key, &signing_input),
    }
}

fn relay_admission_signing_input(
    server_id: ServerId,
    device_id: DeviceId,
    nonce: &termd_proto::Nonce,
    timestamp_ms: termd_proto::UnixTimestampMillis,
) -> Vec<u8> {
    // 中文注释：relay admission 只证明设备愿意进入该 daemon 房间；
    // daemon 后续仍用 auth challenge 做最终认证。
    let mut out = b"termd-relay-admission-v1\n".to_vec();
    append_canonical_field(&mut out, "server_id", &server_id.0.to_string());
    append_canonical_field(&mut out, "device_id", &device_id.0.to_string());
    append_canonical_field(&mut out, "nonce", &nonce.0);
    append_canonical_field(&mut out, "timestamp_ms", &timestamp_ms.0.to_string());
    out
}

fn append_canonical_field(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(format!("{name}:{}:{value}\n", value.as_bytes().len()).as_bytes());
}

impl DirectClient {
    pub async fn connect(
        url: &str,
        route_server_id: ServerId,
        device_id: DeviceId,
        expected_daemon_public_key: PublicKey,
        admission: Option<RelayAdmissionPayload>,
    ) -> Result<Self> {
        let (mut socket, _) = connect_websocket(url)
            .await
            .map_err(|_| TermctlError::ConnectFailed)?;

        send_outer_on_socket(
            &mut socket,
            envelope_value(
                MessageType::RouteHello,
                RouteHelloPayload {
                    server_id: route_server_id,
                    role: RouteRole::Client,
                    protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                    nonce: crypto::nonce(),
                    admission,
                    route_generation: None,
                    client_id: None,
                    data_token: None,
                    timestamp_ms: crypto::now_ms(),
                },
            )?,
        )
        .await?;
        let route_ready: RouteReadyPayload =
            expect_outer_payload(&mut socket, MessageType::RouteReady).await?;
        if route_ready.server_id != route_server_id || route_ready.role != RouteRole::Client {
            return Err(TermctlError::RouteServerMismatch);
        }

        let daemon_identity = DaemonPublicIdentity {
            server_id: route_server_id,
            public_key: expected_daemon_public_key,
        };

        let envelope = timeout(DAEMON_HELLO_TIMEOUT, read_outer(&mut socket))
            .await
            .map_err(|_| TermctlError::DaemonHelloTimeout)??;
        let hello: HelloPayload = match envelope.kind {
            MessageType::Hello => {
                decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)?
            }
            MessageType::Error => return Err(protocol_error(envelope.payload)),
            _ => return Err(TermctlError::UnexpectedMessage),
        };
        if hello
            .server_id
            .is_some_and(|server_id| server_id != route_server_id)
        {
            return Err(TermctlError::RouteServerMismatch);
        }
        if hello.protocol_version.0 != PROTOCOL_PACKET_VERSION {
            return Err(TermctlError::InvalidEnvelope);
        }
        if hello.daemon_public_key.as_ref() != Some(&daemon_identity.public_key) {
            return Err(TermctlError::RouteServerMismatch);
        }
        let binary_mode = hello
            .binary_version
            .is_some_and(|version| version.0 == BINARY_PROTOCOL_VERSION);
        let mut client = Self {
            socket,
            device_id,
            daemon_identity,
            binary_mode,
            pending_packets: VecDeque::new(),
        };

        client
            .send_outer(envelope_value(
                MessageType::Hello,
                HelloPayload {
                    protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                    nonce: crypto::nonce(),
                    timestamp_ms: crypto::now_ms(),
                    server_id: Some(route_server_id),
                    daemon_public_key: None,
                    binary_version: if binary_mode {
                        Some(ProtocolVersion(BINARY_PROTOCOL_VERSION))
                    } else {
                        None
                    },
                    device_id: Some(device_id),
                },
            )?)
            .await?;

        Ok(client)
    }

    pub async fn pair(
        &mut self,
        device_public_key: PublicKey,
        token: String,
    ) -> Result<PairAcceptPayload> {
        self.request_packet(
            METHOD_PAIR_REQUEST,
            PairRequestPayload {
                device_id: self.device_id,
                device_public_key,
                token: PairingToken(token),
                nonce: crypto::nonce(),
                timestamp_ms: crypto::now_ms(),
            },
        )
        .await
    }

    pub async fn authenticate(
        &mut self,
        signing_key: &SigningKey,
        paired_server: &PairedServerState,
    ) -> Result<()> {
        let challenge: AuthChallengePayload = self
            .expect_packet_event(METHOD_AUTH_CHALLENGE)
            .await
            .map_err(|error| match error {
                TermctlError::ConnectionClosed => TermctlError::AuthChallengeTimeout,
                other => other,
            })?;

        let mut auth = AuthPayload {
            device_id: self.device_id,
            challenge: challenge.challenge,
            nonce: crypto::nonce(),
            timestamp_ms: crypto::now_ms(),
            signature: termd_proto::Signature("ed25519-v1:placeholder".to_owned()),
        };
        if paired_server.server_id != self.daemon_identity.server_id
            || paired_server.daemon_public_key != self.daemon_identity.public_key
        {
            return Err(TermctlError::RouteServerMismatch);
        }
        let signing_input = AuthSigningInput::from_payload(&auth, &self.daemon_identity).to_bytes();
        auth.signature = crypto::sign_to_wire(signing_key, &signing_input);

        let _: Value = self.request_packet(METHOD_AUTH, auth).await?;
        Ok(())
    }

    pub async fn create_session(
        &mut self,
        command: Vec<String>,
        size: TerminalSize,
    ) -> Result<SessionCreatedPayload> {
        self.request_packet(
            METHOD_SESSION_CREATE,
            SessionCreatePayload { command, size },
        )
        .await
    }

    pub async fn attach_terminal_stream_with_options(
        &mut self,
        session_id: SessionId,
        options: TerminalAttachOptions,
    ) -> Result<(SessionAttachedPayload, TerminalStream)> {
        let request_id = PacketRequestId::new();
        let mut stream = TerminalStream::new();
        // 重连刚成功但还没收到新帧时，也要保留 resume 下限，避免下一次重连退回全量 replay。
        stream.last_terminal_seq = options.last_terminal_seq;
        let packet = terminal_attach_packet(request_id, stream.id, session_id, options)?;

        self.send_packet(packet).await?;
        let attached = self
            .expect_packet_response(request_id, METHOD_TERMINAL_ATTACH)
            .await?;
        Ok((attached, stream))
    }

    pub async fn request_control(&mut self, session_id: SessionId) -> Result<ControlGrantPayload> {
        self.request_packet(
            METHOD_CONTROL_REQUEST,
            ControlRequestPayload {
                session_id,
                device_id: self.device_id,
            },
        )
        .await
    }

    pub async fn resize_session(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> Result<()> {
        // resize 必须等 daemon 返回明确确认后才算完成，避免客户端先行调整本地状态。
        let _: SessionResizedPayload = self
            .request_packet(
                METHOD_SESSION_RESIZE,
                SessionResizePayload { session_id, size },
            )
            .await?;
        Ok(())
    }

    pub async fn list_sessions(&mut self) -> Result<SessionListResultPayload> {
        self.request_packet(METHOD_SESSION_LIST, SessionListPayload {})
            .await
    }

    pub async fn close_session(&mut self, session_id: SessionId) -> Result<SessionClosedPayload> {
        // 中文注释：关闭 session 必须走 daemon 的协议路径，由 daemon 终止 supervisor
        // 并同步清理 SQLite 状态；不能只往 PTY 写 exit 之类的 shell 命令。
        self.request_packet(METHOD_SESSION_CLOSE, SessionClosePayload { session_id })
            .await
    }

    pub async fn send_terminal_data(
        &mut self,
        stream: &mut TerminalStream,
        session_id: SessionId,
        bytes: &[u8],
    ) -> Result<()> {
        let packet = stream.data_chunk_packet(session_id, bytes)?;
        self.send_packet(packet).await
    }

    pub async fn cancel_terminal_stream(&mut self, stream: &TerminalStream) -> Result<()> {
        self.send_packet(stream.cancel_packet()).await
    }

    pub async fn receive_terminal_event(
        &mut self,
        stream: &mut TerminalStream,
    ) -> Result<TerminalStreamEvent> {
        if let Some(event) = stream.pending_events.pop_front() {
            return Ok(event);
        }

        loop {
            let packet = self.receive_packet().await?;
            if packet.stream_id != Some(stream.id) {
                continue;
            }

            match packet.kind {
                PacketKind::StreamChunk => {
                    stream.last_recv_seq = packet.seq;
                    if let Ok(frame) =
                        decode_payload::<TerminalFramePayload>(packet.payload.clone())
                    {
                        let terminal_progress = terminal_frame_progress(&frame);
                        let (events, credit) = terminal_frame_events_and_credit(frame)?;
                        self.send_packet(stream.flow_packet(packet.seq, credit))
                            .await?;
                        // termctl 没有浏览器端渲染队列；它把 snapshot/output 都作为 stdout bytes 输出。
                        // resize 只更新远端终端状态，不产生本地字节；exit 则结束流。
                        // 只有 flow ack 成功后才推进 resume 序号，避免 ack 失败重连时跳过未确认帧。
                        stream.record_terminal_progress(terminal_progress);
                        stream.pending_events.extend(events);
                        if let Some(event) = stream.pending_events.pop_front() {
                            return Ok(event);
                        }
                        continue;
                    }

                    let payload: SessionDataPayload = decode_payload(packet.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope)?;
                    let bytes = crypto::decode_session_data(&payload.data_base64)?;
                    let credit = bytes.len().max(1).min(u32::MAX as usize) as u32;
                    self.send_packet(stream.flow_packet(packet.seq, credit))
                        .await?;
                    return Ok(TerminalStreamEvent::Output(bytes));
                }
                PacketKind::StreamEnd | PacketKind::Cancel => return Ok(TerminalStreamEvent::End),
                PacketKind::Error => return Err(packet_error(packet.payload)),
                PacketKind::Flow
                | PacketKind::Event
                | PacketKind::Response
                | PacketKind::Request
                | PacketKind::StreamOpen => continue,
            }
        }
    }

    #[allow(dead_code)]
    pub async fn send_ping(&mut self) -> Result<()> {
        let _: PongPayload = self
            .request_packet(
                METHOD_PING,
                PingPayload {
                    nonce: crypto::nonce(),
                    timestamp_ms: crypto::now_ms(),
                },
            )
            .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn receive_inner(&mut self) -> Result<JsonEnvelope> {
        let message = read_outer_message(&mut self.socket).await?;

        match message {
            OuterMessage::Json(envelope) => match envelope.kind {
                MessageType::Packet => Ok(envelope),
                MessageType::Error => Err(protocol_error(envelope.payload)),
                _ => Err(TermctlError::UnexpectedMessage),
            },
            OuterMessage::Binary(raw) => {
                let binary_packet = decode_binary_protocol_packet(&raw)
                    .map_err(|_| TermctlError::InvalidEnvelope)?;
                let packet = protocol_packet_from_binary(binary_packet)
                    .map_err(|_| TermctlError::InvalidEnvelope)?;
                if packet.kind == PacketKind::Error {
                    return Err(packet_error(packet.payload));
                }
                Ok(packet_envelope(packet)?)
            }
        }
    }

    async fn request_packet<P, T>(&mut self, method: &'static str, payload: P) -> Result<T>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let request_id = PacketRequestId::new();
        self.send_packet(packet_request(request_id, method, payload)?)
            .await?;
        self.expect_packet_response(request_id, method).await
    }

    async fn expect_packet_response<T>(
        &mut self,
        request_id: PacketRequestId,
        method: &'static str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            if let Some(payload) = self.take_pending_packet_response(request_id, method)? {
                return Ok(payload);
            }

            let packet = self.receive_packet_from_socket_timeout().await?;
            if let Some(payload) =
                decode_packet_response_for_request(packet.clone(), request_id, method)?
            {
                return Ok(payload);
            }
            self.queue_pending_packet(packet)?;
        }
    }

    async fn expect_packet_event<T>(&mut self, method: &'static str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            if let Some(payload) = self.take_pending_packet_event(method)? {
                return Ok(payload);
            }

            let packet = self.receive_packet_from_socket_timeout().await?;
            match decode_packet_event_for_method(packet.clone(), method)? {
                Some(payload) => return Ok(payload),
                None => {
                    self.queue_pending_packet(packet)?;
                }
            }
        }
    }

    async fn receive_packet(&mut self) -> Result<ProtocolPacket<Value>> {
        if let Some(packet) = self.pending_packets.pop_front() {
            return Ok(packet);
        }

        self.receive_packet_from_socket().await
    }

    async fn receive_packet_from_socket(&mut self) -> Result<ProtocolPacket<Value>> {
        let message = read_outer_message(&mut self.socket).await?;

        let packet = match message {
            OuterMessage::Json(envelope) => match envelope.kind {
                MessageType::Packet => {
                    decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)?
                }
                MessageType::Error => return Err(protocol_error(envelope.payload)),
                _ => return Err(TermctlError::UnexpectedMessage),
            },
            OuterMessage::Binary(raw) => protocol_packet_from_binary(
                decode_binary_protocol_packet(&raw).map_err(|_| TermctlError::InvalidEnvelope)?,
            )
            .map_err(|_| TermctlError::InvalidEnvelope)?,
        };

        validate_packet_version(&packet)?;
        Ok(packet)
    }

    async fn receive_packet_from_socket_timeout(&mut self) -> Result<ProtocolPacket<Value>> {
        timeout(RESPONSE_TIMEOUT, self.receive_packet_from_socket())
            .await
            .map_err(|_| TermctlError::ConnectionClosed)?
    }

    fn take_pending_packet_response<T>(
        &mut self,
        request_id: PacketRequestId,
        method: &'static str,
    ) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let mut index = 0;
        while index < self.pending_packets.len() {
            let packet = self
                .pending_packets
                .get(index)
                .cloned()
                .ok_or(TermctlError::InvalidEnvelope)?;
            if let Some(payload) = decode_packet_response_for_request(packet, request_id, method)? {
                self.pending_packets.remove(index);
                return Ok(Some(payload));
            }
            index += 1;
        }
        Ok(None)
    }

    fn take_pending_packet_event<T>(&mut self, method: &'static str) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let mut index = 0;
        while index < self.pending_packets.len() {
            let packet = self
                .pending_packets
                .get(index)
                .cloned()
                .ok_or(TermctlError::InvalidEnvelope)?;
            if let Some(payload) = decode_packet_event_for_method(packet, method)? {
                self.pending_packets.remove(index);
                return Ok(Some(payload));
            }
            index += 1;
        }
        Ok(None)
    }

    fn queue_pending_packet(&mut self, packet: ProtocolPacket<Value>) -> Result<()> {
        if self.pending_packets.len() >= PENDING_PACKET_QUEUE_LIMIT {
            // 中文注释：resize/control 等 RPC 等响应时可能持续读到 terminal chunk。
            // 这些包不能丢，但也不能无限缓存；超过上限直接让 attach 重新建立状态。
            return Err(TermctlError::PendingPacketQueueFull);
        }
        self.pending_packets.push_back(packet);
        Ok(())
    }

    async fn send_packet(&mut self, packet: ProtocolPacket<Value>) -> Result<()> {
        if self.binary_mode {
            let binary_packet =
                protocol_packet_to_binary(packet).map_err(|_| TermctlError::InvalidEnvelope)?;
            return self
                .send_binary_outer(encode_binary_protocol_packet(&binary_packet))
                .await;
        }

        self.send_inner(packet_envelope(packet)?).await
    }

    async fn send_inner(&mut self, inner: JsonEnvelope) -> Result<()> {
        self.send_outer(inner).await
    }

    async fn send_binary_outer(&mut self, raw: Vec<u8>) -> Result<()> {
        timeout(SEND_TIMEOUT, self.socket.send(Message::Binary(raw.into())))
            .await
            .map_err(|_| TermctlError::SendFailed)?
            .map_err(|_| TermctlError::SendFailed)
    }

    async fn send_outer(&mut self, envelope: JsonEnvelope) -> Result<()> {
        let raw = serde_json::to_string(&envelope).map_err(|_| TermctlError::InvalidEnvelope)?;
        timeout(SEND_TIMEOUT, self.socket.send(Message::Text(raw.into())))
            .await
            .map_err(|_| TermctlError::SendFailed)?
            .map_err(|_| TermctlError::SendFailed)
    }
}

async fn connect_websocket(url: &str) -> Result<(WsStream, WsResponse)> {
    for attempt in 1..=CONNECT_ATTEMPTS {
        let result = connect_websocket_hedged(url).await;

        match result {
            Ok(socket) => return Ok(socket),
            Err(error) if attempt < CONNECT_ATTEMPTS => {
                // termctl 是一次性 CLI，没有 daemon/web 那样的长连接自动恢复；TCP/TLS 建连阶段
                // 允许很短的重试，避免公网入口瞬时抖动直接变成用户可见失败。
                let _ = error;
                sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }

    Err(TermctlError::ConnectFailed)
}

async fn connect_websocket_hedged(url: &str) -> Result<(WsStream, WsResponse)> {
    // 中文注释：公网 relay 偶发卡在单条 TLS/WebSocket open 上。这里必须让第一条
    // 连接继续推进，300ms 后再补开第二条；谁先成功就用谁，失败的一条不会立刻拖垮
    // 另一条，避免一次坏握手或半开 TLS 连接把 CLI 卡到秒级。
    let owned_url = url.to_owned();
    let mut connections: FuturesUnordered<_> = FuturesUnordered::new();
    connections.push(connect_websocket_once_owned(owned_url.clone()));

    let hedge_delay = sleep(CONNECT_HEDGE_DELAY);
    tokio::pin!(hedge_delay);
    let mut hedge_started = false;
    let mut last_error: Option<TermctlError> = None;

    loop {
        tokio::select! {
            result = connections.next() => {
                match result {
                    Some(Ok(socket)) => return Ok(socket),
                    Some(Err(error)) => {
                        last_error = Some(error);
                        if hedge_started && connections.is_empty() {
                            return Err(last_error.unwrap_or(TermctlError::ConnectFailed));
                        }
                    }
                    None => {
                        return Err(last_error.unwrap_or(TermctlError::ConnectFailed));
                    }
                }
            }
            _ = &mut hedge_delay, if !hedge_started => {
                hedge_started = true;
                connections.push(connect_websocket_once_owned(owned_url.clone()));
            }
        }
    }
}

async fn connect_websocket_once_owned(url: String) -> Result<(WsStream, WsResponse)> {
    timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            None,
            false,
            Some(termctl_tls_connector()),
        ),
    )
    .await
    .map_err(|_| TermctlError::ConnectFailed)
    .and_then(|result| result.map_err(|_| TermctlError::ConnectFailed))
}

fn termctl_tls_connector() -> Connector {
    Connector::Rustls(Arc::new(termctl_tls_client_config()))
}

fn termctl_tls_client_config() -> ClientConfig {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = termctl_tls_kx_groups();

    // 部分公网 TLS 入口会吞掉 rustls 默认 hybrid ClientHello；termctl 作为人工操作工具，
    // 这里和 daemon relay 连接保持一致，优先使用传统 ECDHE，避免连接建立阶段假死。
    ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("termctl TLS protocol versions should be valid")
        .with_root_certificates(root_store)
        .with_no_client_auth()
}

fn termctl_tls_kx_groups() -> Vec<&'static dyn rustls::crypto::SupportedKxGroup> {
    vec![
        rustls::crypto::aws_lc_rs::kx_group::X25519,
        rustls::crypto::aws_lc_rs::kx_group::SECP256R1,
        rustls::crypto::aws_lc_rs::kx_group::SECP384R1,
    ]
}

async fn expect_outer_payload<T>(socket: &mut WsStream, expected: MessageType) -> Result<T>
where
    T: DeserializeOwned,
{
    let envelope = timeout(HANDSHAKE_TIMEOUT, read_outer(socket))
        .await
        .map_err(|_| TermctlError::ConnectionClosed)??;
    if envelope.kind == MessageType::Error {
        return Err(protocol_error(envelope.payload));
    }
    if envelope.kind != expected {
        return Err(TermctlError::UnexpectedMessage);
    }

    decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)
}

async fn send_outer_on_socket(socket: &mut WsStream, envelope: JsonEnvelope) -> Result<()> {
    let raw = serde_json::to_string(&envelope).map_err(|_| TermctlError::InvalidEnvelope)?;
    timeout(SEND_TIMEOUT, socket.send(Message::Text(raw.into())))
        .await
        .map_err(|_| TermctlError::SendFailed)?
        .map_err(|_| TermctlError::SendFailed)
}

async fn read_outer(socket: &mut WsStream) -> Result<JsonEnvelope> {
    match read_outer_message(socket).await? {
        OuterMessage::Json(envelope) => Ok(envelope),
        // 握手阶段还没有 E2EE session，daemon 不应发送 binary E2EE frame。
        OuterMessage::Binary(_) => Err(TermctlError::UnexpectedMessage),
    }
}

enum OuterMessage {
    Json(JsonEnvelope),
    Binary(Vec<u8>),
}

async fn read_outer_message(socket: &mut WsStream) -> Result<OuterMessage> {
    while let Some(message) = socket.next().await {
        let message = message.map_err(|_| TermctlError::ReceiveFailed)?;

        match message {
            Message::Text(raw) => {
                let envelope = serde_json::from_str(raw.as_str())
                    .map_err(|_| TermctlError::InvalidEnvelope)?;
                return Ok(OuterMessage::Json(envelope));
            }
            Message::Binary(raw) => {
                return Ok(OuterMessage::Binary(raw.to_vec()));
            }
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| TermctlError::SendFailed)?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(TermctlError::ConnectionClosed),
            Message::Frame(_) => {}
        }
    }

    Err(TermctlError::ConnectionClosed)
}

fn protocol_error(payload: Value) -> TermctlError {
    match decode_payload::<ErrorPayload>(payload) {
        Ok(error) => TermctlError::Protocol {
            code: error.code,
            message: error.message,
        },
        Err(_) => TermctlError::InvalidEnvelope,
    }
}

#[cfg(test)]
fn verify_daemon_e2ee_key_exchange(
    payload: &E2eeKeyExchangePayload,
    daemon_identity: &DaemonPublicIdentity,
) -> Result<()> {
    if payload.packet_version != Some(ProtocolVersion(PROTOCOL_PACKET_VERSION)) {
        return Err(TermctlError::InvalidEnvelope);
    }
    let signature = payload
        .signature
        .as_ref()
        .ok_or(TermctlError::InvalidEnvelope)?;
    let signing_input = DaemonE2eeSigningInput::from_payload(payload, daemon_identity).to_bytes();

    Ed25519SignatureVerifier
        .verify(&daemon_identity.public_key, &signing_input, signature)
        .map_err(|_| TermctlError::E2eeFailed)
}

#[cfg(test)]
fn packet_request_envelope<P>(
    request_id: PacketRequestId,
    method: &'static str,
    payload: P,
) -> Result<JsonEnvelope>
where
    P: Serialize,
{
    let payload = serde_json::to_value(payload).map_err(|_| TermctlError::InvalidEnvelope)?;
    packet_envelope(ProtocolPacket::request(request_id, method, payload))
}

fn packet_request<P>(
    request_id: PacketRequestId,
    method: &'static str,
    payload: P,
) -> Result<ProtocolPacket<Value>>
where
    P: Serialize,
{
    let payload = serde_json::to_value(payload).map_err(|_| TermctlError::InvalidEnvelope)?;
    Ok(ProtocolPacket::request(request_id, method, payload))
}

fn packet_stream_open<P>(
    request_id: PacketRequestId,
    stream_id: PacketStreamId,
    method: &'static str,
    credit: u32,
    payload: P,
) -> Result<ProtocolPacket<Value>>
where
    P: Serialize,
{
    let payload = serde_json::to_value(payload).map_err(|_| TermctlError::InvalidEnvelope)?;
    Ok(ProtocolPacket::stream_open(
        request_id, stream_id, method, credit, payload,
    ))
}

fn terminal_attach_packet(
    request_id: PacketRequestId,
    stream_id: PacketStreamId,
    session_id: SessionId,
    options: TerminalAttachOptions,
) -> Result<ProtocolPacket<Value>> {
    packet_stream_open(
        request_id,
        stream_id,
        METHOD_TERMINAL_ATTACH,
        TERMINAL_STREAM_INITIAL_CREDIT,
        SessionAttachPayload {
            session_id,
            watch_updates: options.watch_updates,
            last_terminal_seq: options.last_terminal_seq,
        },
    )
}

fn packet_stream_chunk<P>(
    stream_id: PacketStreamId,
    seq: u64,
    payload: P,
) -> Result<ProtocolPacket<Value>>
where
    P: Serialize,
{
    let payload = serde_json::to_value(payload).map_err(|_| TermctlError::InvalidEnvelope)?;
    Ok(ProtocolPacket::stream_chunk(stream_id, seq, payload))
}

fn terminal_frame_events_and_credit(
    frame: TerminalFramePayload,
) -> Result<(Vec<TerminalStreamEvent>, u32)> {
    match frame {
        TerminalFramePayload::Snapshot { data_base64, .. }
        | TerminalFramePayload::Output { data_base64, .. } => {
            let bytes = crypto::decode_session_data(&data_base64)?;
            let credit = bytes.len().max(1).min(u32::MAX as usize) as u32;
            Ok((vec![TerminalStreamEvent::Output(bytes)], credit))
        }
        TerminalFramePayload::Resize { .. } => Ok((Vec::new(), 1)),
        TerminalFramePayload::Exit { .. } => Ok((vec![TerminalStreamEvent::End], 1)),
        TerminalFramePayload::Batch { frames, .. } => {
            let mut events = Vec::new();
            let mut credit = 0_u32;
            for frame in frames {
                let (frame_events, frame_credit) = terminal_frame_events_and_credit(frame)?;
                credit = credit.saturating_add(frame_credit.max(1));
                let saw_end = frame_events
                    .iter()
                    .any(|event| matches!(event, TerminalStreamEvent::End));
                events.extend(frame_events);
                if saw_end {
                    break;
                }
            }
            Ok((coalesce_terminal_output_events(events), credit.max(1)))
        }
    }
}

fn terminal_frame_progress(frame: &TerminalFramePayload) -> Option<u64> {
    match frame {
        TerminalFramePayload::Snapshot { base_seq, .. } => Some(*base_seq),
        other => other.terminal_seq(),
    }
}

fn coalesce_terminal_output_events(events: Vec<TerminalStreamEvent>) -> Vec<TerminalStreamEvent> {
    let mut coalesced = Vec::new();
    for event in events {
        match event {
            TerminalStreamEvent::Output(bytes) => {
                if let Some(TerminalStreamEvent::Output(previous)) = coalesced.last_mut() {
                    previous.extend(bytes);
                } else {
                    coalesced.push(TerminalStreamEvent::Output(bytes));
                }
            }
            TerminalStreamEvent::End => coalesced.push(TerminalStreamEvent::End),
        }
    }
    coalesced
}

fn packet_envelope(packet: ProtocolPacket<Value>) -> Result<JsonEnvelope> {
    envelope_value(MessageType::Packet, packet).map_err(Into::into)
}

fn decode_packet_response_for_request<T>(
    packet: ProtocolPacket<Value>,
    request_id: PacketRequestId,
    method: &'static str,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    validate_packet_version(&packet)?;

    let Some(packet_request_id) = packet.id else {
        return Ok(None);
    };
    if packet_request_id != request_id {
        return Ok(None);
    }
    if packet
        .method
        .as_deref()
        .is_some_and(|packet_method| packet_method != method)
    {
        return Err(TermctlError::UnexpectedMessage);
    }

    match packet.kind {
        PacketKind::Response => decode_payload(packet.payload)
            .map(Some)
            .map_err(|_| TermctlError::InvalidEnvelope),
        PacketKind::Error => Err(packet_error(packet.payload)),
        _ => Ok(None),
    }
}

fn decode_packet_event_for_method<T>(
    packet: ProtocolPacket<Value>,
    method: &'static str,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    validate_packet_version(&packet)?;

    match packet.kind {
        PacketKind::Event if packet.method.as_deref() == Some(method) => {
            decode_payload(packet.payload)
                .map(Some)
                .map_err(|_| TermctlError::InvalidEnvelope)
        }
        PacketKind::Error if packet.id.is_none() => Err(packet_error(packet.payload)),
        _ => Ok(None),
    }
}

fn validate_packet_version(packet: &ProtocolPacket<Value>) -> Result<()> {
    if packet.version != PROTOCOL_PACKET_VERSION {
        return Err(TermctlError::InvalidEnvelope);
    }

    Ok(())
}

fn packet_error(payload: Value) -> TermctlError {
    match decode_payload::<PacketErrorPayload>(payload) {
        Ok(error) => TermctlError::Protocol {
            code: error.code,
            message: error.message,
        },
        Err(_) => TermctlError::InvalidEnvelope,
    }
}

#[cfg(test)]
fn encrypted_envelope_for_test(
    e2ee: &mut E2eeSession,
    inner: JsonEnvelope,
) -> Result<JsonEnvelope> {
    let frame = e2ee
        .encrypt_json_payload(&inner)
        .map_err(|_| TermctlError::E2eeFailed)?;
    envelope_value(MessageType::EncryptedFrame, frame).map_err(|_| TermctlError::InvalidEnvelope)
}

#[cfg(test)]
mod tests {
    use termd::auth::DaemonIdentity;
    use termd::net::{E2eeKeyPair, E2eeSessionContext};
    use termd_proto::{
        PacketErrorPayload, PacketKind, PacketRequestId, PacketStreamId, PairingToken,
        ProtocolPacket, SessionFileTransferChunkPayload, UnixTimestampMillis,
        binary_protocol_packet,
    };
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    use super::*;

    #[test]
    fn termctl_tls_kx_groups_exclude_hybrid_post_quantum_groups() {
        let names = termctl_tls_kx_groups()
            .into_iter()
            .map(|group| format!("{:?}", group.name()))
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name.contains("X25519")));
        assert!(names.iter().any(|name| name.contains("secp256r1")));
        assert!(!names.iter().any(|name| name.contains("MLKEM")));
    }

    #[test]
    fn daemon_e2ee_key_exchange_requires_packet_v3_and_valid_signature() {
        let identity = DaemonIdentity::generate();
        let daemon_identity = identity.public_identity();
        let mut exchange = E2eeKeyExchangePayload::new(
            daemon_identity.server_id,
            DeviceId::default(),
            PublicKey("x25519-v1:daemon-session-key".to_owned()),
            crypto::nonce(),
            UnixTimestampMillis(1_710_000_000_000),
        )
        .with_packet_version(ProtocolVersion(PROTOCOL_PACKET_VERSION));
        let signing_input =
            DaemonE2eeSigningInput::from_payload(&exchange, &daemon_identity).to_bytes();
        exchange = exchange.with_signature(identity.sign_to_wire(&signing_input).unwrap());

        verify_daemon_e2ee_key_exchange(&exchange, &daemon_identity).unwrap();

        let mut missing_version = exchange.clone();
        missing_version.packet_version = None;
        assert!(matches!(
            verify_daemon_e2ee_key_exchange(&missing_version, &daemon_identity).unwrap_err(),
            TermctlError::InvalidEnvelope
        ));

        let mut tampered = exchange;
        tampered.nonce = crypto::nonce();
        assert!(matches!(
            verify_daemon_e2ee_key_exchange(&tampered, &daemon_identity).unwrap_err(),
            TermctlError::E2eeFailed
        ));
    }

    #[test]
    fn packet_request_envelope_uses_protocol_packet_request_id() {
        let request_id = PacketRequestId::new();

        let envelope =
            packet_request_envelope(request_id, METHOD_SESSION_LIST, SessionListPayload {})
                .expect("packet request should serialize");
        let packet: ProtocolPacket<Value> =
            decode_payload(envelope.payload).expect("packet payload should decode");

        assert_eq!(envelope.kind, MessageType::Packet);
        assert_eq!(packet.kind, PacketKind::Request);
        assert_eq!(packet.id, Some(request_id));
        assert_eq!(packet.method.as_deref(), Some(METHOD_SESSION_LIST));
        assert_eq!(packet.stream_id, None);
        assert_eq!(packet.payload, serde_json::json!({}));
    }

    #[test]
    fn packet_response_matching_is_bound_to_request_id() {
        let expected_id = PacketRequestId::new();
        let other_id = PacketRequestId::new();

        let other_response = ProtocolPacket::response(
            other_id,
            METHOD_SESSION_LIST,
            serde_json::json!({"sessions": []}),
        );
        let matched: Option<SessionListResultPayload> =
            decode_packet_response_for_request(other_response, expected_id, METHOD_SESSION_LIST)
                .expect("unrelated response should be ignored");
        assert!(matched.is_none());

        let packet_error = ProtocolPacket::request_error(
            expected_id,
            PacketErrorPayload {
                code: "session_not_found".to_owned(),
                message: "session was not found".to_owned(),
                retryable: false,
            },
        );
        let packet_error: ProtocolPacket<Value> =
            serde_json::from_value(serde_json::to_value(packet_error).unwrap()).unwrap();
        let err = decode_packet_response_for_request::<SessionListResultPayload>(
            packet_error,
            expected_id,
            METHOD_SESSION_LIST,
        )
        .expect_err("matching packet error should map to the request");

        assert!(matches!(
            err,
            TermctlError::Protocol { ref code, .. } if code == "session_not_found"
        ));
    }

    #[test]
    fn terminal_stream_packets_carry_stream_id_sequence_flow_and_cancel() {
        let stream_id = PacketStreamId::new();
        let session_id = SessionId::new();
        let mut stream = TerminalStream::with_id(stream_id);

        let chunk = stream
            .data_chunk_packet(session_id, b"abc")
            .expect("terminal data chunk should serialize");
        let chunk_payload: SessionDataPayload =
            decode_payload(chunk.payload.clone()).expect("terminal data should decode");
        assert_eq!(chunk.kind, PacketKind::StreamChunk);
        assert_eq!(chunk.stream_id, Some(stream_id));
        assert_eq!(chunk.seq, 1);
        assert_eq!(chunk_payload.session_id, session_id);
        assert_eq!(
            crypto::decode_session_data(&chunk_payload.data_base64).unwrap(),
            b"abc"
        );

        let flow = stream.flow_packet(7, 16);
        assert_eq!(flow.kind, PacketKind::Flow);
        assert_eq!(flow.stream_id, Some(stream_id));
        assert_eq!(flow.ack, Some(7));
        assert_eq!(flow.credit, Some(16));

        let cancel = stream.cancel_packet();
        assert_eq!(cancel.kind, PacketKind::Cancel);
        assert_eq!(cancel.stream_id, Some(stream_id));
    }

    #[test]
    fn terminal_attach_packet_controls_watch_updates_and_resume_sequence() {
        let request_id = PacketRequestId::new();
        let stream_id = PacketStreamId::new();
        let session_id = SessionId::new();

        let packet = terminal_attach_packet(
            request_id,
            stream_id,
            session_id,
            TerminalAttachOptions {
                watch_updates: false,
                last_terminal_seq: Some(41),
            },
        )
        .expect("terminal attach should serialize");
        let payload: SessionAttachPayload =
            decode_payload(packet.payload.clone()).expect("attach payload should decode");

        assert_eq!(packet.kind, PacketKind::StreamOpen);
        assert_eq!(packet.id, Some(request_id));
        assert_eq!(packet.stream_id, Some(stream_id));
        assert_eq!(packet.method.as_deref(), Some(METHOD_TERMINAL_ATTACH));
        assert_eq!(payload.session_id, session_id);
        assert!(!payload.watch_updates);
        assert_eq!(payload.last_terminal_seq, Some(41));
    }

    #[test]
    fn terminal_stream_keeps_resume_sequence_until_new_frames_arrive() {
        let mut stream = TerminalStream::new();
        stream.last_terminal_seq = Some(41);

        let session_id = SessionId::new();
        let old_frame = TerminalFramePayload::Output {
            session_id,
            terminal_seq: 40,
            data_base64: crypto::encode_session_data(b"old"),
        };
        stream.record_terminal_progress(terminal_frame_progress(&old_frame));
        assert_eq!(stream.last_terminal_seq(), Some(41));

        let new_frame = TerminalFramePayload::Output {
            session_id,
            terminal_seq: 42,
            data_base64: crypto::encode_session_data(b"new"),
        };
        stream.record_terminal_progress(terminal_frame_progress(&new_frame));
        assert_eq!(stream.last_terminal_seq(), Some(42));
    }

    #[test]
    fn terminal_stream_can_carry_pending_events_across_reconnect() {
        let mut old_stream = TerminalStream::new();
        old_stream
            .pending_events
            .push_back(TerminalStreamEvent::Output(b"tail".to_vec()));
        old_stream
            .pending_events
            .push_back(TerminalStreamEvent::End);

        let pending = old_stream.drain_pending_events();
        assert!(old_stream.pending_events.is_empty());

        let mut new_stream = TerminalStream::new();
        new_stream.prepend_pending_events(pending);

        assert_eq!(
            new_stream.pending_events.pop_front(),
            Some(TerminalStreamEvent::Output(b"tail".to_vec()))
        );
        assert_eq!(
            new_stream.pending_events.pop_front(),
            Some(TerminalStreamEvent::End)
        );
    }

    #[tokio::test]
    async fn resize_wait_preserves_interleaved_terminal_stream_chunk() {
        let (mut client, mut daemon_socket, mut daemon_e2ee) =
            connected_direct_client_for_test().await;
        let session_id = SessionId::new();
        let size = TerminalSize::new(40, 120);
        let mut stream = TerminalStream::new();
        let stream_id = stream.id;

        let client_task = tokio::spawn(async move {
            client.resize_session(session_id, size).await?;
            let event = timeout(
                Duration::from_millis(500),
                client.receive_terminal_event(&mut stream),
            )
            .await
            .map_err(|_| TermctlError::ConnectionClosed)??;
            Ok::<_, TermctlError>((client, event))
        });

        let resize_request =
            read_client_packet_for_test(&mut daemon_socket, &mut daemon_e2ee).await;
        assert_eq!(resize_request.kind, PacketKind::Request);
        assert_eq!(
            resize_request.method.as_deref(),
            Some(METHOD_SESSION_RESIZE)
        );
        let resize_request_id = resize_request
            .id
            .expect("resize request should carry packet id");

        send_daemon_packet_for_test(
            &mut daemon_socket,
            &mut daemon_e2ee,
            ProtocolPacket::stream_chunk(
                stream_id,
                7,
                serde_json::to_value(TerminalFramePayload::Output {
                    session_id,
                    terminal_seq: 7,
                    data_base64: crypto::encode_session_data(b"after-resize"),
                })
                .expect("terminal frame should serialize"),
            ),
        )
        .await;
        send_daemon_packet_for_test(
            &mut daemon_socket,
            &mut daemon_e2ee,
            ProtocolPacket::response(
                resize_request_id,
                METHOD_SESSION_RESIZE,
                serde_json::to_value(SessionResizedPayload {
                    session_id,
                    size,
                    resize_owner: true,
                })
                .expect("resize response should serialize"),
            ),
        )
        .await;

        let (_client, event) = client_task
            .await
            .expect("client task should join")
            .expect("resize and terminal receive should succeed");
        assert_eq!(event, TerminalStreamEvent::Output(b"after-resize".to_vec()));

        let flow = timeout(
            Duration::from_secs(1),
            read_client_packet_for_test(&mut daemon_socket, &mut daemon_e2ee),
        )
        .await
        .expect("terminal chunk should be flow-acked after resize response");
        assert_eq!(flow.kind, PacketKind::Flow);
        assert_eq!(flow.stream_id, Some(stream_id));
        assert_eq!(flow.ack, Some(7));
    }

    #[tokio::test]
    async fn response_wait_bounds_interleaved_pending_packets() {
        let (mut client, mut daemon_socket, mut daemon_e2ee) =
            connected_direct_client_for_test().await;
        let session_id = SessionId::new();
        let size = TerminalSize::new(40, 120);
        let stream_id = PacketStreamId::new();

        let client_task =
            tokio::spawn(async move { client.resize_session(session_id, size).await });
        let resize_request =
            read_client_packet_for_test(&mut daemon_socket, &mut daemon_e2ee).await;
        assert_eq!(resize_request.kind, PacketKind::Request);
        assert_eq!(
            resize_request.method.as_deref(),
            Some(METHOD_SESSION_RESIZE)
        );

        for seq in 1..=(PENDING_PACKET_QUEUE_LIMIT as u64 + 1) {
            send_daemon_packet_for_test(
                &mut daemon_socket,
                &mut daemon_e2ee,
                ProtocolPacket::stream_chunk(
                    stream_id,
                    seq,
                    serde_json::to_value(TerminalFramePayload::Output {
                        session_id,
                        terminal_seq: seq,
                        data_base64: crypto::encode_session_data(b"queued-while-resizing"),
                    })
                    .expect("terminal frame should serialize"),
                ),
            )
            .await;
        }

        let error = client_task
            .await
            .expect("client task should join")
            .expect_err("pending packet queue should be bounded");
        assert!(matches!(error, TermctlError::PendingPacketQueueFull));
    }

    #[test]
    fn terminal_frame_tracking_records_latest_terminal_sequence() {
        let session_id = SessionId::new();
        let mut stream = TerminalStream::new();
        let frame = TerminalFramePayload::Batch {
            session_id,
            frames: vec![
                TerminalFramePayload::Output {
                    session_id,
                    terminal_seq: 7,
                    data_base64: crypto::encode_session_data(b"ok"),
                },
                TerminalFramePayload::Resize {
                    session_id,
                    terminal_seq: 8,
                    size: TerminalSize::new(24, 80),
                },
            ],
        };
        let (_events, credit) =
            terminal_frame_events_and_credit(frame.clone()).expect("batch should decode");
        stream.record_terminal_progress(terminal_frame_progress(&frame));

        assert_eq!(credit, 3);
        assert_eq!(stream.last_terminal_seq(), Some(8));
    }

    #[test]
    fn terminal_frame_batch_keeps_output_before_exit_for_termctl() {
        let session_id = SessionId::new();
        let (events, credit) = terminal_frame_events_and_credit(TerminalFramePayload::Batch {
            session_id,
            frames: vec![
                TerminalFramePayload::Output {
                    session_id,
                    terminal_seq: 1,
                    data_base64: crypto::encode_session_data(b"bye"),
                },
                TerminalFramePayload::Exit {
                    session_id,
                    terminal_seq: 2,
                    code: Some(0),
                },
            ],
        })
        .expect("batch should decode");

        assert_eq!(
            events,
            vec![
                TerminalStreamEvent::Output(b"bye".to_vec()),
                TerminalStreamEvent::End
            ]
        );
        assert_eq!(credit, 4);
    }

    #[test]
    fn device_relay_admission_carries_signed_device_shape() {
        let generated = crate::crypto::generate_device_identity();
        let signing_key =
            crate::crypto::decode_signing_key(&generated.device_signing_key_secret).unwrap();
        let admission =
            signed_device_relay_admission(ServerId::new(), generated.device_id, &signing_key);

        match admission {
            RelayAdmissionPayload::Device {
                device_id,
                nonce,
                timestamp_ms,
                signature,
            } => {
                assert_eq!(device_id, generated.device_id);
                assert!(nonce.0.starts_with("nonce-"));
                assert!(timestamp_ms.0 > 0);
                assert!(signature.0.starts_with("ed25519-v1:"));
            }
            other => panic!("expected device admission, got {other:?}"),
        }
    }

    #[test]
    fn encrypted_business_envelope_hides_pairing_and_session_plaintext() {
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            server_id,
            device_id,
            daemon_keypair.public_key(),
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            daemon_keypair.public_key(),
            context.clone(),
        )
        .unwrap();
        let mut daemon_e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon_keypair,
            device_keypair.public_key(),
            context,
        )
        .unwrap();
        let inner = packet_request_envelope(
            PacketRequestId::new(),
            METHOD_PAIR_REQUEST,
            PairRequestPayload {
                device_id,
                device_public_key: PublicKey("ed25519-v1:public".to_owned()),
                token: PairingToken("secret-token".to_owned()),
                nonce: crypto::nonce(),
                timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            },
        )
        .unwrap();

        let outer = encrypted_envelope_for_test(&mut device_e2ee, inner).unwrap();
        let wire = serde_json::to_string(&outer).unwrap();

        assert_eq!(outer.kind, MessageType::EncryptedFrame);
        assert!(!wire.contains("pair_request"));
        assert!(!wire.contains("pair.request"));
        assert!(!wire.contains("secret-token"));

        let frame: EncryptedFramePayload = decode_payload(outer.payload).unwrap();
        let decrypted: JsonEnvelope = daemon_e2ee.decrypt_json_payload(&frame).unwrap();
        assert_eq!(decrypted.kind, MessageType::Packet);
        let packet: ProtocolPacket<Value> = decode_payload(decrypted.payload).unwrap();
        assert_eq!(packet.kind, PacketKind::Request);
        assert_eq!(packet.method.as_deref(), Some(METHOD_PAIR_REQUEST));
    }

    #[test]
    fn encrypted_session_data_hides_terminal_bytes() {
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            server_id,
            device_id,
            daemon_keypair.public_key(),
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            daemon_keypair.public_key(),
            context,
        )
        .unwrap();
        let mut stream = TerminalStream::new();
        let packet = stream
            .data_chunk_packet(SessionId::new(), b"terminal secret\n")
            .unwrap();
        let inner = packet_envelope(packet).unwrap();

        let outer = encrypted_envelope_for_test(&mut device_e2ee, inner).unwrap();
        let wire = serde_json::to_string(&outer).unwrap();

        assert!(!wire.contains("session_data"));
        assert!(!wire.contains("stream_chunk"));
        assert!(!wire.contains("terminal secret"));
    }

    #[test]
    fn binary_packet_roundtrips_terminal_stream_data_without_json_payload() {
        let session_id = SessionId::new();
        let mut stream = TerminalStream::new();
        let packet = stream
            .data_chunk_packet(session_id, b"terminal secret\n")
            .unwrap();

        let binary = protocol_packet_to_binary(packet).unwrap();
        let encoded = encode_binary_protocol_packet(&binary);
        let decoded = protocol_packet_from_binary(
            decode_binary_protocol_packet(&encoded).expect("binary packet should decode"),
        )
        .expect("binary packet should map back to protocol packet");

        assert!(matches!(
            binary.payload,
            Some(binary_protocol_packet::Payload::SessionData(_))
        ));
        assert_eq!(decoded.kind, PacketKind::StreamChunk);
        assert_eq!(decoded.stream_id, Some(stream.id));
        let payload: SessionDataPayload = decode_payload(decoded.payload).unwrap();
        assert_eq!(payload.session_id, session_id);
        assert_eq!(
            crypto::decode_session_data(&payload.data_base64).unwrap(),
            b"terminal secret\n"
        );
    }

    #[test]
    fn binary_packet_roundtrips_file_chunk_without_becoming_terminal_data() {
        let session_id = SessionId::new();
        let stream_id = PacketStreamId::new();
        let raw = b"\x00file-bytes\xff".to_vec();
        let packet = ProtocolPacket::stream_chunk(
            stream_id,
            1,
            serde_json::to_value(SessionFileTransferChunkPayload {
                session_id,
                offset_bytes: 0,
                data_base64: crypto::encode_session_data(&raw),
                size_bytes: raw.len() as u64,
                eof: true,
            })
            .unwrap(),
        );

        let binary = protocol_packet_to_binary(packet).unwrap();
        let encoded = encode_binary_protocol_packet(&binary);
        let decoded = protocol_packet_from_binary(
            decode_binary_protocol_packet(&encoded).expect("binary packet should decode"),
        )
        .expect("binary packet should map back to protocol packet");

        assert!(matches!(
            binary.payload,
            Some(binary_protocol_packet::Payload::FileChunk(_))
        ));
        let payload: SessionFileTransferChunkPayload = decode_payload(decoded.payload).unwrap();
        assert_eq!(payload.session_id, session_id);
        assert_eq!(payload.offset_bytes, 0);
        assert_eq!(
            crypto::decode_session_data(&payload.data_base64).unwrap(),
            raw
        );
        assert!(payload.eof);
    }

    #[test]
    fn ping_packet_carries_nonce_and_timestamp_in_request_payload() {
        let request_id = PacketRequestId::new();
        let envelope = packet_request_envelope(
            request_id,
            METHOD_PING,
            PingPayload {
                nonce: crypto::nonce(),
                timestamp_ms: crypto::now_ms(),
            },
        )
        .expect("ping packet should serialize");
        let packet: ProtocolPacket<Value> = decode_payload(envelope.payload).unwrap();
        let payload: PingPayload = decode_payload(packet.payload).unwrap();

        assert_eq!(envelope.kind, MessageType::Packet);
        assert_eq!(packet.kind, PacketKind::Request);
        assert_eq!(packet.id, Some(request_id));
        assert_eq!(packet.method.as_deref(), Some(METHOD_PING));
        assert!(payload.nonce.0.starts_with("nonce-"));
        assert!(payload.timestamp_ms.0 > 0);
    }

    async fn connected_direct_client_for_test()
    -> (DirectClient, WebSocketStream<TcpStream>, E2eeSession) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener
            .local_addr()
            .expect("test listener should expose local addr");
        let server_socket_task = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("test server should accept websocket TCP");
            tokio_tungstenite::accept_async(stream)
                .await
                .expect("test server should accept websocket")
        });
        let (client_socket, _) = tokio_tungstenite::connect_async_tls_with_config(
            format!("ws://{addr}/ws"),
            None,
            false,
            Some(termctl_tls_connector()),
        )
        .await
        .expect("test client websocket should connect");
        let server_socket = server_socket_task
            .await
            .expect("test server websocket task should join");

        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let daemon_identity = DaemonIdentity::generate().public_identity();
        let daemon_keypair = E2eeKeyPair::generate();
        let device_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            server_id,
            device_id,
            daemon_keypair.public_key(),
            device_keypair.public_key(),
        );
        let daemon_e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon_keypair,
            device_keypair.public_key(),
            context,
        )
        .expect("daemon E2EE should initialize");
        (
            DirectClient {
                socket: client_socket,
                device_id,
                daemon_identity,
                binary_mode: false,
                pending_packets: VecDeque::new(),
            },
            server_socket,
            daemon_e2ee,
        )
    }

    async fn send_daemon_packet_for_test(
        socket: &mut WebSocketStream<TcpStream>,
        _daemon_e2ee: &mut E2eeSession,
        packet: ProtocolPacket<Value>,
    ) {
        let outer = packet_envelope(packet).expect("test packet should encode");
        let raw = serde_json::to_string(&outer).expect("test packet should serialize");
        socket
            .send(Message::Text(raw.into()))
            .await
            .expect("test daemon packet should send");
    }

    async fn read_client_packet_for_test(
        socket: &mut WebSocketStream<TcpStream>,
        _daemon_e2ee: &mut E2eeSession,
    ) -> ProtocolPacket<Value> {
        loop {
            let message = socket
                .next()
                .await
                .expect("client should send websocket message")
                .expect("client websocket message should be valid");
            let Message::Text(raw) = message else {
                continue;
            };
            let envelope: JsonEnvelope =
                serde_json::from_str(&raw).expect("client envelope should decode");
            assert_eq!(envelope.kind, MessageType::Packet);
            return decode_payload(envelope.payload).expect("client packet should decode");
        }
    }
}
