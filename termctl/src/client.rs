//! direct WebSocket termd 客户端。
//!
//! 外层只发送 `hello`/`e2ee_key_exchange`/`encrypted_frame`，pair/auth/session/control
//! 业务 envelope 都放进 E2EE 密文。relay 因而只能看到 server_id、sequence 和密文。

use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use termd::auth::{AuthSigningInput, DaemonPublicIdentity};
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::{
    E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
};
use termd_proto::{
    AuthChallengePayload, AuthPayload, ControlGrantPayload, ControlRequestPayload, DeviceId,
    E2eeKeyExchangePayload, EncryptedFramePayload, ErrorPayload, MessageType, PairAcceptPayload,
    PairRequestPayload, PairingToken, PingPayload, PublicKey, ServerId, SessionAttachPayload,
    SessionAttachedPayload, SessionCreatePayload, SessionCreatedPayload, SessionDataPayload,
    SessionId, SessionListPayload, SessionListResultPayload, SessionResizePayload, TerminalSize,
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

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct DirectClient {
    socket: WsStream,
    e2ee: E2eeSession,
    server_id: ServerId,
    device_id: DeviceId,
}

impl DirectClient {
    pub async fn connect(url: &str, device_id: DeviceId) -> Result<Self> {
        let (mut socket, _) = connect_async(url)
            .await
            .map_err(|_| TermctlError::ConnectFailed)?;
        let mut server_id = None;
        let mut server_e2ee_key = None;

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
                    server_id = payload.server_id.or(server_id);
                }
                MessageType::E2eeKeyExchange => {
                    let payload: E2eeKeyExchangePayload = decode_payload(envelope.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope)?;
                    server_id = Some(payload.server_id);
                    server_e2ee_key = Some(
                        E2eePeerPublicKey::try_from(&payload.public_key)
                            .map_err(|_| TermctlError::E2eeFailed)?,
                    );
                }
                MessageType::Error => return Err(protocol_error(envelope.payload)),
                _ => return Err(TermctlError::UnexpectedMessage),
            }
        }

        let server_id = server_id.ok_or(TermctlError::InvalidEnvelope)?;
        let server_e2ee_key = server_e2ee_key.ok_or(TermctlError::InvalidEnvelope)?;
        let device_e2ee_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            server_id,
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
        let mut client = Self {
            socket,
            e2ee,
            server_id,
            device_id,
        };

        client
            .send_outer(envelope_value(
                MessageType::E2eeKeyExchange,
                E2eeKeyExchangePayload {
                    server_id,
                    device_id,
                    public_key: device_e2ee_keypair.public_key_wire(),
                    nonce: crypto::nonce(),
                    timestamp_ms: crypto::now_ms(),
                },
            )?)
            .await?;

        Ok(client)
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    pub async fn pair(
        &mut self,
        device_public_key: PublicKey,
        token: String,
    ) -> Result<PairAcceptPayload> {
        self.send_inner(envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id: self.device_id,
                device_public_key,
                token: PairingToken(token),
                nonce: crypto::nonce(),
                timestamp_ms: crypto::now_ms(),
            },
        )?)
        .await?;

        // 已信任 device 再次执行 pair 时，daemon 会先按认证路径预发 auth_challenge。
        // pairing token 仍由 pair_request 校验；CLI 只需要忽略这个与重新配对无关的挑战。
        self.expect_payload_ignoring(MessageType::PairAccept, &[MessageType::AuthChallenge])
            .await
    }

    pub async fn authenticate(
        &mut self,
        signing_key: &SigningKey,
        paired_server: &PairedServerState,
    ) -> Result<()> {
        let challenge: AuthChallengePayload = timeout(RESPONSE_TIMEOUT, async {
            let envelope = self.receive_inner().await?;
            if envelope.kind != MessageType::AuthChallenge {
                return Err(TermctlError::UnexpectedMessage);
            }

            decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope)
        })
        .await
        .map_err(|_| TermctlError::AuthChallengeTimeout)??;

        let mut auth = AuthPayload {
            device_id: self.device_id,
            challenge: challenge.challenge,
            nonce: crypto::nonce(),
            timestamp_ms: crypto::now_ms(),
            signature: termd_proto::Signature("ed25519-v1:placeholder".to_owned()),
        };
        let daemon_identity = DaemonPublicIdentity {
            server_id: paired_server.server_id,
            public_key: paired_server.daemon_public_key.clone(),
        };
        let signing_input = AuthSigningInput::from_payload(&auth, &daemon_identity).to_bytes();
        auth.signature = crypto::sign_to_wire(signing_key, &signing_input);

        self.send_inner(envelope_value(MessageType::Auth, auth)?)
            .await
    }

    pub async fn create_session(
        &mut self,
        command: Vec<String>,
        size: TerminalSize,
    ) -> Result<SessionCreatedPayload> {
        self.send_inner(envelope_value(
            MessageType::SessionCreate,
            SessionCreatePayload { command, size },
        )?)
        .await?;
        self.expect_payload(MessageType::SessionCreated).await
    }

    pub async fn attach_session(
        &mut self,
        session_id: SessionId,
    ) -> Result<SessionAttachedPayload> {
        self.send_inner(envelope_value(
            MessageType::SessionAttach,
            SessionAttachPayload { session_id },
        )?)
        .await?;
        self.expect_payload(MessageType::SessionAttached).await
    }

    pub async fn request_control(&mut self, session_id: SessionId) -> Result<ControlGrantPayload> {
        self.send_inner(envelope_value(
            MessageType::ControlRequest,
            ControlRequestPayload {
                session_id,
                device_id: self.device_id,
            },
        )?)
        .await?;

        // control 在 shared-control 模式下是 noop 确认命令；仍跳过 attach 后可能先到的输出帧。
        loop {
            let envelope = self.receive_inner_timeout().await?;
            match envelope.kind {
                MessageType::ControlGrant => {
                    return decode_payload(envelope.payload)
                        .map_err(|_| TermctlError::InvalidEnvelope);
                }
                MessageType::Pong | MessageType::SessionData => continue,
                _ => return Err(TermctlError::UnexpectedMessage),
            }
        }
    }

    pub async fn resize_session(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> Result<()> {
        self.send_inner(envelope_value(
            MessageType::SessionResize,
            SessionResizePayload { session_id, size },
        )?)
        .await?;
        self.send_ping().await?;

        // resize 成功没有业务 ack；随后发送一个加密 ping，用 pong 证明 daemon 已处理到
        // resize 后的帧。若 resize 失败，error 会排在 pong 前返回。
        loop {
            let envelope = self.receive_inner_timeout().await?;
            match envelope.kind {
                MessageType::Pong => return Ok(()),
                MessageType::SessionData => continue,
                _ => return Err(TermctlError::UnexpectedMessage),
            }
        }
    }

    pub async fn list_sessions(&mut self) -> Result<SessionListResultPayload> {
        self.send_inner(envelope_value(
            MessageType::SessionList,
            SessionListPayload {},
        )?)
        .await?;
        self.expect_payload(MessageType::SessionListResult).await
    }

    pub async fn send_session_data(&mut self, session_id: SessionId, bytes: &[u8]) -> Result<()> {
        self.send_inner(envelope_value(
            MessageType::SessionData,
            SessionDataPayload {
                session_id,
                data_base64: crypto::encode_session_data(bytes),
            },
        )?)
        .await
    }

    pub async fn send_ping(&mut self) -> Result<()> {
        self.send_inner(envelope_value(
            MessageType::Ping,
            PingPayload {
                nonce: crypto::nonce(),
                timestamp_ms: crypto::now_ms(),
            },
        )?)
        .await
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

    async fn receive_inner_timeout(&mut self) -> Result<JsonEnvelope> {
        timeout(RESPONSE_TIMEOUT, self.receive_inner())
            .await
            .map_err(|_| TermctlError::ConnectionClosed)?
    }

    async fn expect_payload<T>(&mut self, expected: MessageType) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.expect_payload_ignoring(expected, &[]).await
    }

    async fn expect_payload_ignoring<T>(
        &mut self,
        expected: MessageType,
        ignored: &[MessageType],
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            let envelope = self.receive_inner_timeout().await?;
            if envelope.kind == MessageType::Pong {
                continue;
            }
            if ignored.contains(&envelope.kind) {
                continue;
            }
            if envelope.kind != expected {
                return Err(TermctlError::UnexpectedMessage);
            }

            return decode_payload(envelope.payload).map_err(|_| TermctlError::InvalidEnvelope);
        }
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

#[cfg(test)]
fn ping_envelope_for_test() -> JsonEnvelope {
    envelope_value(
        MessageType::Ping,
        PingPayload {
            nonce: crypto::nonce(),
            timestamp_ms: crypto::now_ms(),
        },
    )
    .expect("ping payload should serialize")
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
    use termd::net::{E2eeKeyPair, E2eeSessionContext};
    use termd_proto::{PairingToken, UnixTimestampMillis};

    use super::*;

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
        let inner = envelope_value(
            MessageType::PairRequest,
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
        assert!(!wire.contains("secret-token"));

        let frame: EncryptedFramePayload = decode_payload(outer.payload).unwrap();
        let decrypted: JsonEnvelope = daemon_e2ee.decrypt_json_payload(&frame).unwrap();
        assert_eq!(decrypted.kind, MessageType::PairRequest);
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
        let inner = envelope_value(
            MessageType::SessionData,
            SessionDataPayload {
                session_id: SessionId::new(),
                data_base64: crypto::encode_session_data(b"terminal secret\n"),
            },
        )
        .unwrap();

        let outer = encrypted_envelope_for_test(&mut device_e2ee, inner).unwrap();
        let wire = serde_json::to_string(&outer).unwrap();

        assert!(!wire.contains("session_data"));
        assert!(!wire.contains("terminal secret"));
    }

    #[test]
    fn ping_envelope_carries_only_nonce_and_timestamp() {
        let ping = ping_envelope_for_test();
        let payload: PingPayload = decode_payload(ping.payload).unwrap();

        assert_eq!(ping.kind, MessageType::Ping);
        assert!(payload.nonce.0.starts_with("nonce-"));
        assert!(payload.timestamp_ms.0 > 0);
    }
}
