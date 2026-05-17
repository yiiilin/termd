//! direct WebSocket termd 客户端。
//!
//! WebSocket 打开后先发送明文 `route_hello` 并等待 `route_ready`，随后才进入
//! `hello`/`e2ee_key_exchange`/`encrypted_frame`。pair/auth/session/control 业务
//! 统一封装为 E2EE 内的 `packet`。relay 因而只能看到 server_id、sequence 和密文。

use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd::auth::{
    AuthSigningInput, DaemonE2eeSigningInput, DaemonPublicIdentity, E2eeAuthTranscript,
    SignatureVerifier,
};
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::signature::Ed25519SignatureVerifier;
use termd::net::{
    E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
};
use termd_proto::{
    AuthChallengePayload, AuthPayload, ControlGrantPayload, ControlRequestPayload, DeviceId,
    E2eeKeyExchangePayload, EncryptedFramePayload, ErrorPayload, MessageType,
    PROTOCOL_PACKET_VERSION, PacketErrorPayload, PacketKind, PacketRequestId, PacketStreamId,
    PairAcceptPayload, PairRequestPayload, PairingToken, PingPayload, PongPayload, ProtocolPacket,
    ProtocolVersion, PublicKey, RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
    SessionAttachPayload, SessionAttachedPayload, SessionCreatePayload, SessionCreatedPayload,
    SessionDataPayload, SessionId, SessionListPayload, SessionListResultPayload,
    SessionResizePayload, SessionResizedPayload, TerminalSize,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::crypto;
use crate::error::{Result, TermctlError};
use crate::state::PairedServerState;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const METHOD_PAIR_REQUEST: &str = "pair.request";
const METHOD_AUTH: &str = "auth";
const METHOD_AUTH_CHALLENGE: &str = "auth.challenge";
const METHOD_SESSION_CREATE: &str = "session.create";
const METHOD_SESSION_LIST: &str = "session.list";
const METHOD_SESSION_RESIZE: &str = "session.resize";
const METHOD_CONTROL_REQUEST: &str = "control.request";
const METHOD_TERMINAL_ATTACH: &str = "terminal.attach";
const METHOD_PING: &str = "ping";
const TERMINAL_STREAM_INITIAL_CREDIT: u32 = 64;
const TERMINAL_STREAM_REPLENISH_CREDIT: u32 = 16;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct TerminalStream {
    id: PacketStreamId,
    next_send_seq: u64,
    last_recv_seq: u64,
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

pub struct DirectClient {
    socket: WsStream,
    e2ee: E2eeSession,
    device_id: DeviceId,
    daemon_identity: DaemonPublicIdentity,
    e2ee_auth_transcript: E2eeAuthTranscript,
}

impl DirectClient {
    pub async fn connect(
        url: &str,
        route_server_id: ServerId,
        device_id: DeviceId,
        expected_daemon_public_key: PublicKey,
    ) -> Result<Self> {
        let (mut socket, _) = connect_async(url)
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
        let mut server_e2ee_exchange = None;

        // daemon 在连接建立后立即发送 hello 和 E2EE 公钥；顺序固定，但这里仍按类型收敛，
        // 便于后续兼容额外的明文握手字段。
        for _ in 0..2 {
            let envelope = timeout(HANDSHAKE_TIMEOUT, read_outer(&mut socket))
                .await
                .map_err(|_| TermctlError::ConnectionClosed)??;

            match envelope.kind {
                MessageType::Hello => {
                    let payload: termd_proto::HelloPayload = decode_payload(envelope.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope)?;
                    if payload
                        .server_id
                        .is_some_and(|server_id| server_id != route_server_id)
                    {
                        return Err(TermctlError::RouteServerMismatch);
                    }
                }
                MessageType::E2eeKeyExchange => {
                    let payload: E2eeKeyExchangePayload = decode_payload(envelope.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope)?;
                    if payload.server_id != route_server_id {
                        return Err(TermctlError::RouteServerMismatch);
                    }
                    verify_daemon_e2ee_key_exchange(&payload, &daemon_identity)?;
                    server_e2ee_exchange = Some(payload);
                }
                MessageType::Error => return Err(protocol_error(envelope.payload)),
                _ => return Err(TermctlError::UnexpectedMessage),
            }
        }

        let server_e2ee_exchange = server_e2ee_exchange.ok_or(TermctlError::InvalidEnvelope)?;
        let server_e2ee_key = E2eePeerPublicKey::try_from(&server_e2ee_exchange.public_key)
            .map_err(|_| TermctlError::E2eeFailed)?;
        let device_e2ee_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            route_server_id,
            device_id,
            server_e2ee_key,
            device_e2ee_keypair.public_key(),
        );
        let e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_e2ee_keypair,
            server_e2ee_key,
            context,
        )
        .map_err(|_| TermctlError::E2eeFailed)?;
        let device_e2ee_exchange = E2eeKeyExchangePayload::new(
            route_server_id,
            device_id,
            device_e2ee_keypair.public_key_wire(),
            crypto::nonce(),
            crypto::now_ms(),
        )
        .with_packet_version(ProtocolVersion(PROTOCOL_PACKET_VERSION));
        let e2ee_auth_transcript = E2eeAuthTranscript::from_key_exchanges(
            &server_e2ee_exchange,
            &device_e2ee_exchange,
            &daemon_identity,
        );
        let mut client = Self {
            socket,
            e2ee,
            device_id,
            daemon_identity,
            e2ee_auth_transcript,
        };

        client
            .send_outer(envelope_value(
                MessageType::E2eeKeyExchange,
                device_e2ee_exchange,
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
        let signing_input = AuthSigningInput::from_payload_with_e2ee_transcript(
            &auth,
            &self.daemon_identity,
            Some(&self.e2ee_auth_transcript),
        )
        .to_bytes();
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

    pub async fn attach_terminal_stream(
        &mut self,
        session_id: SessionId,
    ) -> Result<(SessionAttachedPayload, TerminalStream)> {
        let request_id = PacketRequestId::new();
        let stream = TerminalStream::new();
        let packet = packet_stream_open(
            request_id,
            stream.id,
            METHOD_TERMINAL_ATTACH,
            TERMINAL_STREAM_INITIAL_CREDIT,
            SessionAttachPayload {
                session_id,
                watch_updates: true,
            },
        )?;

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
        loop {
            let packet = self.receive_packet().await?;
            if packet.stream_id != Some(stream.id) {
                continue;
            }

            match packet.kind {
                PacketKind::StreamChunk => {
                    let payload: SessionDataPayload = decode_payload(packet.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope)?;
                    let bytes = crypto::decode_session_data(&payload.data_base64)?;
                    stream.last_recv_seq = packet.seq;
                    self.send_packet(
                        stream.flow_packet(packet.seq, TERMINAL_STREAM_REPLENISH_CREDIT),
                    )
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

    pub async fn receive_inner(&mut self) -> Result<JsonEnvelope> {
        let envelope = read_outer(&mut self.socket).await?;

        match envelope.kind {
            MessageType::EncryptedFrame => {
                let frame: EncryptedFramePayload =
                    decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)?;
                let inner: JsonEnvelope = self
                    .e2ee
                    .decrypt_json_payload(&frame)
                    .map_err(|_| TermctlError::E2eeFailed)?;
                if inner.kind == MessageType::Error {
                    return Err(protocol_error(inner.payload));
                }
                Ok(inner)
            }
            MessageType::Error => Err(protocol_error(envelope.payload)),
            _ => Err(TermctlError::UnexpectedMessage),
        }
    }

    async fn request_packet<P, T>(&mut self, method: &'static str, payload: P) -> Result<T>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let request_id = PacketRequestId::new();
        self.send_inner(packet_request_envelope(request_id, method, payload)?)
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
            let packet = self.receive_packet_timeout().await?;
            if let Some(payload) = decode_packet_response_for_request(packet, request_id, method)? {
                return Ok(payload);
            }
        }
    }

    async fn expect_packet_event<T>(&mut self, method: &'static str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            let packet = self.receive_packet_timeout().await?;
            validate_packet_version(&packet)?;

            match packet.kind {
                PacketKind::Event if packet.method.as_deref() == Some(method) => {
                    return decode_payload(packet.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope);
                }
                PacketKind::Error if packet.id.is_none() => {
                    return Err(packet_error(packet.payload));
                }
                _ => continue,
            }
        }
    }

    async fn send_packet(&mut self, packet: ProtocolPacket<Value>) -> Result<()> {
        self.send_inner(packet_envelope(packet)?).await
    }

    async fn receive_packet(&mut self) -> Result<ProtocolPacket<Value>> {
        let envelope = self.receive_inner().await?;
        if envelope.kind != MessageType::Packet {
            return Err(TermctlError::UnexpectedMessage);
        }

        let packet: ProtocolPacket<Value> =
            decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)?;
        validate_packet_version(&packet)?;
        Ok(packet)
    }

    async fn receive_packet_timeout(&mut self) -> Result<ProtocolPacket<Value>> {
        timeout(RESPONSE_TIMEOUT, self.receive_packet())
            .await
            .map_err(|_| TermctlError::ConnectionClosed)?
    }

    async fn send_inner(&mut self, inner: JsonEnvelope) -> Result<()> {
        let frame = self
            .e2ee
            .encrypt_json_payload(&inner)
            .map_err(|_| TermctlError::E2eeFailed)?;
        self.send_outer(envelope_value(MessageType::EncryptedFrame, frame)?)
            .await
    }

    async fn send_outer(&mut self, envelope: JsonEnvelope) -> Result<()> {
        let raw = serde_json::to_string(&envelope).map_err(|_| TermctlError::InvalidEnvelope)?;
        self.socket
            .send(Message::Text(raw.into()))
            .await
            .map_err(|_| TermctlError::SendFailed)
    }
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
    socket
        .send(Message::Text(raw.into()))
        .await
        .map_err(|_| TermctlError::SendFailed)
}

async fn read_outer(socket: &mut WsStream) -> Result<JsonEnvelope> {
    while let Some(message) = socket.next().await {
        let message = message.map_err(|_| TermctlError::ReceiveFailed)?;

        match message {
            Message::Text(raw) => {
                return serde_json::from_str(raw.as_str())
                    .map_err(|_| TermctlError::InvalidEnvelope);
            }
            Message::Binary(raw) => {
                return serde_json::from_slice(&raw).map_err(|_| TermctlError::InvalidEnvelope);
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
        ProtocolPacket, UnixTimestampMillis,
    };

    use super::*;

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
}
