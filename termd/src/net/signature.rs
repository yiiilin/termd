//! WebSocket 协议层使用的 Ed25519 设备签名验证器。
//!
//! auth 模块只定义 `SignatureVerifier` trait；真实算法放在 net 边界，避免反向修改
//! 上游 auth trait，同时让 wire 编码约定集中在网络协议层。

use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};

use crate::auth::{SignatureError, SignatureVerifier};
use termd_proto::{PublicKey, Signature};

const ED25519_WIRE_PREFIX: &str = "ed25519-v1:";
const ED25519_PUBLIC_KEY_LEN: usize = 32;
const ED25519_SIGNATURE_LEN: usize = 64;

/// 设备公钥与签名的 wire 编码均为 `ed25519-v1:<base64>`。
#[derive(Debug, Clone, Copy, Default)]
pub struct Ed25519SignatureVerifier;

impl Ed25519SignatureVerifier {
    fn decode_prefixed(value: &str, expected_len: usize) -> Result<Vec<u8>, SignatureError> {
        let encoded = value
            .strip_prefix(ED25519_WIRE_PREFIX)
            .ok_or(SignatureError::InvalidSignature)?;
        let bytes = general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| SignatureError::InvalidSignature)?;

        if bytes.len() != expected_len {
            return Err(SignatureError::InvalidSignature);
        }

        Ok(bytes)
    }
}

impl SignatureVerifier for Ed25519SignatureVerifier {
    fn verify(
        &self,
        device_public_key: &PublicKey,
        signing_input: &[u8],
        signature: &Signature,
    ) -> Result<(), SignatureError> {
        let public_key_bytes = Self::decode_prefixed(&device_public_key.0, ED25519_PUBLIC_KEY_LEN)?;
        let signature_bytes = Self::decode_prefixed(&signature.0, ED25519_SIGNATURE_LEN)?;
        let public_key_bytes: [u8; ED25519_PUBLIC_KEY_LEN] = public_key_bytes
            .try_into()
            .map_err(|_| SignatureError::InvalidSignature)?;

        let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
            .map_err(|_| SignatureError::InvalidSignature)?;
        let signature = Ed25519Signature::from_slice(&signature_bytes)
            .map_err(|_| SignatureError::InvalidSignature)?;

        verifying_key
            .verify(signing_input, &signature)
            .map_err(|_| SignatureError::InvalidSignature)
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;

    use super::*;

    fn wire(bytes: &[u8]) -> String {
        format!(
            "{ED25519_WIRE_PREFIX}{}",
            general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn ed25519_verifier_accepts_valid_wire_key_and_signature() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let input = b"canonical auth input";
        let signature = signing_key.sign(input);
        let verifier = Ed25519SignatureVerifier;

        verifier
            .verify(
                &PublicKey(wire(verifying_key.as_bytes())),
                input,
                &Signature(wire(&signature.to_bytes())),
            )
            .unwrap();
    }

    #[test]
    fn ed25519_verifier_rejects_wrong_prefix_and_length() {
        let verifier = Ed25519SignatureVerifier;
        let input = b"canonical auth input";
        let signature = Signature(wire(&[7_u8; ED25519_SIGNATURE_LEN]));

        assert_eq!(
            verifier
                .verify(
                    &PublicKey("x25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned()),
                    input,
                    &signature,
                )
                .unwrap_err(),
            SignatureError::InvalidSignature
        );
        assert_eq!(
            verifier
                .verify(&PublicKey(wire(&[1_u8; 4])), input, &signature)
                .unwrap_err(),
            SignatureError::InvalidSignature
        );
    }

    #[test]
    fn ed25519_verifier_rejects_tampered_signature() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let signature = signing_key.sign(b"original");
        let verifier = Ed25519SignatureVerifier;

        assert_eq!(
            verifier
                .verify(
                    &PublicKey(wire(verifying_key.as_bytes())),
                    b"tampered",
                    &Signature(wire(&signature.to_bytes())),
                )
                .unwrap_err(),
            SignatureError::InvalidSignature
        );
    }
}
