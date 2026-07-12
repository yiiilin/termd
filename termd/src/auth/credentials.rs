use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ed25519_dalek::{Signature as DalekSignature, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use termd_proto::{AuthPayload, DeviceId, PublicKey, ServerId, UnixTimestampMillis};
use uuid::Uuid;

use super::DaemonIdentity;

const ED25519_WIRE_PREFIX: &str = "ed25519-v1:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    PairTicket,
    DeviceCertificate,
    AccessToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialClaims {
    pub kind: CredentialKind,
    pub issuer: ServerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_public_key: Option<PublicKey>,
    pub issued_at_ms: UnixTimestampMillis,
    pub expires_at_ms: UnixTimestampMillis,
    pub credential_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    Malformed,
    UnsupportedAlgorithm,
    WrongKeyId,
    WrongIssuer,
    WrongKind,
    Expired,
    InvalidSignature,
    SigningFailed,
}

#[derive(Debug, Serialize, Deserialize)]
struct JwsHeader {
    alg: String,
    kid: String,
    typ: String,
}

#[derive(Clone)]
pub struct CredentialService {
    identity: DaemonIdentity,
}

pub struct AccessTokenProofInput<'a> {
    pub server_id: ServerId,
    pub payload: &'a AuthPayload,
}

impl AccessTokenProofInput<'_> {
    pub fn to_bytes(&self) -> Vec<u8> {
        format!(
            "termd-access-token-v1\nserver_id={}\ndevice_id={}\nchallenge={}\nnonce={}\ntimestamp_ms={}\n",
            self.server_id.0,
            self.payload.device_id.0,
            self.payload.challenge.0,
            self.payload.nonce.0,
            self.payload.timestamp_ms.0,
        )
        .into_bytes()
    }
}

impl CredentialService {
    pub fn new(identity: DaemonIdentity) -> Self {
        Self { identity }
    }

    pub fn issue_pair_ticket(
        &self,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Result<String, CredentialError> {
        self.sign(CredentialClaims {
            kind: CredentialKind::PairTicket,
            issuer: self.identity.server_id(),
            device_id: None,
            device_public_key: None,
            issued_at_ms,
            expires_at_ms,
            credential_id: Uuid::new_v4().to_string(),
        })
    }

    pub fn issue_device_certificate(
        &self,
        device_id: DeviceId,
        device_public_key: PublicKey,
        issued_at_ms: UnixTimestampMillis,
    ) -> Result<String, CredentialError> {
        self.sign(CredentialClaims {
            kind: CredentialKind::DeviceCertificate,
            issuer: self.identity.server_id(),
            device_id: Some(device_id),
            device_public_key: Some(device_public_key),
            issued_at_ms,
            expires_at_ms: UnixTimestampMillis(u64::MAX),
            credential_id: Uuid::new_v4().to_string(),
        })
    }

    pub fn issue_access_token(
        &self,
        device_id: DeviceId,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Result<String, CredentialError> {
        self.sign(CredentialClaims {
            kind: CredentialKind::AccessToken,
            issuer: self.identity.server_id(),
            device_id: Some(device_id),
            device_public_key: None,
            issued_at_ms,
            expires_at_ms,
            credential_id: Uuid::new_v4().to_string(),
        })
    }

    fn sign(&self, claims: CredentialClaims) -> Result<String, CredentialError> {
        let header = JwsHeader {
            alg: "EdDSA".to_owned(),
            kid: self.identity.server_id().0.to_string(),
            typ: "JWT".to_owned(),
        };
        let header = serde_json::to_vec(&header).map_err(|_| CredentialError::SigningFailed)?;
        let claims = serde_json::to_vec(&claims).map_err(|_| CredentialError::SigningFailed)?;
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(claims)
        );
        let signature = self
            .identity
            .sign_to_wire(signing_input.as_bytes())
            .map_err(|_| CredentialError::SigningFailed)?;
        let signature = signature
            .0
            .strip_prefix(ED25519_WIRE_PREFIX)
            .ok_or(CredentialError::SigningFailed)?;
        let signature = STANDARD
            .decode(signature)
            .map_err(|_| CredentialError::SigningFailed)?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

pub fn verify_credential(
    compact: &str,
    daemon_public_key: &PublicKey,
    expected_server_id: ServerId,
    now_ms: UnixTimestampMillis,
    expected_kind: CredentialKind,
) -> Result<CredentialClaims, CredentialError> {
    let mut segments = compact.split('.');
    let encoded_header = segments.next().ok_or(CredentialError::Malformed)?;
    let encoded_claims = segments.next().ok_or(CredentialError::Malformed)?;
    let encoded_signature = segments.next().ok_or(CredentialError::Malformed)?;
    if segments.next().is_some() || encoded_signature.is_empty() {
        return Err(CredentialError::Malformed);
    }
    let header: JwsHeader = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_header)
            .map_err(|_| CredentialError::Malformed)?,
    )
    .map_err(|_| CredentialError::Malformed)?;
    if header.alg != "EdDSA" || header.typ != "JWT" {
        return Err(CredentialError::UnsupportedAlgorithm);
    }
    if header.kid != expected_server_id.0.to_string() {
        return Err(CredentialError::WrongKeyId);
    }
    let claims: CredentialClaims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_claims)
            .map_err(|_| CredentialError::Malformed)?,
    )
    .map_err(|_| CredentialError::Malformed)?;
    if claims.issuer != expected_server_id {
        return Err(CredentialError::WrongIssuer);
    }
    if claims.kind != expected_kind {
        return Err(CredentialError::WrongKind);
    }
    if now_ms.0 > claims.expires_at_ms.0 {
        return Err(CredentialError::Expired);
    }
    let public_bytes = daemon_public_key
        .0
        .strip_prefix(ED25519_WIRE_PREFIX)
        .ok_or(CredentialError::InvalidSignature)
        .and_then(|value| {
            STANDARD
                .decode(value)
                .map_err(|_| CredentialError::InvalidSignature)
        })?;
    let verifying_key = VerifyingKey::from_bytes(
        public_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CredentialError::InvalidSignature)?,
    )
    .map_err(|_| CredentialError::InvalidSignature)?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(encoded_signature)
        .map_err(|_| CredentialError::Malformed)?;
    let signature =
        DalekSignature::from_slice(&signature_bytes).map_err(|_| CredentialError::Malformed)?;
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    verifying_key
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| CredentialError::InvalidSignature)?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::DaemonIdentity;
    use termd_proto::{DeviceId, PublicKey, ServerId, UnixTimestampMillis};

    #[test]
    fn daemon_signed_credentials_are_fixed_algorithm_compact_jws() {
        let server_id = ServerId::new();
        let identity = DaemonIdentity::generate_for_server_id(server_id);
        let service = CredentialService::new(identity.clone());
        let issued = UnixTimestampMillis(1_000);
        let expires = UnixTimestampMillis(301_000);
        let token = service
            .issue_access_token(DeviceId::new(), issued, expires)
            .expect("access token should sign");

        assert_eq!(token.split('.').count(), 3);
        let claims = verify_credential(
            &token,
            identity.public_key(),
            server_id,
            UnixTimestampMillis(2_000),
            CredentialKind::AccessToken,
        )
        .expect("daemon public key should verify token");
        assert_eq!(claims.expires_at_ms, expires);
        assert!(claims.device_id.is_some());
    }

    #[test]
    fn credential_rejects_wrong_kind_expiry_and_key() {
        let identity = DaemonIdentity::generate();
        let other = DaemonIdentity::generate();
        let server_id = identity.server_id();
        let token = CredentialService::new(identity.clone())
            .issue_pair_ticket(UnixTimestampMillis(100), UnixTimestampMillis(200))
            .unwrap();

        assert_eq!(
            verify_credential(
                &token,
                identity.public_key(),
                server_id,
                UnixTimestampMillis(201),
                CredentialKind::PairTicket,
            ),
            Err(CredentialError::Expired)
        );
        assert_eq!(
            verify_credential(
                &token,
                other.public_key(),
                server_id,
                UnixTimestampMillis(150),
                CredentialKind::PairTicket,
            ),
            Err(CredentialError::InvalidSignature)
        );
        assert_eq!(
            verify_credential(
                &token,
                identity.public_key(),
                server_id,
                UnixTimestampMillis(150),
                CredentialKind::AccessToken,
            ),
            Err(CredentialError::WrongKind)
        );
    }

    #[test]
    fn device_certificate_binds_device_public_key() {
        let identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_key =
            PublicKey("ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into());
        let certificate = CredentialService::new(identity.clone())
            .issue_device_certificate(device_id, device_key.clone(), UnixTimestampMillis(5))
            .unwrap();
        let claims = verify_credential(
            &certificate,
            identity.public_key(),
            identity.server_id(),
            UnixTimestampMillis(9_999_999),
            CredentialKind::DeviceCertificate,
        )
        .unwrap();
        assert_eq!(claims.device_id, Some(device_id));
        assert_eq!(claims.device_public_key, Some(device_key));
        assert_eq!(claims.expires_at_ms, UnixTimestampMillis(u64::MAX));
    }
}
