//! termctl 的脱敏错误边界。
//!
//! CLI 只向用户输出稳定 code 和安全 message，不把 token、签名、私钥、终端明文或
//! Rust backtrace 泄漏到 stderr。内部 source 在 MVP 中刻意不透传到 Display。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, TermctlError>;

#[derive(Debug, Error)]
pub enum TermctlError {
    #[error("invalid session id")]
    InvalidSessionId,
    #[error("terminal size must be positive")]
    InvalidSize,
    #[error("device is not paired")]
    MissingPairing,
    #[error("device is not paired with this daemon")]
    NotPaired,
    #[error("failed to read local state")]
    StateRead,
    #[error("failed to write local state")]
    StateWrite,
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
    #[error("local stdin/stdout failed")]
    LocalIo,
}

impl TermctlError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidSessionId => "invalid_session_id",
            Self::InvalidSize => "invalid_size",
            Self::MissingPairing => "missing_pairing",
            Self::NotPaired => "not_paired",
            Self::StateRead => "state_read_failed",
            Self::StateWrite => "state_write_failed",
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
            Self::LocalIo => "local_io_failed",
        }
    }

    pub fn safe_message(&self) -> &str {
        match self {
            Self::InvalidSessionId => "session id must be a UUID",
            Self::InvalidSize => "rows and cols must be positive",
            Self::MissingPairing => "run termctl pair before session commands",
            Self::NotPaired => "this daemon is not in local paired state",
            Self::StateRead => "failed to read local state",
            Self::StateWrite => "failed to write local state",
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
            Self::LocalIo => "local stdin/stdout failed",
        }
    }

    pub fn user_message(&self) -> String {
        format!("termctl error {}: {}", self.code(), self.safe_message())
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidSessionId | Self::InvalidSize => 2,
            Self::MissingPairing | Self::NotPaired | Self::InvalidDeviceKey => 3,
            Self::Protocol { code, .. } if code == "auth_failed" => 4,
            Self::Protocol { .. }
            | Self::InvalidEnvelope
            | Self::UnexpectedMessage
            | Self::E2eeFailed
            | Self::AuthChallengeTimeout => 5,
            Self::ConnectFailed
            | Self::ConnectionClosed
            | Self::SendFailed
            | Self::ReceiveFailed => 6,
            Self::StateRead | Self::StateWrite | Self::LocalIo => 7,
        }
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
