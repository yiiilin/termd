//! termctl 的脱敏错误边界。
//!
//! CLI 只向用户输出稳定 code 和安全 message，不把 token、签名、私钥、终端明文或
//! Rust backtrace 泄漏到 stderr。内部 source 在 MVP 中刻意不透传到 Display。

use std::{
    borrow::Cow,
    sync::atomic::{AtomicBool, Ordering},
};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, TermctlError>;

static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);

pub fn set_json_output(enabled: bool) {
    JSON_OUTPUT.store(enabled, Ordering::Relaxed);
}

#[derive(Debug, Error)]
pub enum TermctlError {
    #[error("invalid session id")]
    InvalidSessionId,
    #[error("terminal size must be positive")]
    InvalidSize,
    #[error("device is not paired")]
    MissingPairing,
    #[error("server URL is invalid")]
    InvalidWsUrl,
    #[error("pairing invite is invalid")]
    InvalidPairingInvite,
    #[error("failed to read pairing invite")]
    PairingPayloadRead,
    #[error("pairing invite has expired")]
    ExpiredPairingInvite,
    #[error("pairing invite is missing websocket URL")]
    MissingPairingUrl,
    #[error("token-only pairing requires a known daemon")]
    TokenRequiresKnownDaemon,
    #[error("pairing payload server_id does not match daemon")]
    PairingPayloadServerMismatch,
    #[error("failed to read local state")]
    StateRead,
    #[error("failed to write local state")]
    StateWrite,
    #[error("failed to finalize local pairing state")]
    PairingStateFinalizeFailed,
    #[error("local device key is invalid")]
    InvalidDeviceKey,
    #[error("failed to connect websocket")]
    ConnectFailed,
    #[error("websocket connection closed")]
    ConnectionClosed,
    #[error("failed to send websocket message")]
    SendFailed,
    #[error("failed to receive websocket message")]
    ReceiveFailed,
    #[error("message envelope is invalid")]
    InvalidEnvelope,
    #[error("unexpected protocol message")]
    UnexpectedMessage,
    #[error("auth challenge timed out")]
    AuthChallengeTimeout,
    #[error("daemon returned protocol error")]
    Protocol { code: String, message: String },
    #[error("terminal reconnect attempts exhausted")]
    ReconnectExhausted,
    #[error("local stdin/stdout failed")]
    LocalIo,
}

impl TermctlError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidSessionId => "invalid_session_id",
            Self::InvalidSize => "invalid_size",
            Self::MissingPairing => "missing_pairing",
            Self::InvalidWsUrl => "invalid_ws_url",
            Self::InvalidPairingInvite => "invalid_pairing_invite",
            Self::PairingPayloadRead => "pairing_payload_read_failed",
            Self::ExpiredPairingInvite => "expired_pairing_invite",
            Self::MissingPairingUrl => "missing_pairing_url",
            Self::TokenRequiresKnownDaemon => "token_requires_known_daemon",
            Self::PairingPayloadServerMismatch => "pairing_payload_server_mismatch",
            Self::StateRead => "state_read_failed",
            Self::StateWrite => "state_write_failed",
            Self::PairingStateFinalizeFailed => "pairing_state_finalize_failed",
            Self::InvalidDeviceKey => "invalid_device_key",
            Self::ConnectFailed => "connect_failed",
            Self::ConnectionClosed => "connection_closed",
            Self::SendFailed => "send_failed",
            Self::ReceiveFailed => "receive_failed",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::UnexpectedMessage => "unexpected_message",
            Self::AuthChallengeTimeout => "auth_challenge_timeout",
            Self::Protocol { code, .. } => code.as_str(),
            Self::ReconnectExhausted => "reconnect_exhausted",
            Self::LocalIo => "local_io_failed",
        }
    }

    pub fn safe_message(&self) -> Cow<'_, str> {
        match self {
            Self::InvalidSessionId => Cow::Borrowed("session id must be a UUID"),
            Self::InvalidSize => Cow::Borrowed("rows and cols must be positive"),
            Self::MissingPairing => Cow::Borrowed("run termctl pair before session commands"),
            Self::InvalidWsUrl => Cow::Borrowed(
                "server URL must use http://, https://, ws://, or wss:// and end at /ws",
            ),
            Self::InvalidPairingInvite => Cow::Borrowed("pairing invite is invalid"),
            Self::PairingPayloadRead => Cow::Borrowed("failed to read pairing invite"),
            Self::ExpiredPairingInvite => Cow::Borrowed("pairing invite has expired"),
            Self::MissingPairingUrl => {
                Cow::Borrowed("pairing invite does not include a URL; pass --url")
            }
            Self::TokenRequiresKnownDaemon => Cow::Borrowed(
                "token-only pairing requires an already known daemon; use an invite for first pairing",
            ),
            Self::PairingPayloadServerMismatch => {
                Cow::Borrowed("pairing payload does not match the connected daemon")
            }
            Self::StateRead => Cow::Borrowed("failed to read local state"),
            Self::StateWrite => Cow::Borrowed("failed to write local state"),
            Self::PairingStateFinalizeFailed => {
                Cow::Borrowed("pairing succeeded but local state could not be finalized")
            }
            Self::InvalidDeviceKey => Cow::Borrowed("local device signing key is invalid"),
            Self::ConnectFailed => Cow::Borrowed("failed to connect websocket"),
            Self::ConnectionClosed => Cow::Borrowed("websocket connection closed"),
            Self::SendFailed => Cow::Borrowed("failed to send websocket message"),
            Self::ReceiveFailed => Cow::Borrowed("failed to receive websocket message"),
            Self::InvalidEnvelope => Cow::Borrowed("message envelope is invalid"),
            Self::UnexpectedMessage => Cow::Borrowed("unexpected protocol message"),
            Self::AuthChallengeTimeout => Cow::Borrowed("daemon did not return an auth challenge"),
            Self::Protocol { message, .. } => Cow::Owned(sanitize_remote_message(message)),
            Self::ReconnectExhausted => Cow::Borrowed("terminal reconnect attempts exhausted"),
            Self::LocalIo => Cow::Borrowed("local stdin/stdout failed"),
        }
    }

    pub fn user_message(&self) -> String {
        let message = self.safe_message();
        if JSON_OUTPUT.load(Ordering::Relaxed) {
            return serde_json::json!({
                "error": {
                    "code": self.code(),
                    "message": message.as_ref(),
                }
            })
            .to_string();
        }

        format!("termctl error {}: {}", self.code(), message)
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidSessionId
            | Self::InvalidSize
            | Self::InvalidWsUrl
            | Self::InvalidPairingInvite
            | Self::ExpiredPairingInvite
            | Self::MissingPairingUrl
            | Self::TokenRequiresKnownDaemon => 2,
            Self::MissingPairing | Self::InvalidDeviceKey => 3,
            Self::PairingPayloadServerMismatch => 2,
            Self::Protocol { code, .. } if code == "auth_failed" => 4,
            Self::Protocol { .. }
            | Self::InvalidEnvelope
            | Self::UnexpectedMessage
            | Self::AuthChallengeTimeout => 5,
            Self::ConnectFailed
            | Self::ConnectionClosed
            | Self::SendFailed
            | Self::ReceiveFailed
            | Self::ReconnectExhausted => 6,
            Self::StateRead
            | Self::StateWrite
            | Self::PairingPayloadRead
            | Self::PairingStateFinalizeFailed
            | Self::LocalIo => 7,
        }
    }

    pub fn is_connection_error(&self) -> bool {
        matches!(
            self,
            Self::ConnectFailed | Self::ConnectionClosed | Self::SendFailed | Self::ReceiveFailed
        )
    }
}

fn sanitize_remote_message(message: &str) -> String {
    let normalized = normalize_remote_message_whitespace(message);
    if normalized.is_empty() {
        return "remote protocol error".to_owned();
    }

    let mut parts = normalized
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    for index in 0..parts.len() {
        let current = parts[index].clone();
        if let Some((key, separator, value)) = split_secret_pair(&current)
            && is_sensitive_remote_keyword(key)
        {
            if value.is_empty() {
                if parts
                    .get(index + 1)
                    .is_some_and(|next| !is_bearer_keyword(next))
                {
                    parts[index + 1] = "<redacted>".to_owned();
                }
            } else if is_bearer_keyword(value) {
                if let Some(next) = parts.get_mut(index + 1) {
                    *next = "<redacted>".to_owned();
                }
            } else {
                parts[index] = format!("{key}{separator}<redacted>");
            }
            continue;
        }

        if is_bearer_keyword(&current) {
            if let Some(next) = parts.get_mut(index + 1) {
                *next = "<redacted>".to_owned();
            }
            continue;
        }

        if is_plain_token_keyword(&current)
            && parts
                .get(index + 1)
                .is_some_and(|next| looks_like_secret_value(next))
        {
            parts[index + 1] = "<redacted>".to_owned();
        }
    }

    let sanitized = parts.join(" ");
    if sanitized.is_empty() {
        "remote protocol error".to_owned()
    } else {
        sanitized
    }
}

fn normalize_remote_message_whitespace(message: &str) -> String {
    let mut normalized = String::with_capacity(message.len());
    let mut last_was_space = false;

    for ch in message.chars() {
        let safe = if ch.is_control() { ' ' } else { ch };
        if safe == ' ' {
            if !last_was_space {
                normalized.push(' ');
            }
            last_was_space = true;
        } else {
            normalized.push(safe);
            last_was_space = false;
        }
    }

    normalized.trim().to_owned()
}

fn split_secret_pair(value: &str) -> Option<(&str, char, &str)> {
    for separator in [':', '='] {
        if let Some((key, secret)) = value.split_once(separator) {
            return Some((key, separator, secret));
        }
    }
    None
}

fn is_plain_token_keyword(value: &str) -> bool {
    matches!(
        normalize_remote_keyword(value).as_str(),
        "token" | "authorization" | "bearer"
    )
}

fn is_bearer_keyword(value: &str) -> bool {
    normalize_remote_keyword(value) == "bearer"
}

fn is_sensitive_remote_keyword(value: &str) -> bool {
    let normalized = normalize_remote_keyword(value);
    matches!(
        normalized.as_str(),
        "token"
            | "relay_token"
            | "access_token"
            | "refresh_token"
            | "session_token"
            | "data_token"
            | "authorization"
            | "auth"
            | "bearer"
    )
}

fn normalize_remote_keyword(value: &str) -> String {
    value
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn looks_like_secret_value(value: &str) -> bool {
    let trimmed =
        value.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-');
    trimmed.len() >= 8
        && trimmed
            .chars()
            .any(|ch| ch.is_ascii_digit() || ch == '-' || ch == '_')
}

impl From<termd::net::protocol::ProtocolError> for TermctlError {
    fn from(error: termd::net::protocol::ProtocolError) -> Self {
        match error {
            termd::net::protocol::ProtocolError::InvalidEnvelope => Self::InvalidEnvelope,
            termd::net::protocol::ProtocolError::InvalidState => Self::UnexpectedMessage,
            other => Self::Protocol {
                code: other.code().to_owned(),
                message: other.safe_message().to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_finalize_failure_has_specific_public_error() {
        set_json_output(false);
        let error = TermctlError::PairingStateFinalizeFailed;

        assert_eq!(error.code(), "pairing_state_finalize_failed");
        assert_eq!(
            error.safe_message(),
            "pairing succeeded but local state could not be finalized"
        );
    }

    #[test]
    fn protocol_error_message_redacts_secrets_and_control_characters() {
        set_json_output(false);
        let error = TermctlError::Protocol {
            code: "pairing_failed".to_owned(),
            message: "authorization: Bearer secret-token\trelay_token=abc12345\nbad\x1bmessage"
                .to_owned(),
        };

        let safe = error.safe_message();
        let user = error.user_message();

        assert!(!safe.contains("secret-token"));
        assert!(!safe.contains("abc12345"));
        assert!(!safe.chars().any(char::is_control));
        assert!(safe.contains("Bearer <redacted>"));
        assert!(safe.contains("relay_token=<redacted>"));
        assert!(user.contains("pairing_failed"));
        assert_eq!(error.exit_code(), 5);
    }

    #[test]
    fn auth_failed_protocol_code_keeps_existing_exit_code_mapping() {
        let error = TermctlError::Protocol {
            code: "auth_failed".to_owned(),
            message: "authorization=Bearer secret-token".to_owned(),
        };

        assert_eq!(error.exit_code(), 4);
        assert!(error.safe_message().contains("<redacted>"));
    }
}
