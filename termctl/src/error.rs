//! termctl 的脱敏错误边界。
//!
//! CLI 只向用户输出稳定 code 和安全 message，不把 token、签名、私钥、终端明文或
//! Rust backtrace 泄漏到 stderr。内部 source 在 MVP 中刻意不透传到 Display。

use std::sync::atomic::{AtomicBool, Ordering};

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
    #[error("websocket URL is invalid")]
    InvalidWsUrl,
    #[error("pairing invite is invalid")]
    InvalidPairingInvite,
    #[error("pairing invite has expired")]
    ExpiredPairingInvite,
    #[error("pairing invite is missing websocket URL")]
    MissingPairingUrl,
    #[error("token-only pairing requires a known daemon")]
    TokenRequiresKnownDaemon,
    #[error("pairing payload server_id does not match daemon")]
    PairingPayloadServerMismatch,
    #[error("route server_id does not match daemon")]
    RouteServerMismatch,
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
    #[error("E2EE frame processing failed")]
    E2eeFailed,
    #[error("auth challenge timed out")]
    AuthChallengeTimeout,
    #[error("daemon returned protocol error")]
    Protocol { code: String, message: String },
    #[error("too many interleaved protocol packets")]
    PendingPacketQueueFull,
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
            Self::ExpiredPairingInvite => "expired_pairing_invite",
            Self::MissingPairingUrl => "missing_pairing_url",
            Self::TokenRequiresKnownDaemon => "token_requires_known_daemon",
            Self::PairingPayloadServerMismatch => "pairing_payload_server_mismatch",
            Self::RouteServerMismatch => "route_server_mismatch",
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
            Self::E2eeFailed => "e2ee_failed",
            Self::AuthChallengeTimeout => "auth_challenge_timeout",
            Self::Protocol { code, .. } => code.as_str(),
            Self::PendingPacketQueueFull => "pending_packet_queue_full",
            Self::ReconnectExhausted => "reconnect_exhausted",
            Self::LocalIo => "local_io_failed",
        }
    }

    pub fn safe_message(&self) -> &str {
        match self {
            Self::InvalidSessionId => "session id must be a UUID",
            Self::InvalidSize => "rows and cols must be positive",
            Self::MissingPairing => "run termctl pair before session commands",
            Self::InvalidWsUrl => "websocket URL must be ws:// or wss:// and end at /ws",
            Self::InvalidPairingInvite => "pairing invite is invalid",
            Self::ExpiredPairingInvite => "pairing invite has expired",
            Self::MissingPairingUrl => "pairing invite does not include a URL; pass --url",
            Self::TokenRequiresKnownDaemon => {
                "token-only pairing requires an already known daemon; use an invite for first pairing"
            }
            Self::PairingPayloadServerMismatch => {
                "pairing payload does not match the connected daemon"
            }
            Self::RouteServerMismatch => "route prelude does not match the connected daemon",
            Self::StateRead => "failed to read local state",
            Self::StateWrite => "failed to write local state",
            Self::PairingStateFinalizeFailed => {
                "pairing succeeded but local state could not be finalized"
            }
            Self::InvalidDeviceKey => "local device signing key is invalid",
            Self::ConnectFailed => "failed to connect websocket",
            Self::ConnectionClosed => "websocket connection closed",
            Self::SendFailed => "failed to send websocket message",
            Self::ReceiveFailed => "failed to receive websocket message",
            Self::InvalidEnvelope => "message envelope is invalid",
            Self::UnexpectedMessage => "unexpected protocol message",
            Self::E2eeFailed => "E2EE frame processing failed",
            Self::AuthChallengeTimeout => "daemon did not return an auth challenge",
            Self::Protocol { message, .. } => message.as_str(),
            Self::PendingPacketQueueFull => {
                "too many terminal packets arrived while waiting for a response"
            }
            Self::ReconnectExhausted => "terminal reconnect attempts exhausted",
            Self::LocalIo => "local stdin/stdout failed",
        }
    }

    pub fn user_message(&self) -> String {
        if JSON_OUTPUT.load(Ordering::Relaxed) {
            return serde_json::json!({
                "error": {
                    "code": self.code(),
                    "message": self.safe_message(),
                }
            })
            .to_string();
        }

        format!("termctl error {}: {}", self.code(), self.safe_message())
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
            | Self::RouteServerMismatch
            | Self::E2eeFailed
            | Self::AuthChallengeTimeout => 5,
            Self::ConnectFailed
            | Self::ConnectionClosed
            | Self::SendFailed
            | Self::ReceiveFailed
            | Self::PendingPacketQueueFull
            | Self::ReconnectExhausted => 6,
            Self::StateRead
            | Self::StateWrite
            | Self::PairingStateFinalizeFailed
            | Self::LocalIo => 7,
        }
    }

    pub fn is_connection_error(&self) -> bool {
        matches!(
            self,
            Self::ConnectFailed
                | Self::ConnectionClosed
                | Self::SendFailed
                | Self::ReceiveFailed
                | Self::PendingPacketQueueFull
        )
    }
}

impl From<termd::net::protocol::ProtocolError> for TermctlError {
    fn from(error: termd::net::protocol::ProtocolError) -> Self {
        match error {
            termd::net::protocol::ProtocolError::InvalidEnvelope => Self::InvalidEnvelope,
            termd::net::protocol::ProtocolError::E2eeFailed => Self::E2eeFailed,
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
}
