//! termd 的端到端加密传输边界。
//!
//! 本模块只负责已认证 daemon 与设备之间的会话密钥派生、帧加密、帧解密和序号防重放。
//! 它不做 pairing、challenge-response、controller/viewer 决策，也不让 relay 接触明文业务内容。

pub mod protocol;
pub mod pty_bridge;
pub mod relay;
pub mod server;
pub mod signature;

use std::fmt;

use base64::{Engine as _, engine::general_purpose};
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce as AeadNonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand_core::OsRng;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use termd_proto::{DeviceId, EncryptedFramePayload, PublicKey, ServerId};
use thiserror::Error;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

const PUBLIC_KEY_PREFIX: &str = "x25519-v1:";
const PROTOCOL_LABEL: &[u8] = b"termd-e2ee-v1";
const CLIENT_TO_SERVER_INFO: &[u8] = b"termd-e2ee-v1/client-to-server";
const SERVER_TO_CLIENT_INFO: &[u8] = b"termd-e2ee-v1/server-to-client";

/// E2EE 内核错误保持可判定，方便 WebSocket 层后续映射成关闭原因或错误消息。
#[derive(Debug, Error)]
pub enum E2eeError {
    #[error("unsupported E2EE public key prefix")]
    UnsupportedPublicKeyPrefix,
    #[error("invalid E2EE public key base64")]
    InvalidPublicKeyEncoding(#[source] base64::DecodeError),
    #[error("invalid X25519 public key length: expected 32 bytes, got {actual}")]
    InvalidPublicKeyLength { actual: usize },
    #[error("public key does not match E2EE context: {role}")]
    PublicKeyContextMismatch { role: &'static str },
    #[error("X25519 key exchange produced a non-contributory shared secret")]
    NonContributoryKeyExchange,
    #[error("failed to derive E2EE session keys")]
    KeyDerivation,
    #[error("encrypted frame server_id does not match E2EE context")]
    ServerIdMismatch,
    #[error("unexpected encrypted frame sequence: expected {expected}, received {received}")]
    UnexpectedSequence { expected: u64, received: u64 },
    #[error("E2EE sequence overflow")]
    SequenceOverflow,
    #[error("invalid encrypted frame base64")]
    InvalidCiphertextEncoding(#[source] base64::DecodeError),
    #[error("failed to encrypt E2EE frame")]
    EncryptFailed,
    #[error("failed to decrypt E2EE frame")]
    DecryptFailed,
    #[error("failed to process E2EE JSON payload")]
    Json(#[from] serde_json::Error),
}

/// X25519 公钥的稳定 wire 形式，和 auth 层历史占位字符串明确区分。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct E2eePeerPublicKey([u8; 32]);

impl E2eePeerPublicKey {
    fn from_x25519(public_key: X25519PublicKey) -> Self {
        Self(public_key.to_bytes())
    }

    /// 返回 proto 可承载的稳定 wire 公钥，例如 `x25519-v1:<base64>`。
    pub fn to_wire_public_key(self) -> PublicKey {
        PublicKey(self.to_wire_string())
    }

    pub fn to_wire_string(self) -> String {
        format!(
            "{PUBLIC_KEY_PREFIX}{}",
            general_purpose::STANDARD.encode(self.0)
        )
    }

    pub fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    fn to_x25519(self) -> X25519PublicKey {
        X25519PublicKey::from(self.0)
    }

    fn fingerprint(self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }
}

impl fmt::Debug for E2eePeerPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("E2eePeerPublicKey")
            .field(&self.to_wire_string())
            .finish()
    }
}

impl TryFrom<&PublicKey> for E2eePeerPublicKey {
    type Error = E2eeError;

    fn try_from(value: &PublicKey) -> Result<Self, Self::Error> {
        let encoded = value
            .0
            .strip_prefix(PUBLIC_KEY_PREFIX)
            .ok_or(E2eeError::UnsupportedPublicKeyPrefix)?;
        let bytes = general_purpose::STANDARD
            .decode(encoded)
            .map_err(E2eeError::InvalidPublicKeyEncoding)?;
        let actual = bytes.len();
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| E2eeError::InvalidPublicKeyLength { actual })?;

        Ok(Self(bytes))
    }
}

impl TryFrom<PublicKey> for E2eePeerPublicKey {
    type Error = E2eeError;

    fn try_from(value: PublicKey) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl From<E2eePeerPublicKey> for PublicKey {
    fn from(value: E2eePeerPublicKey) -> Self {
        value.to_wire_public_key()
    }
}

/// X25519 keypair 只暴露公钥；Debug 明确不输出私钥材料。
pub struct E2eeKeyPair {
    secret: StaticSecret,
    public_key: E2eePeerPublicKey,
}

impl E2eeKeyPair {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public_key = E2eePeerPublicKey::from_x25519(X25519PublicKey::from(&secret));

        Self { secret, public_key }
    }

    pub fn public_key(&self) -> E2eePeerPublicKey {
        self.public_key
    }

    pub fn public_key_wire(&self) -> PublicKey {
        self.public_key.to_wire_public_key()
    }
}

impl fmt::Debug for E2eeKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("E2eeKeyPair")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// 会话上下文进入 HKDF 和 AEAD associated data，防止密文跨 server/device/keypair 复用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eeSessionContext {
    server_id: ServerId,
    device_id: DeviceId,
    daemon_public_key: E2eePeerPublicKey,
    device_public_key: E2eePeerPublicKey,
}

impl E2eeSessionContext {
    pub fn new(
        server_id: ServerId,
        device_id: DeviceId,
        daemon_public_key: E2eePeerPublicKey,
        device_public_key: E2eePeerPublicKey,
    ) -> Self {
        Self {
            server_id,
            device_id,
            daemon_public_key,
            device_public_key,
        }
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn daemon_public_key(&self) -> E2eePeerPublicKey {
        self.daemon_public_key
    }

    pub fn device_public_key(&self) -> E2eePeerPublicKey {
        self.device_public_key
    }

    fn kdf_salt(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PROTOCOL_LABEL);
        hasher.update(b"/kdf-context");
        hasher.update(self.server_id.0.as_bytes());
        hasher.update(self.device_id.0.as_bytes());
        hasher.update(self.daemon_public_key.as_bytes());
        hasher.update(self.device_public_key.as_bytes());
        hasher.finalize().into()
    }

    fn associated_data(&self, sequence: u64) -> Vec<u8> {
        let mut data = Vec::with_capacity(16 + 16 + 8 + 32 + 32 + PROTOCOL_LABEL.len() + 8);
        data.extend_from_slice(PROTOCOL_LABEL);
        data.extend_from_slice(b"/aead");
        data.extend_from_slice(self.server_id.0.as_bytes());
        data.extend_from_slice(self.device_id.0.as_bytes());
        data.extend_from_slice(&sequence.to_be_bytes());
        data.extend_from_slice(&self.daemon_public_key.fingerprint());
        data.extend_from_slice(&self.device_public_key.fingerprint());
        data
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2eeSessionRole {
    Daemon,
    Device,
}

/// 单条已认证连接上的 E2EE 会话。发送和接收方向使用不同 AEAD key。
pub struct E2eeSession {
    context: E2eeSessionContext,
    send_cipher: ChaCha20Poly1305,
    receive_cipher: ChaCha20Poly1305,
    next_send_sequence: u64,
    next_receive_sequence: u64,
}

impl E2eeSession {
    pub fn new(
        role: E2eeSessionRole,
        local_keypair: &E2eeKeyPair,
        peer_public_key: E2eePeerPublicKey,
        context: E2eeSessionContext,
    ) -> Result<Self, E2eeError> {
        validate_context(role, local_keypair.public_key(), peer_public_key, &context)?;

        let shared_secret = local_keypair
            .secret
            .diffie_hellman(&peer_public_key.to_x25519());
        if !shared_secret.was_contributory() {
            return Err(E2eeError::NonContributoryKeyExchange);
        }

        let (client_to_server_key, server_to_client_key) =
            derive_direction_keys(shared_secret.as_bytes(), &context)?;
        let (send_key, receive_key) = match role {
            E2eeSessionRole::Device => (client_to_server_key, server_to_client_key),
            E2eeSessionRole::Daemon => (server_to_client_key, client_to_server_key),
        };

        Ok(Self {
            context,
            send_cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
            receive_cipher: ChaCha20Poly1305::new(Key::from_slice(&receive_key)),
            next_send_sequence: 0,
            next_receive_sequence: 0,
        })
    }

    /// 序列化内部业务 payload 后整体加密，relay 只能看到外层 frame。
    pub fn encrypt_json_payload<T>(
        &mut self,
        payload: &T,
    ) -> Result<EncryptedFramePayload, E2eeError>
    where
        T: Serialize,
    {
        let plaintext = serde_json::to_vec(payload)?;
        self.encrypt_bytes(&plaintext)
    }

    /// 解密后再反序列化内部业务 payload；解密失败不会推进接收序号。
    pub fn decrypt_json_payload<T>(&mut self, frame: &EncryptedFramePayload) -> Result<T, E2eeError>
    where
        T: DeserializeOwned,
    {
        let plaintext = self.decrypt_bytes(frame)?;
        Ok(serde_json::from_slice(&plaintext)?)
    }

    pub fn encrypt_bytes(&mut self, plaintext: &[u8]) -> Result<EncryptedFramePayload, E2eeError> {
        let sequence = self.next_send_sequence;
        let next_sequence = sequence.checked_add(1).ok_or(E2eeError::SequenceOverflow)?;
        let nonce = nonce_for_sequence(sequence);
        let associated_data = self.context.associated_data(sequence);
        let ciphertext = self
            .send_cipher
            .encrypt(
                AeadNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &associated_data,
                },
            )
            .map_err(|_| E2eeError::EncryptFailed)?;

        self.next_send_sequence = next_sequence;

        Ok(EncryptedFramePayload {
            server_id: self.context.server_id,
            sequence,
            ciphertext_base64: general_purpose::STANDARD.encode(ciphertext),
        })
    }

    pub fn decrypt_bytes(&mut self, frame: &EncryptedFramePayload) -> Result<Vec<u8>, E2eeError> {
        if frame.server_id != self.context.server_id {
            return Err(E2eeError::ServerIdMismatch);
        }

        let expected = self.next_receive_sequence;
        if frame.sequence != expected {
            return Err(E2eeError::UnexpectedSequence {
                expected,
                received: frame.sequence,
            });
        }

        let next_sequence = expected.checked_add(1).ok_or(E2eeError::SequenceOverflow)?;
        let ciphertext = general_purpose::STANDARD
            .decode(&frame.ciphertext_base64)
            .map_err(E2eeError::InvalidCiphertextEncoding)?;
        let nonce = nonce_for_sequence(frame.sequence);
        let associated_data = self.context.associated_data(frame.sequence);
        let plaintext = self
            .receive_cipher
            .decrypt(
                AeadNonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext.as_slice(),
                    aad: &associated_data,
                },
            )
            .map_err(|_| E2eeError::DecryptFailed)?;

        self.next_receive_sequence = next_sequence;

        Ok(plaintext)
    }
}

fn validate_context(
    role: E2eeSessionRole,
    local_public_key: E2eePeerPublicKey,
    peer_public_key: E2eePeerPublicKey,
    context: &E2eeSessionContext,
) -> Result<(), E2eeError> {
    match role {
        E2eeSessionRole::Daemon => {
            if local_public_key != context.daemon_public_key {
                return Err(E2eeError::PublicKeyContextMismatch {
                    role: "local daemon public key",
                });
            }
            if peer_public_key != context.device_public_key {
                return Err(E2eeError::PublicKeyContextMismatch {
                    role: "peer device public key",
                });
            }
        }
        E2eeSessionRole::Device => {
            if local_public_key != context.device_public_key {
                return Err(E2eeError::PublicKeyContextMismatch {
                    role: "local device public key",
                });
            }
            if peer_public_key != context.daemon_public_key {
                return Err(E2eeError::PublicKeyContextMismatch {
                    role: "peer daemon public key",
                });
            }
        }
    }

    Ok(())
}

fn derive_direction_keys(
    shared_secret: &[u8; 32],
    context: &E2eeSessionContext,
) -> Result<([u8; 32], [u8; 32]), E2eeError> {
    let salt = context.kdf_salt();
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared_secret);
    let mut client_to_server_key = [0u8; 32];
    let mut server_to_client_key = [0u8; 32];

    hkdf.expand(CLIENT_TO_SERVER_INFO, &mut client_to_server_key)
        .map_err(|_| E2eeError::KeyDerivation)?;
    hkdf.expand(SERVER_TO_CLIENT_INFO, &mut server_to_client_key)
        .map_err(|_| E2eeError::KeyDerivation)?;

    Ok((client_to_server_key, server_to_client_key))
}

fn nonce_for_sequence(sequence: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use termd_proto::{DeviceId, EncryptedFramePayload, PublicKey, ServerId};

    use super::*;

    fn test_context(
        daemon: &E2eeKeyPair,
        device: &E2eeKeyPair,
    ) -> (ServerId, DeviceId, E2eeSessionContext) {
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let context = E2eeSessionContext::new(
            server_id,
            device_id,
            daemon.public_key(),
            device.public_key(),
        );

        (server_id, device_id, context)
    }

    #[test]
    fn net_x25519_public_key_wire_encoding_roundtrips_without_debug_private_key() {
        let keypair = E2eeKeyPair::generate();
        let public_key = keypair.public_key();
        let wire = public_key.to_wire_public_key();
        let parsed = E2eePeerPublicKey::try_from(&wire).expect("public key should parse");
        let debug = format!("{keypair:?}");

        assert_eq!(wire.0.split_once(':').unwrap().0, "x25519-v1");
        assert_eq!(public_key, parsed);
        assert!(!debug.to_lowercase().contains("private"));
        assert!(!debug.to_lowercase().contains("secret"));
    }

    #[test]
    fn net_rejects_non_contributory_x25519_public_key() {
        let daemon = E2eeKeyPair::generate();
        let zero_peer = E2eePeerPublicKey::try_from(&PublicKey(
            "x25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
        ))
        .unwrap();
        let context = E2eeSessionContext::new(
            ServerId::new(),
            DeviceId::new(),
            daemon.public_key(),
            zero_peer,
        );

        let result = E2eeSession::new(E2eeSessionRole::Daemon, &daemon, zero_peer, context);

        assert!(matches!(result, Err(E2eeError::NonContributoryKeyExchange)));
    }

    #[test]
    fn net_device_and_daemon_derive_bidirectional_session_and_hide_plaintext() {
        let daemon = E2eeKeyPair::generate();
        let device = E2eeKeyPair::generate();
        let (server_id, _, context) = test_context(&daemon, &device);
        let mut device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device,
            daemon.public_key(),
            context.clone(),
        )
        .unwrap();
        let mut daemon_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            context,
        )
        .unwrap();
        let payload = json!({
            "type": "session_data",
            "payload": {
                "session_id": "internal-session",
                "data_base64": "dGVybWluYWwtb3V0cHV0"
            }
        });

        let frame = device_session.encrypt_json_payload(&payload).unwrap();
        let wire_json = serde_json::to_string(&frame).unwrap();
        let decrypted: serde_json::Value = daemon_session.decrypt_json_payload(&frame).unwrap();

        assert_eq!(frame.server_id, server_id);
        assert_eq!(frame.sequence, 0);
        assert_eq!(decrypted, payload);
        assert!(!wire_json.contains("session_data"));
        assert!(!wire_json.contains("internal-session"));
        assert!(!wire_json.contains("dGVybWluYWwtb3V0cHV0"));
    }

    #[test]
    fn net_rejects_tampered_context_sequence_ciphertext_and_replay() {
        let daemon = E2eeKeyPair::generate();
        let device = E2eeKeyPair::generate();
        let (_, _, context) = test_context(&daemon, &device);
        let payload = json!({"type": "session_resize", "payload": {"session_id": "s", "rows": 24}});

        let mut device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device,
            daemon.public_key(),
            context.clone(),
        )
        .unwrap();
        let frame = device_session.encrypt_json_payload(&payload).unwrap();

        let mut daemon_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            context.clone(),
        )
        .unwrap();
        let mut wrong_server = frame.clone();
        wrong_server.server_id = ServerId::new();
        assert!(
            daemon_session
                .decrypt_json_payload::<serde_json::Value>(&wrong_server)
                .is_err()
        );

        let mut daemon_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            context.clone(),
        )
        .unwrap();
        let mut wrong_sequence = frame.clone();
        wrong_sequence.sequence = 1;
        assert!(
            daemon_session
                .decrypt_json_payload::<serde_json::Value>(&wrong_sequence)
                .is_err()
        );

        let mut daemon_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            context.clone(),
        )
        .unwrap();
        let mut wrong_ciphertext = frame.clone();
        wrong_ciphertext.ciphertext_base64.push('A');
        assert!(
            daemon_session
                .decrypt_json_payload::<serde_json::Value>(&wrong_ciphertext)
                .is_err()
        );

        let mut daemon_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            context,
        )
        .unwrap();
        let _: serde_json::Value = daemon_session.decrypt_json_payload(&frame).unwrap();
        assert!(
            daemon_session
                .decrypt_json_payload::<serde_json::Value>(&frame)
                .is_err()
        );
    }

    #[test]
    fn net_context_binds_device_id_and_public_keys() {
        let daemon = E2eeKeyPair::generate();
        let device = E2eeKeyPair::generate();
        let (_, _, context) = test_context(&daemon, &device);
        let wrong_device_context = E2eeSessionContext::new(
            context.server_id(),
            DeviceId::new(),
            daemon.public_key(),
            device.public_key(),
        );
        let other_device = E2eeKeyPair::generate();
        let wrong_public_key_context = E2eeSessionContext::new(
            context.server_id(),
            context.device_id(),
            daemon.public_key(),
            other_device.public_key(),
        );

        let mut sender = E2eeSession::new(
            E2eeSessionRole::Device,
            &device,
            daemon.public_key(),
            context,
        )
        .unwrap();
        let frame = sender
            .encrypt_json_payload(&json!({"terminal": "secret"}))
            .unwrap();

        let mut wrong_device_receiver = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &daemon,
            device.public_key(),
            wrong_device_context,
        )
        .unwrap();
        assert!(
            wrong_device_receiver
                .decrypt_json_payload::<serde_json::Value>(&frame)
                .is_err()
        );

        assert!(
            E2eeSession::new(
                E2eeSessionRole::Daemon,
                &daemon,
                device.public_key(),
                wrong_public_key_context,
            )
            .is_err()
        );
    }

    #[test]
    fn net_repeated_plaintext_uses_distinct_sequences_and_ciphertexts() {
        let daemon = E2eeKeyPair::generate();
        let device = E2eeKeyPair::generate();
        let (_, _, context) = test_context(&daemon, &device);
        let mut session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device,
            daemon.public_key(),
            context,
        )
        .unwrap();
        let payload = json!({"type": "session_data", "payload": {"data_base64": "same"}});

        let first: EncryptedFramePayload = session.encrypt_json_payload(&payload).unwrap();
        let second: EncryptedFramePayload = session.encrypt_json_payload(&payload).unwrap();

        assert_eq!(first.sequence, 0);
        assert_eq!(second.sequence, 1);
        assert_ne!(first.ciphertext_base64, second.ciphertext_base64);
    }
}
