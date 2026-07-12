//! termctl 的客户端侧加密和签名辅助。
//!
//! 这里只处理设备自己的 Ed25519 key、wire 编码、nonce 和 session_data 的 base64。
//! daemon 的 auth 规范化输入由 `termd::auth::AuthSigningInput` 提供，避免复制服务端规则。

use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use termd::auth::current_unix_timestamp_millis;
use termd_proto::{DeviceId, Nonce, PublicKey, Signature, UnixTimestampMillis};

use crate::error::{Result, TermctlError};

const ED25519_WIRE_PREFIX: &str = "ed25519-v1:";
const ED25519_SECRET_KEY_LEN: usize = 32;

#[derive(Debug, Clone)]
pub struct GeneratedDeviceIdentity {
    pub device_id: DeviceId,
    pub device_public_key: PublicKey,
    pub device_signing_key_secret: String,
}

pub fn generate_device_identity() -> GeneratedDeviceIdentity {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    GeneratedDeviceIdentity {
        device_id: DeviceId::new(),
        device_public_key: PublicKey(wire_prefixed(verifying_key.as_bytes())),
        device_signing_key_secret: wire_prefixed(&signing_key.to_bytes()),
    }
}

pub fn decode_signing_key(secret: &str) -> Result<SigningKey> {
    let bytes = decode_prefixed(secret, ED25519_SECRET_KEY_LEN)?;
    let bytes: [u8; ED25519_SECRET_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| TermctlError::InvalidDeviceKey)?;

    Ok(SigningKey::from_bytes(&bytes))
}

pub fn sign_to_wire(signing_key: &SigningKey, signing_input: &[u8]) -> Signature {
    let signature = signing_key.sign(signing_input);
    Signature(wire_prefixed(&signature.to_bytes()))
}

pub fn now_ms() -> UnixTimestampMillis {
    current_unix_timestamp_millis()
}

pub fn nonce() -> Nonce {
    Nonce(format!("nonce-{}", uuid::Uuid::new_v4()))
}

fn wire_prefixed(bytes: &[u8]) -> String {
    format!(
        "{ED25519_WIRE_PREFIX}{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

fn decode_prefixed(value: &str, expected_len: usize) -> Result<Vec<u8>> {
    let encoded = value
        .strip_prefix(ED25519_WIRE_PREFIX)
        .ok_or(TermctlError::InvalidDeviceKey)?;
    let bytes = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| TermctlError::InvalidDeviceKey)?;

    if bytes.len() != expected_len {
        return Err(TermctlError::InvalidDeviceKey);
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use termd::auth::{AuthPayload, AuthSigningInput, DaemonPublicIdentity, SignatureVerifier};
    use termd::net::signature::Ed25519SignatureVerifier;
    use termd_proto::{Challenge, ServerId};

    use super::*;

    #[test]
    fn generated_device_key_signs_daemon_auth_input() {
        let generated = generate_device_identity();
        let signing_key = decode_signing_key(&generated.device_signing_key_secret).unwrap();
        let daemon_identity = DaemonPublicIdentity {
            server_id: ServerId::new(),
            public_key: PublicKey("termd-daemon-public-test".to_owned()),
        };
        let mut payload = AuthPayload {
            device_id: generated.device_id,
            challenge: Challenge("challenge".to_owned()),
            nonce: nonce(),
            timestamp_ms: UnixTimestampMillis(1_710_000_000_000),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input = AuthSigningInput::from_payload(&payload, &daemon_identity).to_bytes();

        payload.signature = sign_to_wire(&signing_key, &signing_input);

        Ed25519SignatureVerifier
            .verify(
                &generated.device_public_key,
                &signing_input,
                &payload.signature,
            )
            .unwrap();
    }
}
