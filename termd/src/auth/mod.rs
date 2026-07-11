//! termd 的设备级认证与信任基础模型。
//!
//! 本模块只维护 daemon identity、device identity 和可信设备清单。它刻意不引入账号体系、
//! 平台级策略或 relay 业务判断；上层只需要先确认设备已配对并可信，再把
//! device id 交给 session/control 模块处理 shared-control operator 规则。
//!
//! 当前实现是 MVP 内存模型：pairing token、challenge-response 与 replay protection 都只在
//! daemon 内核中做生命周期管理；Noise/X25519 或 E2EE 会在后续协议层接入。后续持久化
//! 可以实现 `TrustedDeviceStore`，并复用同一组查询边界来拒绝未配对设备。

use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub use termd_proto::{
    AuthPayload, Challenge, DeviceId, E2eeKeyExchangePayload, HttpE2eeAuthPayload, Nonce,
    PairAcceptPayload, PairRequestPayload, PairingToken, PublicKey, ServerId, SessionToken,
    Signature, UnixTimestampMillis,
};

/// auth 模块统一使用的 Result 类型。
pub type AuthResult<T> = Result<T, AuthError>;

/// pairing token 生命周期使用的 Result 类型。
pub type PairingResult<T> = Result<T, PairingError>;
pub type SessionTokenResult<T> = Result<T, SessionTokenError>;

const ED25519_WIRE_PREFIX: &str = "ed25519-v1:";
const ED25519_PRIVATE_KEY_LEN: usize = 32;
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// 设备信任查询的错误。
///
/// 这里只表达“设备是否可信”和“设备公钥是否与登记信息一致”两类基础拒绝原因。
/// 签名验签、nonce 防重放和挑战响应用更靠后的 `ChallengeResponseService` 固定流程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 设备还没有完成 pairing，因此不能 attach session。
    UntrustedDevice { device_id: DeviceId },
    /// device id 已登记，但本次声明的 public key 与可信记录不一致。
    DeviceKeyMismatch { device_id: DeviceId },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedDevice { .. } => write!(f, "device is not trusted"),
            Self::DeviceKeyMismatch { .. } => write!(f, "device public key does not match"),
        }
    }
}

impl Error for AuthError {}

/// pairing 生命周期的拒绝原因。
///
/// pairing 只是设备级 trust establishment：通过一次性 token 把 device id 与 device public
/// key 登记为可信设备。这里不表达账号、operator 状态或平台级策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    /// token 不存在，可能是拼写错误、已被清理，或来自未授权来源。
    InvalidToken,
    /// token 已超过 `expires_at_ms`，过期瞬间及之后都必须拒绝。
    ExpiredToken {
        expires_at_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    },
    /// token 已经成功用于 pairing，不能重复消费。
    AlreadyUsedToken,
    /// token 正在由另一个 pairing 请求等待外部 admission 确认。
    ReservedToken,
    /// token 已被 daemon 主动撤销。
    RevokedToken,
    /// pairing 阶段收到的设备公钥不符合 Ed25519 wire 编码约定。
    InvalidDevicePublicKey,
    /// ttl 必须为正数，并且不能让过期时间发生整数溢出。
    InvalidTtl { ttl_ms: u64 },
}

/// session token 生命周期的拒绝原因。
///
/// session token 只负责认证后续控制面/终端连接，不替代 E2EE，也不绑定 session 控制权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTokenError {
    InvalidToken,
    ExpiredToken {
        expires_at_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    },
    InvalidTtl {
        ttl_ms: u64,
    },
}

impl fmt::Display for SessionTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken => write!(f, "session token is invalid"),
            Self::ExpiredToken { .. } => write!(f, "session token is expired"),
            Self::InvalidTtl { .. } => write!(f, "session token ttl is invalid"),
        }
    }
}

impl Error for SessionTokenError {}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken => write!(f, "pairing token is invalid"),
            Self::ExpiredToken { .. } => write!(f, "pairing token is expired"),
            Self::AlreadyUsedToken => write!(f, "pairing token was already used"),
            Self::ReservedToken => write!(f, "pairing token is already reserved"),
            Self::RevokedToken => write!(f, "pairing token was revoked"),
            Self::InvalidDevicePublicKey => write!(f, "device public key wire format is invalid"),
            Self::InvalidTtl { .. } => write!(f, "pairing token ttl is invalid"),
        }
    }
}

impl Error for PairingError {}

/// challenge 生命周期使用的 Result 类型。
pub type ChallengeResult<T> = Result<T, ChallengeError>;

/// challenge 生命周期的拒绝原因。
///
/// challenge 只证明后续 auth 请求必须回应 daemon 最近签发的一次性材料；它不代表
/// operator 状态，也不包含账号或控制权语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeError {
    /// challenge 不存在，可能来自其他 daemon、其他连接，或已被清理。
    InvalidChallenge,
    /// challenge 绑定到另一个 device id，不能跨设备复用。
    DeviceMismatch {
        expected_device_id: DeviceId,
        actual_device_id: DeviceId,
    },
    /// challenge 已超过 `expires_at_ms`，过期瞬间及之后都必须拒绝。
    ExpiredChallenge {
        expires_at_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    },
    /// challenge 已被消费；成功或失败的 auth 尝试都不能重复使用同一 challenge。
    UsedChallenge,
    /// challenge 已被 daemon 主动撤销。
    RevokedChallenge,
    /// ttl 必须为正数，并且不能让过期时间发生整数溢出。
    InvalidTtl { ttl_ms: u64 },
}

impl fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChallenge => write!(f, "auth challenge is invalid"),
            Self::DeviceMismatch { .. } => write!(f, "auth challenge belongs to another device"),
            Self::ExpiredChallenge { .. } => write!(f, "auth challenge is expired"),
            Self::UsedChallenge => write!(f, "auth challenge was already used"),
            Self::RevokedChallenge => write!(f, "auth challenge was revoked"),
            Self::InvalidTtl { .. } => write!(f, "auth challenge ttl is invalid"),
        }
    }
}

impl Error for ChallengeError {}

/// replay protection 的拒绝原因。
///
/// replay protection 同时检查客户端声明时间和每个设备自己的 nonce 集合。timestamp 只作为
/// 防重放输入，不被当作服务端当前时间；服务端必须传入可信的 `now_ms`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// 客户端 timestamp 与服务端 `now_ms` 的差值超过允许窗口。
    TimestampOutOfWindow {
        timestamp_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
        allowed_clock_skew_ms: u64,
    },
    /// 同一 device id 下 nonce 已出现过；不同 device id 互不影响。
    ReplayedNonce { device_id: DeviceId },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimestampOutOfWindow { .. } => {
                write!(f, "auth timestamp is outside allowed replay window")
            }
            Self::ReplayedNonce { .. } => write!(f, "auth nonce was already used"),
        }
    }
}

impl Error for ReplayError {}

/// 签名验证边界的拒绝原因。
///
/// 本 item 不引入真实 Ed25519/X25519/Noise 依赖，只固定 daemon 内核如何把可信设备公钥、
/// 规范化待签名字节和 wire signature 交给可替换 verifier。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// verifier 判定签名不匹配。
    InvalidSignature,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSignature => write!(f, "auth signature is invalid"),
        }
    }
}

impl Error for SignatureError {}

/// daemon static keypair 恢复失败的原因。
///
/// 这些错误只针对 daemon 本地状态；pair/auth wire payload 里不能携带 daemon private key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonIdentityError {
    UnsupportedPrivateKeyPrefix,
    InvalidPrivateKeyEncoding,
    InvalidPrivateKeyLength { actual: usize },
    PublicKeyMismatch,
}

impl fmt::Display for DaemonIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPrivateKeyPrefix => write!(f, "daemon private key prefix is invalid"),
            Self::InvalidPrivateKeyEncoding => write!(f, "daemon private key encoding is invalid"),
            Self::InvalidPrivateKeyLength { .. } => {
                write!(f, "daemon private key length is invalid")
            }
            Self::PublicKeyMismatch => write!(f, "daemon private key does not match public key"),
        }
    }
}

impl Error for DaemonIdentityError {}

/// challenge-response auth 的统一拒绝原因。
///
/// auth 只证明“已配对设备持有对应私钥”。它不代表用户身份，也不授予 operator 状态；
/// operator 状态仍由 session/control attach 状态机决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeAuthError {
    UntrustedDevice {
        device_id: DeviceId,
    },
    DeviceKeyMismatch {
        device_id: DeviceId,
    },
    InvalidChallenge,
    ChallengeDeviceMismatch {
        expected_device_id: DeviceId,
        actual_device_id: DeviceId,
    },
    ExpiredChallenge {
        expires_at_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    },
    UsedChallenge,
    RevokedChallenge,
    InvalidTtl {
        ttl_ms: u64,
    },
    TimestampOutOfWindow {
        timestamp_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
        allowed_clock_skew_ms: u64,
    },
    ReplayedNonce {
        device_id: DeviceId,
    },
    InvalidSignature,
}

impl fmt::Display for ChallengeAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedDevice { .. } => write!(f, "device is not trusted"),
            Self::DeviceKeyMismatch { .. } => write!(f, "device public key does not match"),
            Self::InvalidChallenge => write!(f, "auth challenge is invalid"),
            Self::ChallengeDeviceMismatch { .. } => {
                write!(f, "auth challenge belongs to another device")
            }
            Self::ExpiredChallenge { .. } => write!(f, "auth challenge is expired"),
            Self::UsedChallenge => write!(f, "auth challenge was already used"),
            Self::RevokedChallenge => write!(f, "auth challenge was revoked"),
            Self::InvalidTtl { .. } => write!(f, "auth challenge ttl is invalid"),
            Self::TimestampOutOfWindow { .. } => {
                write!(f, "auth timestamp is outside allowed replay window")
            }
            Self::ReplayedNonce { .. } => write!(f, "auth nonce was already used"),
            Self::InvalidSignature => write!(f, "auth signature is invalid"),
        }
    }
}

impl Error for ChallengeAuthError {}

impl From<AuthError> for ChallengeAuthError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::UntrustedDevice { device_id } => Self::UntrustedDevice { device_id },
            AuthError::DeviceKeyMismatch { device_id } => Self::DeviceKeyMismatch { device_id },
        }
    }
}

impl From<ChallengeError> for ChallengeAuthError {
    fn from(error: ChallengeError) -> Self {
        match error {
            ChallengeError::InvalidChallenge => Self::InvalidChallenge,
            ChallengeError::DeviceMismatch {
                expected_device_id,
                actual_device_id,
            } => Self::ChallengeDeviceMismatch {
                expected_device_id,
                actual_device_id,
            },
            ChallengeError::ExpiredChallenge {
                expires_at_ms,
                now_ms,
            } => Self::ExpiredChallenge {
                expires_at_ms,
                now_ms,
            },
            ChallengeError::UsedChallenge => Self::UsedChallenge,
            ChallengeError::RevokedChallenge => Self::RevokedChallenge,
            ChallengeError::InvalidTtl { ttl_ms } => Self::InvalidTtl { ttl_ms },
        }
    }
}

impl From<ReplayError> for ChallengeAuthError {
    fn from(error: ReplayError) -> Self {
        match error {
            ReplayError::TimestampOutOfWindow {
                timestamp_ms,
                now_ms,
                allowed_clock_skew_ms,
            } => Self::TimestampOutOfWindow {
                timestamp_ms,
                now_ms,
                allowed_clock_skew_ms,
            },
            ReplayError::ReplayedNonce { device_id } => Self::ReplayedNonce { device_id },
        }
    }
}

impl From<SignatureError> for ChallengeAuthError {
    fn from(error: SignatureError) -> Self {
        match error {
            SignatureError::InvalidSignature => Self::InvalidSignature,
        }
    }
}

/// pairing token 的内存状态。
///
/// `Active` 才允许消费；成功消费后转为 `Consumed`，撤销后转为 `Revoked`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingTokenState {
    Active,
    Reserved,
    Consumed,
    Revoked,
}

/// daemon 内部保存的 pairing token 记录。
///
/// token 明文是敏感材料，当前只在 auth 内核中保存和比对。Debug 输出会脱敏，避免日志或
/// 测试失败时把可消费 token 泄漏出去。
#[derive(Clone, PartialEq, Eq)]
pub struct PairingTokenRecord {
    token: PairingToken,
    issued_at_ms: UnixTimestampMillis,
    expires_at_ms: UnixTimestampMillis,
    state: PairingTokenState,
    consumed_at_ms: Option<UnixTimestampMillis>,
    reservation_id: Option<String>,
}

impl PairingTokenRecord {
    fn new(
        token: PairingToken,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            token,
            issued_at_ms,
            expires_at_ms,
            state: PairingTokenState::Active,
            consumed_at_ms: None,
            reservation_id: None,
        }
    }

    /// 返回 token 明文引用；调用方只能在 auth/pairing 边界内使用，不能写入日志。
    pub fn token(&self) -> &PairingToken {
        &self.token
    }

    /// 返回 token 签发时间。
    pub fn issued_at_ms(&self) -> UnixTimestampMillis {
        self.issued_at_ms
    }

    /// 返回 token 过期时间；`now_ms >= expires_at_ms` 时必须拒绝。
    pub fn expires_at_ms(&self) -> UnixTimestampMillis {
        self.expires_at_ms
    }

    /// 返回当前 token 状态。
    pub fn state(&self) -> PairingTokenState {
        self.state
    }

    /// 返回 token 成功消费时间；未消费 token 返回 `None`。
    pub fn consumed_at_ms(&self) -> Option<UnixTimestampMillis> {
        self.consumed_at_ms
    }

    /// 判断 token 是否已经到达过期时间。
    pub fn is_expired(&self, now_ms: UnixTimestampMillis) -> bool {
        now_ms >= self.expires_at_ms
    }
}

impl fmt::Debug for PairingTokenRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingTokenRecord")
            .field("token", &"<redacted>")
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("state", &self.state)
            .field("consumed_at_ms", &self.consumed_at_ms)
            .field(
                "reservation_id",
                &self.reservation_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// pairing token 的内存生命周期管理器。
///
/// 该类型只负责签发、查询、一次性消费、撤销和清理过期 token；不实现扫码、CLI、WebSocket、
/// E2EE 或持久化。后续持久化层可以围绕同一状态语义扩展，不应把账号体系混入这里。
#[derive(Default)]
pub struct PairingTokenManager {
    tokens: HashMap<String, PairingTokenRecord>,
}

/// daemon 内部保存的短期 session token 记录。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionTokenRecord {
    token: SessionToken,
    server_id: ServerId,
    device_id: DeviceId,
    issued_at_ms: UnixTimestampMillis,
    expires_at_ms: UnixTimestampMillis,
}

/// daemon 内部保存的短期 session scope token 记录。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionScopeRecord {
    token: SessionToken,
    server_id: ServerId,
    device_id: DeviceId,
    session_id: termd_proto::SessionId,
    issued_at_ms: UnixTimestampMillis,
    expires_at_ms: UnixTimestampMillis,
}

impl SessionScopeRecord {
    fn new(
        token: SessionToken,
        server_id: ServerId,
        device_id: DeviceId,
        session_id: termd_proto::SessionId,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            token,
            server_id,
            device_id,
            session_id,
            issued_at_ms,
            expires_at_ms,
        }
    }

    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn session_id(&self) -> termd_proto::SessionId {
        self.session_id
    }

    pub fn issued_at_ms(&self) -> UnixTimestampMillis {
        self.issued_at_ms
    }

    pub fn expires_at_ms(&self) -> UnixTimestampMillis {
        self.expires_at_ms
    }

    fn is_expired(&self, now_ms: UnixTimestampMillis) -> bool {
        now_ms >= self.expires_at_ms
    }
}

impl fmt::Debug for SessionScopeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionScopeRecord")
            .field("token", &"<redacted>")
            .field("server_id", &self.server_id)
            .field("device_id", &self.device_id)
            .field("session_id", &self.session_id)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl SessionTokenRecord {
    fn new(
        token: SessionToken,
        server_id: ServerId,
        device_id: DeviceId,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            token,
            server_id,
            device_id,
            issued_at_ms,
            expires_at_ms,
        }
    }

    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn issued_at_ms(&self) -> UnixTimestampMillis {
        self.issued_at_ms
    }

    pub fn expires_at_ms(&self) -> UnixTimestampMillis {
        self.expires_at_ms
    }

    fn is_expired(&self, now_ms: UnixTimestampMillis) -> bool {
        now_ms >= self.expires_at_ms
    }
}

impl fmt::Debug for SessionTokenRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTokenRecord")
            .field("token", &"<redacted>")
            .field("server_id", &self.server_id)
            .field("device_id", &self.device_id)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// session token 的内存生命周期管理器。
#[derive(Default, Clone)]
pub struct SessionTokenManager {
    tokens: HashMap<String, SessionTokenRecord>,
}

/// session scope token 的内存生命周期管理器。
#[derive(Default, Clone)]
pub struct SessionScopeManager {
    tokens: HashMap<String, SessionScopeRecord>,
}

impl SessionTokenManager {
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    pub fn issue(
        &mut self,
        server_id: ServerId,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> SessionTokenResult<SessionTokenRecord> {
        if ttl_ms == 0 {
            return Err(SessionTokenError::InvalidTtl { ttl_ms });
        }
        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(ttl_ms)
                .ok_or(SessionTokenError::InvalidTtl { ttl_ms })?,
        );
        self.prune_expired(now_ms);
        loop {
            let token = generate_session_token();
            let key = session_token_key(&token).to_owned();
            if self.tokens.contains_key(&key) {
                continue;
            }
            let record =
                SessionTokenRecord::new(token, server_id, device_id, now_ms, expires_at_ms);
            self.tokens.insert(key, record.clone());
            return Ok(record);
        }
    }

    pub fn record(&self, token: &SessionToken) -> Option<&SessionTokenRecord> {
        self.tokens.get(session_token_key(token))
    }

    pub fn verify(
        &mut self,
        token: &SessionToken,
        now_ms: UnixTimestampMillis,
    ) -> SessionTokenResult<SessionTokenRecord> {
        let token_key = session_token_key(token).to_owned();
        self.prune_expired_except(now_ms, Some(token_key.as_str()));
        let Some(record) = self.tokens.get(&token_key) else {
            return Err(SessionTokenError::InvalidToken);
        };
        if record.is_expired(now_ms) {
            let expires_at_ms = record.expires_at_ms();
            self.tokens.remove(&token_key);
            return Err(SessionTokenError::ExpiredToken {
                expires_at_ms,
                now_ms,
            });
        }
        Ok(record.clone())
    }

    pub fn prune_expired(&mut self, now_ms: UnixTimestampMillis) -> usize {
        self.prune_expired_except(now_ms, None)
    }

    fn prune_expired_except(
        &mut self,
        now_ms: UnixTimestampMillis,
        keep_key: Option<&str>,
    ) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|key, record| {
            if keep_key.is_some_and(|keep| keep == key) {
                return true;
            }
            !record.is_expired(now_ms)
        });
        before - self.tokens.len()
    }
}

impl SessionScopeManager {
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    pub fn issue(
        &mut self,
        server_id: ServerId,
        device_id: DeviceId,
        session_id: termd_proto::SessionId,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> SessionTokenResult<SessionScopeRecord> {
        if ttl_ms == 0 {
            return Err(SessionTokenError::InvalidTtl { ttl_ms });
        }
        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(ttl_ms)
                .ok_or(SessionTokenError::InvalidTtl { ttl_ms })?,
        );
        self.prune_expired(now_ms);
        loop {
            let token = generate_session_token();
            let key = session_token_key(&token).to_owned();
            if self.tokens.contains_key(&key) {
                continue;
            }
            let record = SessionScopeRecord::new(
                token,
                server_id,
                device_id,
                session_id,
                now_ms,
                expires_at_ms,
            );
            self.tokens.insert(key, record.clone());
            return Ok(record);
        }
    }

    pub fn verify(
        &mut self,
        token: &SessionToken,
        now_ms: UnixTimestampMillis,
    ) -> SessionTokenResult<SessionScopeRecord> {
        let token_key = session_token_key(token).to_owned();
        self.prune_expired_except(now_ms, Some(token_key.as_str()));
        let Some(record) = self.tokens.get(&token_key) else {
            return Err(SessionTokenError::InvalidToken);
        };
        if record.is_expired(now_ms) {
            let expires_at_ms = record.expires_at_ms();
            self.tokens.remove(&token_key);
            return Err(SessionTokenError::ExpiredToken {
                expires_at_ms,
                now_ms,
            });
        }
        Ok(record.clone())
    }

    pub fn prune_expired(&mut self, now_ms: UnixTimestampMillis) -> usize {
        self.prune_expired_except(now_ms, None)
    }

    fn prune_expired_except(
        &mut self,
        now_ms: UnixTimestampMillis,
        keep_key: Option<&str>,
    ) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|key, record| {
            if keep_key.is_some_and(|keep| keep == key) {
                return true;
            }
            !record.is_expired(now_ms)
        });
        before - self.tokens.len()
    }
}

impl fmt::Debug for SessionScopeManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionScopeManager")
            .field("len", &self.tokens.len())
            .finish()
    }
}

impl fmt::Debug for SessionTokenManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTokenManager")
            .field("len", &self.tokens.len())
            .finish()
    }
}

impl PairingTokenManager {
    /// 创建空的 pairing token 管理器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 签发一次性 pairing token，并计算 `expires_at_ms`。
    ///
    /// `ttl_ms` 必须大于 0。过期时间使用 checked add，避免极端输入造成时间戳回绕。
    pub fn issue(
        &mut self,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> PairingResult<PairingTokenRecord> {
        if ttl_ms == 0 {
            return Err(PairingError::InvalidTtl { ttl_ms });
        }

        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(ttl_ms)
                .ok_or(PairingError::InvalidTtl { ttl_ms })?,
        );
        self.prune_expired(now_ms);

        loop {
            let token = generate_pairing_token();
            let key = pairing_token_key(&token).to_owned();

            if self.tokens.contains_key(&key) {
                continue;
            }

            let record = PairingTokenRecord::new(token, now_ms, expires_at_ms);
            self.tokens.insert(key, record.clone());
            return Ok(record);
        }
    }

    /// 查询 token 记录；返回值可能是 active、consumed 或 revoked。
    pub fn record(&self, token: &PairingToken) -> Option<&PairingTokenRecord> {
        self.tokens.get(pairing_token_key(token))
    }

    /// 消费 active token。
    ///
    /// 成功后 token 立即进入 `Consumed`，同一 token 后续请求必须被拒绝，避免重复 pairing。
    pub fn consume(
        &mut self,
        token: &PairingToken,
        now_ms: UnixTimestampMillis,
    ) -> PairingResult<PairingTokenRecord> {
        let token_key = pairing_token_key(token).to_owned();
        self.prune_expired_except(now_ms, Some(token_key.as_str()));

        let record = self
            .tokens
            .get_mut(&token_key)
            .ok_or(PairingError::InvalidToken)?;

        match record.state {
            PairingTokenState::Active => {}
            PairingTokenState::Reserved => return Err(PairingError::ReservedToken),
            PairingTokenState::Consumed => return Err(PairingError::AlreadyUsedToken),
            PairingTokenState::Revoked => return Err(PairingError::RevokedToken),
        }

        if record.is_expired(now_ms) {
            let error = PairingError::ExpiredToken {
                expires_at_ms: record.expires_at_ms,
                now_ms,
            };
            // 过期 token 的第一次消费仍返回精确错误，随后立即删除避免内存长期保留。
            self.tokens.remove(&token_key);
            return Err(error);
        }

        record.state = PairingTokenState::Consumed;
        record.consumed_at_ms = Some(now_ms);
        Ok(record.clone())
    }

    /// 主动撤销 active token。
    ///
    /// 已消费 token 保持 `Consumed`，这样重复消费仍能得到明确的 already-used 拒绝原因。
    pub fn revoke(&mut self, token: &PairingToken) -> bool {
        let Some(record) = self.tokens.get_mut(pairing_token_key(token)) else {
            return false;
        };

        if matches!(
            record.state,
            PairingTokenState::Consumed | PairingTokenState::Revoked
        ) {
            return false;
        }

        record.state = PairingTokenState::Revoked;
        record.reservation_id = None;
        true
    }

    /// 清理已经过期的 token 记录，返回被删除数量。
    pub fn prune_expired(&mut self, now_ms: UnixTimestampMillis) -> usize {
        self.prune_expired_except(now_ms, None)
    }

    fn prune_expired_except(
        &mut self,
        now_ms: UnixTimestampMillis,
        protected_key: Option<&str>,
    ) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|key, record| {
            protected_key.is_some_and(|protected| protected == key.as_str())
                || !record.is_expired(now_ms)
        });
        before - self.tokens.len()
    }

    /// 返回当前保留的 token 记录数量。
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// 判断 token 管理器是否为空。
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

impl fmt::Debug for PairingTokenManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingTokenManager")
            .field("len", &self.tokens.len())
            .finish()
    }
}

/// pair_request / pair_accept 的 daemon 内核服务。
///
/// 该服务只完成“token -> trusted device”的设备级信任建立，不给设备分配 operator 状态，
/// 也不创建账号。operator 状态仍由 session/control 状态机在 attach 时决定。
#[derive(Debug, Default)]
pub struct PairingService {
    token_manager: PairingTokenManager,
}

/// 已通过 token 校验、但尚未写入本地 trust store 的 pairing 结果。
///
/// trusted relay 模式需要先确认 relay 已登记新设备，再把设备落成本地可信设备，
/// 避免用户看到 PairAccept 后却无法经 relay 访问。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPairing {
    accepted: PairAcceptPayload,
    device_identity: DeviceIdentity,
    token_key: String,
    reservation_id: String,
}

impl PendingPairing {
    pub fn accepted(&self) -> &PairAcceptPayload {
        &self.accepted
    }

    pub fn device_identity(&self) -> &DeviceIdentity {
        &self.device_identity
    }

    pub fn into_accepted(self) -> PairAcceptPayload {
        self.accepted
    }
}

impl PairingService {
    /// 使用指定 token manager 创建 pairing 服务，便于后续替换为持久化实现。
    pub fn new(token_manager: PairingTokenManager) -> Self {
        Self { token_manager }
    }

    /// 返回 token manager 只读引用，供 daemon 查询 token 状态。
    pub fn token_manager(&self) -> &PairingTokenManager {
        &self.token_manager
    }

    /// 返回 token manager 可变引用，供 daemon 执行撤销或清理。
    pub fn token_manager_mut(&mut self) -> &mut PairingTokenManager {
        &mut self.token_manager
    }

    /// 通过服务签发 token，保持上层只依赖 pairing 服务即可完成常规流程。
    pub fn issue_token(
        &mut self,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> PairingResult<PairingTokenRecord> {
        self.token_manager.issue(now_ms, ttl_ms)
    }

    /// 消费 pair_request token 并生成待确认 pairing，不立即写 trust store。
    ///
    /// 上层可在调用外部 relay 注册成功后，再调用 `trust_pending_pairing` 落成本地信任。
    pub fn consume_pair_request(
        &mut self,
        request: PairRequestPayload,
        now_ms: UnixTimestampMillis,
        daemon_identity: &DaemonIdentity,
    ) -> PairingResult<PendingPairing> {
        let pending = self.reserve_pair_request(request, now_ms, daemon_identity)?;
        self.commit_pairing_reservation(&pending, now_ms)?;
        Ok(pending)
    }

    pub fn reserve_pair_request(
        &mut self,
        request: PairRequestPayload,
        now_ms: UnixTimestampMillis,
        daemon_identity: &DaemonIdentity,
    ) -> PairingResult<PendingPairing> {
        let PairRequestPayload {
            device_id,
            device_public_key,
            token,
            nonce: _,
            timestamp_ms: _,
        } = request;

        // 中文注释：pairing 阶段就收紧设备公钥 wire 形状，避免 direct 模式先返回成功，
        // 后续 auth 再因为坏 key 失败，或把坏 key 写进本地 trust store。
        validate_device_public_key_wire(&device_public_key)?;
        let token_key = pairing_token_key(&token).to_owned();
        self.token_manager
            .prune_expired_except(now_ms, Some(token_key.as_str()));
        let token_record = self
            .token_manager
            .tokens
            .get_mut(&token_key)
            .ok_or(PairingError::InvalidToken)?;
        match token_record.state {
            PairingTokenState::Active => {}
            PairingTokenState::Reserved => return Err(PairingError::ReservedToken),
            PairingTokenState::Consumed => return Err(PairingError::AlreadyUsedToken),
            PairingTokenState::Revoked => return Err(PairingError::RevokedToken),
        }
        if token_record.is_expired(now_ms) {
            let error = PairingError::ExpiredToken {
                expires_at_ms: token_record.expires_at_ms,
                now_ms,
            };
            self.token_manager.tokens.remove(&token_key);
            return Err(error);
        }
        let reservation_id = generate_pairing_token().0;
        token_record.state = PairingTokenState::Reserved;
        token_record.reservation_id = Some(reservation_id.clone());
        let device_identity = DeviceIdentity::new(device_id, device_public_key);
        let public_identity = daemon_identity.public_identity();

        Ok(PendingPairing {
            accepted: PairAcceptPayload {
                server_id: public_identity.server_id,
                daemon_public_key: public_identity.public_key,
                device_id,
                expires_at_ms: token_record.expires_at_ms,
            },
            device_identity,
            token_key,
            reservation_id,
        })
    }

    pub fn release_pairing_reservation(&mut self, pending: &PendingPairing) -> bool {
        let Some(record) = self.token_manager.tokens.get_mut(&pending.token_key) else {
            return false;
        };
        if record.state != PairingTokenState::Reserved
            || record.reservation_id.as_deref() != Some(pending.reservation_id.as_str())
        {
            return false;
        }
        record.state = PairingTokenState::Active;
        record.reservation_id = None;
        true
    }

    pub fn commit_pairing_reservation(
        &mut self,
        pending: &PendingPairing,
        now_ms: UnixTimestampMillis,
    ) -> PairingResult<PairingTokenRecord> {
        let record = self
            .token_manager
            .tokens
            .get_mut(&pending.token_key)
            .ok_or(PairingError::InvalidToken)?;
        match record.state {
            PairingTokenState::Reserved
                if record.reservation_id.as_deref() == Some(pending.reservation_id.as_str()) => {}
            PairingTokenState::Reserved => return Err(PairingError::ReservedToken),
            PairingTokenState::Active => return Err(PairingError::InvalidToken),
            PairingTokenState::Consumed => return Err(PairingError::AlreadyUsedToken),
            PairingTokenState::Revoked => return Err(PairingError::RevokedToken),
        }
        if record.is_expired(now_ms) {
            record.state = PairingTokenState::Active;
            record.reservation_id = None;
            return Err(PairingError::ExpiredToken {
                expires_at_ms: record.expires_at_ms,
                now_ms,
            });
        }
        record.state = PairingTokenState::Consumed;
        record.reservation_id = None;
        record.consumed_at_ms = Some(now_ms);
        Ok(record.clone())
    }

    /// 将已完成外部确认的 pairing 写入本地 trust store。
    pub fn trust_pending_pairing<S>(
        pending: &PendingPairing,
        now_ms: UnixTimestampMillis,
        trusted_store: &mut S,
    ) where
        S: TrustedDeviceStore,
    {
        trusted_store.trust_device(pending.device_identity.clone(), now_ms, None);
    }

    /// 处理 pair_request 并返回 pair_accept payload。
    ///
    /// 本函数故意不校验 nonce/timestamp 的防重放语义；这些字段属于后续 challenge-response
    /// 与 replay protection。这里唯一的安全门槛是 token 未过期、未撤销、未消费。
    pub fn accept_pair_request<S>(
        &mut self,
        request: PairRequestPayload,
        now_ms: UnixTimestampMillis,
        daemon_identity: &DaemonIdentity,
        trusted_store: &mut S,
    ) -> PairingResult<PairAcceptPayload>
    where
        S: TrustedDeviceStore,
    {
        let pending = self.consume_pair_request(request, now_ms, daemon_identity)?;
        Self::trust_pending_pairing(&pending, now_ms, trusted_store);
        Ok(pending.into_accepted())
    }
}

/// auth challenge 的内存状态。
///
/// `Active` 才允许被一次 auth 尝试消费；消费后无论签名是否通过，challenge 都不再可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthChallengeState {
    Active,
    Consumed,
    Revoked,
}

/// daemon 内部保存的 auth challenge 记录。
///
/// challenge 是短期认证材料，Debug 输出会脱敏。记录绑定 device id，避免一个设备拿到的
/// challenge 被另一个设备跨连接复用。
#[derive(Clone, PartialEq, Eq)]
pub struct AuthChallengeRecord {
    challenge: Challenge,
    device_id: DeviceId,
    issued_at_ms: UnixTimestampMillis,
    expires_at_ms: UnixTimestampMillis,
    state: AuthChallengeState,
    consumed_at_ms: Option<UnixTimestampMillis>,
}

impl AuthChallengeRecord {
    fn new(
        challenge: Challenge,
        device_id: DeviceId,
        issued_at_ms: UnixTimestampMillis,
        expires_at_ms: UnixTimestampMillis,
    ) -> Self {
        Self {
            challenge,
            device_id,
            issued_at_ms,
            expires_at_ms,
            state: AuthChallengeState::Active,
            consumed_at_ms: None,
        }
    }

    /// 返回 challenge 明文引用；调用方只能在 auth 边界内使用，不能写入日志。
    pub fn challenge(&self) -> &Challenge {
        &self.challenge
    }

    /// 返回 challenge 绑定的 device id。
    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// 返回 challenge 签发时间。
    pub fn issued_at_ms(&self) -> UnixTimestampMillis {
        self.issued_at_ms
    }

    /// 返回 challenge 过期时间；`now_ms >= expires_at_ms` 时必须拒绝。
    pub fn expires_at_ms(&self) -> UnixTimestampMillis {
        self.expires_at_ms
    }

    /// 返回当前 challenge 状态。
    pub fn state(&self) -> AuthChallengeState {
        self.state
    }

    /// 返回 challenge 被消费的服务端时间；未消费返回 `None`。
    pub fn consumed_at_ms(&self) -> Option<UnixTimestampMillis> {
        self.consumed_at_ms
    }

    /// 判断 challenge 是否已经到达过期时间。
    pub fn is_expired(&self, now_ms: UnixTimestampMillis) -> bool {
        now_ms >= self.expires_at_ms
    }
}

impl fmt::Debug for AuthChallengeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChallengeRecord")
            .field("challenge", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("state", &self.state)
            .field("consumed_at_ms", &self.consumed_at_ms)
            .finish()
    }
}

/// auth challenge 的内存生命周期管理器。
///
/// 管理器只签发、查询、一次性消费、撤销和清理 challenge；不做 WebSocket 握手、不做
/// E2EE，也不决定 operator 状态。auth 成功后才由上层继续 attach session。
#[derive(Default)]
pub struct AuthChallengeManager {
    challenges: HashMap<String, AuthChallengeRecord>,
}

impl AuthChallengeManager {
    /// 单设备最多保留的未消费 challenge 数量，防止同一设备反复握手撑大内存状态。
    pub const MAX_OUTSTANDING_PER_DEVICE: usize = 8;

    /// 创建空的 challenge 管理器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 为指定设备签发一次性 challenge，并计算 `expires_at_ms`。
    pub fn issue(
        &mut self,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> ChallengeResult<AuthChallengeRecord> {
        if ttl_ms == 0 {
            return Err(ChallengeError::InvalidTtl { ttl_ms });
        }

        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(ttl_ms)
                .ok_or(ChallengeError::InvalidTtl { ttl_ms })?,
        );
        self.prune_unusable_except(now_ms, None);
        self.enforce_device_outstanding_cap(device_id);

        loop {
            let challenge = generate_auth_challenge();
            let key = challenge_key(&challenge).to_owned();

            if self.challenges.contains_key(&key) {
                continue;
            }

            let record = AuthChallengeRecord::new(challenge, device_id, now_ms, expires_at_ms);
            self.challenges.insert(key, record.clone());
            return Ok(record);
        }
    }

    /// 查询 challenge 记录；返回值可能是 active、consumed 或 revoked。
    pub fn record(&self, challenge: &Challenge) -> Option<&AuthChallengeRecord> {
        self.challenges.get(challenge_key(challenge))
    }

    /// 消费 active 且未过期的 challenge。
    ///
    /// 成功返回时，内部记录已经转为 `Consumed`。调用方随后即使因为 nonce、timestamp 或
    /// signature 失败，也不能重新使用同一个 challenge。
    pub fn consume(
        &mut self,
        device_id: &DeviceId,
        challenge: &Challenge,
        now_ms: UnixTimestampMillis,
    ) -> ChallengeResult<AuthChallengeRecord> {
        let challenge_key = challenge_key(challenge).to_owned();
        self.prune_unusable_except(now_ms, Some(challenge_key.as_str()));

        let record = self
            .challenges
            .get_mut(&challenge_key)
            .ok_or(ChallengeError::InvalidChallenge)?;

        match record.state {
            AuthChallengeState::Active => {}
            AuthChallengeState::Consumed => return Err(ChallengeError::UsedChallenge),
            AuthChallengeState::Revoked => return Err(ChallengeError::RevokedChallenge),
        }

        if record.is_expired(now_ms) {
            let error = ChallengeError::ExpiredChallenge {
                expires_at_ms: record.expires_at_ms,
                now_ms,
            };
            // 过期 challenge 的第一次消费保留精确错误，同时删除记录避免重复占用。
            self.challenges.remove(&challenge_key);
            return Err(error);
        }

        if record.device_id != *device_id {
            // challenge 一旦参与认证尝试就烧掉；即使 device id 不匹配，也不能让同一
            // challenge 回到可用状态形成重放入口。
            record.state = AuthChallengeState::Consumed;
            record.consumed_at_ms = Some(now_ms);
            return Err(ChallengeError::DeviceMismatch {
                expected_device_id: record.device_id,
                actual_device_id: *device_id,
            });
        }

        record.state = AuthChallengeState::Consumed;
        record.consumed_at_ms = Some(now_ms);
        Ok(record.clone())
    }

    /// 主动撤销 active challenge。
    pub fn revoke(&mut self, device_id: &DeviceId, challenge: &Challenge) -> bool {
        let Some(record) = self.challenges.get_mut(challenge_key(challenge)) else {
            return false;
        };

        if record.device_id != *device_id || record.state != AuthChallengeState::Active {
            return false;
        }

        record.state = AuthChallengeState::Revoked;
        true
    }

    /// 清理已经过期的 challenge 记录，返回被删除数量。
    pub fn prune_expired(&mut self, now_ms: UnixTimestampMillis) -> usize {
        let before = self.challenges.len();
        self.challenges
            .retain(|_, record| !record.is_expired(now_ms));
        before - self.challenges.len()
    }

    fn prune_unusable_except(
        &mut self,
        now_ms: UnixTimestampMillis,
        protected_key: Option<&str>,
    ) -> usize {
        let before = self.challenges.len();
        self.challenges.retain(|key, record| {
            if protected_key.is_some_and(|protected| protected == key.as_str()) {
                return true;
            }

            !record.is_expired(now_ms) && record.state != AuthChallengeState::Consumed
        });
        before - self.challenges.len()
    }

    fn enforce_device_outstanding_cap(&mut self, device_id: DeviceId) -> usize {
        let mut removed = 0;

        while self
            .challenges
            .values()
            .filter(|record| {
                record.device_id == device_id && record.state == AuthChallengeState::Active
            })
            .count()
            >= Self::MAX_OUTSTANDING_PER_DEVICE
        {
            let Some(oldest_key) = self
                .challenges
                .iter()
                .filter(|(_, record)| {
                    record.device_id == device_id && record.state == AuthChallengeState::Active
                })
                .min_by(|(left_key, left), (right_key, right)| {
                    left.issued_at_ms
                        .cmp(&right.issued_at_ms)
                        .then_with(|| left_key.cmp(right_key))
                })
                .map(|(key, _)| key.clone())
            else {
                break;
            };

            // cap 只淘汰同一 device 最旧的 active challenge；其他设备互不影响。
            self.challenges.remove(&oldest_key);
            removed += 1;
        }

        removed
    }

    /// 返回当前保留的 challenge 记录数量。
    pub fn len(&self) -> usize {
        self.challenges.len()
    }

    /// 判断 challenge 管理器是否为空。
    pub fn is_empty(&self) -> bool {
        self.challenges.is_empty()
    }
}

impl fmt::Debug for AuthChallengeManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChallengeManager")
            .field("len", &self.challenges.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplayNonceRecord {
    recorded_at_ms: UnixTimestampMillis,
}

/// 按 device id 隔离的 nonce replay 保护器。
///
/// `AuthPayload.timestamp_ms` 是客户端声明时间，只用于判断是否落在服务端 `now_ms` 的允许
/// 窗口内；重复 nonce 记录使用服务端观察时间，避免信任客户端时间清理状态。
#[derive(Debug)]
pub struct ReplayProtector {
    allowed_clock_skew_ms: u64,
    nonces_by_device: HashMap<DeviceId, HashMap<String, ReplayNonceRecord>>,
}

impl ReplayProtector {
    /// 默认允许 5 分钟时钟偏移，后续可由 daemon 配置层接入。
    pub const DEFAULT_ALLOWED_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;

    /// 使用指定允许窗口创建 replay protector。
    pub fn new(allowed_clock_skew_ms: u64) -> Self {
        Self {
            allowed_clock_skew_ms,
            nonces_by_device: HashMap::new(),
        }
    }

    /// 检查 timestamp 与 nonce，但不记录 nonce。
    ///
    /// HTTP E2EE 与 WebSocket auth 这类需要先验证签名的路径先做 replay 预检，
    /// 签名通过后再记录 nonce，避免坏签名请求消耗合法 nonce。
    pub fn check(
        &mut self,
        device_id: &DeviceId,
        nonce: &Nonce,
        timestamp_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), ReplayError> {
        self.prune(now_ms);

        if timestamp_ms.0.abs_diff(now_ms.0) > self.allowed_clock_skew_ms {
            return Err(ReplayError::TimestampOutOfWindow {
                timestamp_ms,
                now_ms,
                allowed_clock_skew_ms: self.allowed_clock_skew_ms,
            });
        }

        let nonce_key = nonce_key(nonce).to_owned();

        if self
            .nonces_by_device
            .get(device_id)
            .is_some_and(|device_nonces| device_nonces.contains_key(&nonce_key))
        {
            return Err(ReplayError::ReplayedNonce {
                device_id: *device_id,
            });
        }

        Ok(())
    }

    /// 在调用方已经完成 `check` 和签名验证后记录 nonce。
    pub fn record_checked(
        &mut self,
        device_id: &DeviceId,
        nonce: &Nonce,
        now_ms: UnixTimestampMillis,
    ) {
        let device_nonces = self.nonces_by_device.entry(*device_id).or_default();
        let nonce_key = nonce_key(nonce).to_owned();
        device_nonces.insert(
            nonce_key,
            ReplayNonceRecord {
                recorded_at_ms: now_ms,
            },
        );
    }

    /// 检查 timestamp 与 nonce，并在通过时记录 nonce。
    ///
    /// 同一 device id 下 nonce 一次通过后，在保留窗口内不能再次使用；不同 device id 的
    /// nonce 空间互相隔离。
    pub fn check_and_record(
        &mut self,
        device_id: &DeviceId,
        nonce: &Nonce,
        timestamp_ms: UnixTimestampMillis,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), ReplayError> {
        self.check(device_id, nonce, timestamp_ms, now_ms)?;
        self.record_checked(device_id, nonce, now_ms);
        Ok(())
    }

    /// 按服务端观察时间清理超出窗口的 nonce 记录。
    pub fn prune(&mut self, now_ms: UnixTimestampMillis) -> usize {
        let mut removed = 0;
        let window_ms = self.allowed_clock_skew_ms;

        self.nonces_by_device.retain(|_, device_nonces| {
            let before = device_nonces.len();
            device_nonces
                .retain(|_, record| now_ms.0.saturating_sub(record.recorded_at_ms.0) <= window_ms);
            removed += before - device_nonces.len();
            !device_nonces.is_empty()
        });

        removed
    }

    /// 返回当前记录的 nonce 数量，主要供测试和运行期指标使用。
    pub fn len(&self) -> usize {
        self.nonces_by_device
            .values()
            .map(HashMap::len)
            .sum::<usize>()
    }

    /// 判断 protector 是否没有保留任何 nonce。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ReplayProtector {
    fn default() -> Self {
        Self::new(Self::DEFAULT_ALLOWED_CLOCK_SKEW_MS)
    }
}

/// 可替换的设备签名验证边界。
///
/// verifier 必须用可信设备记录中的 public key 验证规范化待签名字节和 wire signature。这里
/// 不实现具体算法，避免在本 item 中引入加密依赖；后续 Ed25519/Noise 集成只需实现此 trait。
pub trait SignatureVerifier {
    fn verify(
        &self,
        device_public_key: &PublicKey,
        signing_input: &[u8],
        signature: &Signature,
    ) -> Result<(), SignatureError>;
}

/// 规范化 auth 待签名输入。
///
/// 字段顺序固定为 daemon/server 上下文、device id、challenge、nonce、timestamp。签名不包含
/// 自身，避免自引用；长度前缀避免字段值中出现分隔符时造成歧义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSigningInput {
    server_id: ServerId,
    daemon_public_key: PublicKey,
    device_id: DeviceId,
    challenge: Challenge,
    nonce: Nonce,
    timestamp_ms: UnixTimestampMillis,
    e2ee_transcript_sha256: Option<String>,
}

impl AuthSigningInput {
    /// 从 wire auth payload 和 daemon public identity 构造规范化签名输入。
    pub fn from_payload(payload: &AuthPayload, daemon_identity: &DaemonPublicIdentity) -> Self {
        Self {
            server_id: daemon_identity.server_id,
            daemon_public_key: daemon_identity.public_key.clone(),
            device_id: payload.device_id,
            challenge: payload.challenge.clone(),
            nonce: payload.nonce.clone(),
            timestamp_ms: payload.timestamp_ms,
            e2ee_transcript_sha256: None,
        }
    }

    /// 从 auth payload 构造绑定当前 E2EE transcript 的签名输入。
    ///
    /// 0.2.0 的客户端必须使用这个路径：设备签名不只证明 challenge，还证明自己看到的是
    /// 当前 daemon 身份签过的 X25519 握手材料，避免 relay 把 challenge 跨连接转发。
    pub fn from_payload_with_e2ee_transcript(
        payload: &AuthPayload,
        daemon_identity: &DaemonPublicIdentity,
        transcript: Option<&E2eeAuthTranscript>,
    ) -> Self {
        let mut input = Self::from_payload(payload, daemon_identity);
        input.e2ee_transcript_sha256 = transcript.map(E2eeAuthTranscript::digest_wire);
        input
    }

    /// 输出稳定字节序列，供具体签名算法验签。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"termd-auth-v1\n");
        append_canonical_field(&mut bytes, "server_id", &self.server_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "daemon_public_key",
            self.daemon_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "device_id", &self.device_id.0.to_string());
        append_canonical_field(&mut bytes, "challenge", self.challenge.0.as_str());
        append_canonical_field(&mut bytes, "nonce", self.nonce.0.as_str());
        append_canonical_field(&mut bytes, "timestamp_ms", &self.timestamp_ms.0.to_string());
        if let Some(transcript) = &self.e2ee_transcript_sha256 {
            append_canonical_field(&mut bytes, "e2ee_transcript_sha256", transcript);
        }
        bytes
    }
}

/// daemon 对自己发出的 E2EE server hello 做身份签名的规范化输入。
///
/// 这个签名把长期 Ed25519 trust anchor 绑定到短期 X25519 公钥。客户端在建立 E2EE session
/// 前先验证它，relay 因而不能替换 X25519 公钥后继续冒充 daemon。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonE2eeSigningInput {
    server_id: ServerId,
    daemon_public_key: PublicKey,
    device_id: DeviceId,
    e2ee_public_key: PublicKey,
    nonce: Nonce,
    timestamp_ms: UnixTimestampMillis,
    packet_version: u16,
    binary_version: u16,
}

impl DaemonE2eeSigningInput {
    pub fn from_payload(
        payload: &E2eeKeyExchangePayload,
        daemon_identity: &DaemonPublicIdentity,
    ) -> Self {
        Self {
            server_id: daemon_identity.server_id,
            daemon_public_key: daemon_identity.public_key.clone(),
            device_id: payload.device_id,
            e2ee_public_key: payload.public_key.clone(),
            nonce: payload.nonce.clone(),
            timestamp_ms: payload.timestamp_ms,
            packet_version: payload
                .packet_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
            binary_version: payload
                .binary_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"termd-daemon-e2ee-key-exchange-v1\n");
        append_canonical_field(&mut bytes, "server_id", &self.server_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "daemon_public_key",
            self.daemon_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "device_id", &self.device_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "e2ee_public_key",
            self.e2ee_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "nonce", self.nonce.0.as_str());
        append_canonical_field(&mut bytes, "timestamp_ms", &self.timestamp_ms.0.to_string());
        append_canonical_field(
            &mut bytes,
            "packet_version",
            &self.packet_version.to_string(),
        );
        append_canonical_field(
            &mut bytes,
            "binary_version",
            &self.binary_version.to_string(),
        );
        bytes
    }
}

/// HTTP E2EE 短期通道的规范化待签名字节。
///
/// 和 WebSocket auth 不同，这里没有服务端 challenge；安全边界来自已配对 device key、
/// nonce/timestamp replay protection，以及 method/path 绑定。这样每次 HTTP transfer 都能
/// 独立认证，不依赖某条 WebSocket 连接的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpE2eeSigningInput {
    server_id: ServerId,
    daemon_public_key: PublicKey,
    device_id: DeviceId,
    e2ee_public_key: PublicKey,
    nonce: Nonce,
    timestamp_ms: UnixTimestampMillis,
    method: String,
    path: String,
}

impl HttpE2eeSigningInput {
    pub fn from_payload(
        payload: &HttpE2eeAuthPayload,
        daemon_identity: &DaemonPublicIdentity,
    ) -> Self {
        Self {
            server_id: daemon_identity.server_id,
            daemon_public_key: daemon_identity.public_key.clone(),
            device_id: payload.device_id,
            e2ee_public_key: payload.e2ee_public_key.clone(),
            nonce: payload.nonce.clone(),
            timestamp_ms: payload.timestamp_ms,
            method: payload.method.clone(),
            path: payload.path.clone(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"termd-http-e2ee-v1\n");
        append_canonical_field(&mut bytes, "server_id", &self.server_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "daemon_public_key",
            self.daemon_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "device_id", &self.device_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "e2ee_public_key",
            self.e2ee_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "nonce", self.nonce.0.as_str());
        append_canonical_field(&mut bytes, "timestamp_ms", &self.timestamp_ms.0.to_string());
        append_canonical_field(&mut bytes, "method", self.method.as_str());
        append_canonical_field(&mut bytes, "path", self.path.as_str());
        bytes
    }
}

/// auth 签名需要绑定的 E2EE transcript 摘要。
///
/// 摘要覆盖 daemon 身份、daemon/server E2EE hello、device E2EE hello 和 packet 版本。
/// wire payload 不额外携带这个摘要，客户端与 daemon 各自从握手材料计算同一个值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eeAuthTranscript {
    server_id: ServerId,
    daemon_public_key: PublicKey,
    daemon_e2ee_public_key: PublicKey,
    daemon_nonce: Nonce,
    daemon_timestamp_ms: UnixTimestampMillis,
    daemon_packet_version: u16,
    daemon_binary_version: u16,
    daemon_signature: Option<Signature>,
    device_id: DeviceId,
    device_e2ee_public_key: PublicKey,
    device_nonce: Nonce,
    device_timestamp_ms: UnixTimestampMillis,
    device_packet_version: u16,
    device_binary_version: u16,
}

impl E2eeAuthTranscript {
    pub fn from_key_exchanges(
        daemon_exchange: &E2eeKeyExchangePayload,
        device_exchange: &E2eeKeyExchangePayload,
        daemon_identity: &DaemonPublicIdentity,
    ) -> Self {
        Self {
            server_id: daemon_identity.server_id,
            daemon_public_key: daemon_identity.public_key.clone(),
            daemon_e2ee_public_key: daemon_exchange.public_key.clone(),
            daemon_nonce: daemon_exchange.nonce.clone(),
            daemon_timestamp_ms: daemon_exchange.timestamp_ms,
            daemon_packet_version: daemon_exchange
                .packet_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
            daemon_binary_version: daemon_exchange
                .binary_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
            daemon_signature: daemon_exchange.signature.clone(),
            device_id: device_exchange.device_id,
            device_e2ee_public_key: device_exchange.public_key.clone(),
            device_nonce: device_exchange.nonce.clone(),
            device_timestamp_ms: device_exchange.timestamp_ms,
            device_packet_version: device_exchange
                .packet_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
            device_binary_version: device_exchange
                .binary_version
                .unwrap_or(termd_proto::ProtocolVersion(0))
                .0,
        }
    }

    pub fn digest_wire(&self) -> String {
        let digest = Sha256::digest(self.to_bytes());
        format!(
            "sha256-v1:{}",
            general_purpose::STANDARD.encode(digest.as_slice())
        )
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"termd-e2ee-auth-transcript-v1\n");
        append_canonical_field(&mut bytes, "server_id", &self.server_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "daemon_public_key",
            self.daemon_public_key.0.as_str(),
        );
        append_canonical_field(
            &mut bytes,
            "daemon_e2ee_public_key",
            self.daemon_e2ee_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "daemon_nonce", self.daemon_nonce.0.as_str());
        append_canonical_field(
            &mut bytes,
            "daemon_timestamp_ms",
            &self.daemon_timestamp_ms.0.to_string(),
        );
        append_canonical_field(
            &mut bytes,
            "daemon_packet_version",
            &self.daemon_packet_version.to_string(),
        );
        append_canonical_field(
            &mut bytes,
            "daemon_binary_version",
            &self.daemon_binary_version.to_string(),
        );
        if let Some(signature) = &self.daemon_signature {
            append_canonical_field(&mut bytes, "daemon_signature", signature.0.as_str());
        }
        append_canonical_field(&mut bytes, "device_id", &self.device_id.0.to_string());
        append_canonical_field(
            &mut bytes,
            "device_e2ee_public_key",
            self.device_e2ee_public_key.0.as_str(),
        );
        append_canonical_field(&mut bytes, "device_nonce", self.device_nonce.0.as_str());
        append_canonical_field(
            &mut bytes,
            "device_timestamp_ms",
            &self.device_timestamp_ms.0.to_string(),
        );
        append_canonical_field(
            &mut bytes,
            "device_packet_version",
            &self.device_packet_version.to_string(),
        );
        append_canonical_field(
            &mut bytes,
            "device_binary_version",
            &self.device_binary_version.to_string(),
        );
        bytes
    }
}

/// challenge-response auth 成功后的最小结果。
///
/// 这里仍只返回设备级事实：哪个可信设备在何时通过认证。它不包含 session role，也不代表
/// operator 控制状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice {
    pub device_id: DeviceId,
    pub device_public_key: PublicKey,
    pub authenticated_at_ms: UnixTimestampMillis,
}

/// challenge-response auth 的 daemon 内核服务。
///
/// 服务流程固定为：先确认 device id 已配对，再一次性消费 challenge，预检 timestamp/nonce，
/// 再把可信 public key 和规范化输入交给 verifier；签名通过后才记录 nonce 并刷新
/// `last_seen`。
#[derive(Debug)]
pub struct ChallengeResponseService {
    daemon_identity: DaemonPublicIdentity,
    challenge_manager: AuthChallengeManager,
    replay_protector: ReplayProtector,
}

impl ChallengeResponseService {
    /// 创建 challenge-response auth 服务。
    pub fn new(
        daemon_identity: DaemonPublicIdentity,
        challenge_manager: AuthChallengeManager,
        replay_protector: ReplayProtector,
    ) -> Self {
        Self {
            daemon_identity,
            challenge_manager,
            replay_protector,
        }
    }

    /// 返回 daemon public identity；调用方可用它构造签名输入或 auth hello。
    pub fn daemon_public_identity(&self) -> &DaemonPublicIdentity {
        &self.daemon_identity
    }

    /// 返回 challenge manager 只读引用。
    pub fn challenge_manager(&self) -> &AuthChallengeManager {
        &self.challenge_manager
    }

    /// 返回 challenge manager 可变引用，供 daemon 执行撤销或清理。
    pub fn challenge_manager_mut(&mut self) -> &mut AuthChallengeManager {
        &mut self.challenge_manager
    }

    /// 返回 replay protector 只读引用。
    pub fn replay_protector(&self) -> &ReplayProtector {
        &self.replay_protector
    }

    /// 返回 replay protector 可变引用，供 daemon 执行清理或指标采集。
    pub fn replay_protector_mut(&mut self) -> &mut ReplayProtector {
        &mut self.replay_protector
    }

    /// 为设备签发 challenge。调用方应在网络层把该值发给对应设备。
    pub fn issue_challenge(
        &mut self,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
        ttl_ms: u64,
    ) -> ChallengeResult<AuthChallengeRecord> {
        self.challenge_manager.issue(device_id, now_ms, ttl_ms)
    }

    /// 校验 auth payload 并在成功时刷新可信设备的 last_seen。
    pub fn authenticate<S, V>(
        &mut self,
        payload: AuthPayload,
        now_ms: UnixTimestampMillis,
        trusted_store: &mut S,
        verifier: &V,
    ) -> Result<AuthenticatedDevice, ChallengeAuthError>
    where
        S: TrustedDeviceStore,
        V: SignatureVerifier,
    {
        self.authenticate_with_transcript(payload, now_ms, trusted_store, verifier, None)
    }

    /// 校验 auth payload，并可选绑定当前 E2EE transcript。
    ///
    /// 这条路径是 0.2.0 新握手所用：relay 即使拿到 challenge，也不能把签名搬到另一条
    /// E2EE 连接上，因为签名输入里会包含本次握手 transcript 的摘要。
    pub fn authenticate_with_transcript<S, V>(
        &mut self,
        payload: AuthPayload,
        now_ms: UnixTimestampMillis,
        trusted_store: &mut S,
        verifier: &V,
        transcript: Option<&E2eeAuthTranscript>,
    ) -> Result<AuthenticatedDevice, ChallengeAuthError>
    where
        S: TrustedDeviceStore,
        V: SignatureVerifier,
    {
        let device_public_key = trusted_store
            .require_trusted(&payload.device_id)
            .map_err(ChallengeAuthError::from)?
            .public_key()
            .clone();

        let challenge_record_key = challenge_key(&payload.challenge).to_owned();
        self.challenge_manager
            .prune_unusable_except(now_ms, Some(challenge_record_key.as_str()));
        self.challenge_manager
            .consume(&payload.device_id, &payload.challenge, now_ms)
            .map_err(ChallengeAuthError::from)?;

        self.replay_protector
            .check(
                &payload.device_id,
                &payload.nonce,
                payload.timestamp_ms,
                now_ms,
            )
            .map_err(ChallengeAuthError::from)?;

        let signing_input = AuthSigningInput::from_payload_with_e2ee_transcript(
            &payload,
            &self.daemon_identity,
            transcript,
        )
        .to_bytes();
        verifier
            .verify(&device_public_key, &signing_input, &payload.signature)
            .map_err(ChallengeAuthError::from)?;
        self.replay_protector
            .record_checked(&payload.device_id, &payload.nonce, now_ms);

        trusted_store
            .mark_seen(&payload.device_id, now_ms)
            .map_err(ChallengeAuthError::from)?;

        Ok(AuthenticatedDevice {
            device_id: payload.device_id,
            device_public_key,
            authenticated_at_ms: now_ms,
        })
    }
}

/// daemon 对外可公开的身份。
///
/// 该结构只包含 server id 和 daemon public key，可以安全返回给 client 或写入协议 payload。
/// server private key 永远不应出现在 client 侧，也不从 `DaemonIdentity` 暴露。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPublicIdentity {
    pub server_id: ServerId,
    pub public_key: PublicKey,
}

/// daemon 本地身份。
///
/// `DaemonIdentity` 是 daemon static keypair 这条信任根的宿主。私钥材料只保存在 daemon
/// 本地持久状态里，不能进入 client、relay、pair payload 或日志。
#[derive(Clone, PartialEq, Eq)]
pub struct DaemonIdentity {
    server_id: ServerId,
    public_key: PublicKey,
    private_key: DaemonPrivateKey,
}

impl DaemonIdentity {
    /// 生成一个新的 daemon identity。
    ///
    /// `server_id` 仍然只是 relay 路由 UUID；真正的 daemon 身份由 Ed25519 static keypair
    /// 表达。
    pub fn generate() -> Self {
        Self::generate_for_server_id(ServerId::new())
    }

    /// 保留已有 `server_id`，重新生成真实 daemon static keypair。
    ///
    /// 旧 fake public key 没有可恢复的私钥，只能在升级时走这个路径；这样不会改变 relay 路由
    /// 使用的 UUID。
    pub fn generate_for_server_id(server_id: ServerId) -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::from_signing_key(server_id, signing_key)
    }

    /// 从 daemon 本地持久化的 static keypair 恢复身份。
    ///
    /// 恢复时会用 private key 重新派生 public key，并与 SQLite 中保存的 public key 比对，避免
    /// silent key rotation 破坏已配对设备的 trust anchor。
    pub fn from_persisted_identity(
        server_id: ServerId,
        public_key: PublicKey,
        private_key: String,
    ) -> Result<Self, DaemonIdentityError> {
        let signing_key = decode_daemon_signing_key(&private_key)?;
        let derived_public_key =
            PublicKey(daemon_wire_prefixed(signing_key.verifying_key().as_bytes()));
        if derived_public_key != public_key {
            return Err(DaemonIdentityError::PublicKeyMismatch);
        }

        Ok(Self {
            server_id,
            public_key,
            private_key: DaemonPrivateKey {
                material: private_key,
            },
        })
    }

    fn from_signing_key(server_id: ServerId, signing_key: SigningKey) -> Self {
        let public_key = PublicKey(daemon_wire_prefixed(signing_key.verifying_key().as_bytes()));
        let private_key = DaemonPrivateKey {
            material: daemon_wire_prefixed(&signing_key.to_bytes()),
        };

        Self {
            server_id,
            public_key,
            private_key,
        }
    }

    /// 返回 daemon 的稳定公开 id。
    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    /// 返回 daemon public key；调用方不得从这里推导或保存 server private key。
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// 返回仅供 daemon 本地持久化使用的 private key wire 文本。
    pub(crate) fn private_key_for_persistence(&self) -> String {
        self.private_key.material.clone()
    }

    /// 使用 daemon static Ed25519 私钥签名规范化输入，并返回 wire signature。
    ///
    /// 该方法只用于 daemon 本地给公开握手材料背书；私钥仍不会进入任何协议 payload。
    pub fn sign_to_wire(&self, signing_input: &[u8]) -> Result<Signature, DaemonIdentityError> {
        let signing_key = decode_daemon_signing_key(&self.private_key.material)?;
        let signature = signing_key.sign(signing_input);
        Ok(Signature(daemon_wire_prefixed(&signature.to_bytes())))
    }

    /// 构造只含公开字段的 daemon identity，供 hello/pair_accept 等协议层使用。
    pub fn public_identity(&self) -> DaemonPublicIdentity {
        DaemonPublicIdentity {
            server_id: self.server_id,
            public_key: self.public_key.clone(),
        }
    }
}

impl Default for DaemonIdentity {
    fn default() -> Self {
        Self::generate()
    }
}

impl fmt::Debug for DaemonIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonIdentity")
            .field("server_id", &self.server_id)
            .field("public_key", &self.public_key)
            .field("private_key", &self.private_key)
            .finish()
    }
}

/// daemon 私钥材料的本地类型。
///
/// 类型不导出，Debug 也只输出脱敏文本，防止日志或测试失败输出中泄漏真实私钥。
#[derive(Clone, PartialEq, Eq)]
struct DaemonPrivateKey {
    material: String,
}

impl fmt::Debug for DaemonPrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

/// 设备身份。
///
/// paired device = trusted identity。这里的 public key 只是登记材料；真实“证明设备持有私钥”
/// 的 challenge-response 不在本模块实现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    device_id: DeviceId,
    public_key: PublicKey,
}

impl DeviceIdentity {
    /// 使用已知 device id 和 public key 构造设备身份。
    pub fn new(device_id: DeviceId, public_key: PublicKey) -> Self {
        Self {
            device_id,
            public_key,
        }
    }

    /// 为新设备生成 device id，并绑定调用方提供的 public key。
    pub fn generate(public_key: PublicKey) -> Self {
        Self::new(DeviceId::new(), public_key)
    }

    /// 返回设备 id。device id 是设备身份的索引，不等同于账号或控制权角色。
    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// 返回登记的设备 public key。
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

/// 已建立信任的设备记录。
///
/// 记录只保存设备级元数据：设备身份、建立信任时间、最近一次可信访问时间和可读标签。
/// 不在这里保存 session 角色，也不保存 operator 控制状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedDevice {
    identity: DeviceIdentity,
    trusted_at_ms: UnixTimestampMillis,
    last_seen_at_ms: Option<UnixTimestampMillis>,
    label: Option<String>,
}

impl TrustedDevice {
    fn new(
        identity: DeviceIdentity,
        trusted_at_ms: UnixTimestampMillis,
        label: Option<String>,
    ) -> Self {
        Self {
            identity,
            trusted_at_ms,
            last_seen_at_ms: None,
            label: normalize_label(label),
        }
    }

    /// 从本地状态文件恢复可信设备记录。
    ///
    /// 这只恢复设备级 trust fact；operator 状态仍由运行时 attach 状态机重新计算。
    pub fn restore(
        identity: DeviceIdentity,
        trusted_at_ms: UnixTimestampMillis,
        last_seen_at_ms: Option<UnixTimestampMillis>,
        label: Option<String>,
    ) -> Self {
        Self {
            identity,
            trusted_at_ms,
            last_seen_at_ms,
            label: normalize_label(label),
        }
    }

    /// 返回可信设备身份。
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// 返回设备 id，便于调用方按 id 进行 attach 前检查。
    pub fn device_id(&self) -> DeviceId {
        self.identity.device_id()
    }

    /// 返回可信记录中的设备 public key。
    pub fn public_key(&self) -> &PublicKey {
        self.identity.public_key()
    }

    /// 返回本次信任建立或刷新时间。
    pub fn trusted_at_ms(&self) -> UnixTimestampMillis {
        self.trusted_at_ms
    }

    /// 返回最近一次已确认可信设备出现的时间。
    pub fn last_seen_at_ms(&self) -> Option<UnixTimestampMillis> {
        self.last_seen_at_ms
    }

    /// 返回用于本地展示的设备标签；标签不参与控制权判断。
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    fn refresh(
        &mut self,
        identity: DeviceIdentity,
        trusted_at_ms: UnixTimestampMillis,
        label: Option<String>,
    ) {
        self.identity = identity;
        self.trusted_at_ms = trusted_at_ms;
        self.label = normalize_label(label);
    }

    fn mark_seen(&mut self, seen_at_ms: UnixTimestampMillis) {
        self.last_seen_at_ms = Some(seen_at_ms);
    }
}

/// 可信设备存储边界。
///
/// item-2 只提供内存实现；item-7 可以把该 trait 接到文件或数据库持久化。调用方应在 attach
/// 前使用 `require_trusted_identity` 或 `is_trusted_identity` 拒绝未配对设备。
pub trait TrustedDeviceStore {
    /// 新增或刷新可信设备。
    ///
    /// 同一个 device id 重复 trust 时只更新该记录的 identity/label/trusted_at 元数据，
    /// 不创建重复实体；last_seen 会保留，因为它表示最近一次认证成功出现的事实。
    fn trust_device(
        &mut self,
        identity: DeviceIdentity,
        trusted_at_ms: UnixTimestampMillis,
        label: Option<String>,
    ) -> &TrustedDevice;

    /// 按 device id 查询可信记录。
    fn trusted_device(&self, device_id: &DeviceId) -> Option<&TrustedDevice>;

    /// 撤销设备信任。撤销后该 device id 必须重新 pairing 才能 attach。
    fn revoke_device(&mut self, device_id: &DeviceId) -> Option<TrustedDevice>;

    /// 更新最近出现时间；未配对设备不能被标记为 seen。
    fn mark_seen(
        &mut self,
        device_id: &DeviceId,
        seen_at_ms: UnixTimestampMillis,
    ) -> AuthResult<()>;

    /// 返回可信设备数量。
    fn len(&self) -> usize;

    /// 按 device id 判断是否可信，用于快速拒绝未配对设备。
    fn is_trusted(&self, device_id: &DeviceId) -> bool {
        self.trusted_device(device_id).is_some()
    }

    /// 判断 device id 和 public key 是否同时匹配可信记录。
    ///
    /// 这不是 challenge-response 验签，只是防止同一个 device id 搭配不同 public key 通过查询。
    fn is_trusted_identity(&self, identity: &DeviceIdentity) -> bool {
        self.require_trusted_identity(identity).is_ok()
    }

    /// 要求 device id 已配对，否则返回可向协议层映射的拒绝原因。
    fn require_trusted(&self, device_id: &DeviceId) -> AuthResult<&TrustedDevice> {
        self.trusted_device(device_id)
            .ok_or(AuthError::UntrustedDevice {
                device_id: *device_id,
            })
    }

    /// 要求 device id 与 public key 都匹配可信记录。
    fn require_trusted_identity(&self, identity: &DeviceIdentity) -> AuthResult<&TrustedDevice> {
        let trusted = self.require_trusted(&identity.device_id())?;

        if trusted.public_key() != identity.public_key() {
            return Err(AuthError::DeviceKeyMismatch {
                device_id: identity.device_id(),
            });
        }

        Ok(trusted)
    }

    /// 判断可信设备清单是否为空。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 单进程内存版可信设备存储。
///
/// 该类型不做持久化，也不跨进程共享。它的职责是先把 trust store 的行为固定下来，
/// 后续文件存储实现必须保持同样的不变量。
#[derive(Debug, Default)]
pub struct InMemoryTrustedDeviceStore {
    devices: HashMap<DeviceId, TrustedDevice>,
}

impl InMemoryTrustedDeviceStore {
    /// 创建空的可信设备清单。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从持久化快照恢复内存 trust store。
    pub fn from_trusted_devices(devices: impl IntoIterator<Item = TrustedDevice>) -> Self {
        let devices = devices
            .into_iter()
            .map(|device| (device.device_id(), device))
            .collect();
        Self { devices }
    }

    /// 返回当前可信设备记录，供状态快照持久化。
    pub fn trusted_devices(&self) -> impl Iterator<Item = &TrustedDevice> {
        self.devices.values()
    }

    /// 使用当前系统时间 trust 设备，便于 daemon 运行时代码调用。
    pub fn trust_device_now(
        &mut self,
        identity: DeviceIdentity,
        label: Option<String>,
    ) -> &TrustedDevice {
        self.trust_device(identity, current_unix_timestamp_millis(), label)
    }

    /// 使用当前系统时间更新最近出现时间。
    pub fn mark_seen_now(&mut self, device_id: &DeviceId) -> AuthResult<()> {
        self.mark_seen(device_id, current_unix_timestamp_millis())
    }
}

impl TrustedDeviceStore for InMemoryTrustedDeviceStore {
    fn trust_device(
        &mut self,
        identity: DeviceIdentity,
        trusted_at_ms: UnixTimestampMillis,
        label: Option<String>,
    ) -> &TrustedDevice {
        let device_id = identity.device_id();

        let device = self
            .devices
            .entry(device_id)
            .and_modify(|trusted| trusted.refresh(identity.clone(), trusted_at_ms, label.clone()))
            .or_insert_with(|| TrustedDevice::new(identity, trusted_at_ms, label));

        &*device
    }

    fn trusted_device(&self, device_id: &DeviceId) -> Option<&TrustedDevice> {
        self.devices.get(device_id)
    }

    fn revoke_device(&mut self, device_id: &DeviceId) -> Option<TrustedDevice> {
        self.devices.remove(device_id)
    }

    fn mark_seen(
        &mut self,
        device_id: &DeviceId,
        seen_at_ms: UnixTimestampMillis,
    ) -> AuthResult<()> {
        let trusted = self
            .devices
            .get_mut(device_id)
            .ok_or(AuthError::UntrustedDevice {
                device_id: *device_id,
            })?;

        trusted.mark_seen(seen_at_ms);
        Ok(())
    }

    fn len(&self) -> usize {
        self.devices.len()
    }
}

/// 返回当前 Unix 毫秒时间戳。
///
/// 如果系统时钟异常早于 Unix epoch，MVP 中饱和为 0，避免 auth 元数据写入时 panic。
pub fn current_unix_timestamp_millis() -> UnixTimestampMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);

    UnixTimestampMillis(millis.min(u128::from(u64::MAX)) as u64)
}

fn normalize_label(label: Option<String>) -> Option<String> {
    label
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn generate_pairing_token() -> PairingToken {
    let token_id = ServerId::new();

    // token 明文只作为短期 pairing 凭据使用；这里借用 UUID 生成不透明字符串，
    // 不把其中任何内容解释为账号、控制权或 session 信息。
    PairingToken(format!("termd-pair-{}", token_id.0))
}

fn pairing_token_key(token: &PairingToken) -> &str {
    token.0.as_str()
}

fn generate_session_token() -> SessionToken {
    let token_id = ServerId::new();

    // session token 只表示“这个设备刚刚在这个 daemon 上完成认证”，
    // 不编码 session id、权限角色或任何可推断的明文业务信息。
    SessionToken(format!("termd-session-{}", token_id.0))
}

fn session_token_key(token: &SessionToken) -> &str {
    token.0.as_str()
}

fn generate_auth_challenge() -> Challenge {
    let challenge_id = ServerId::new();

    // challenge 是短期、一次性的 auth 材料；它只证明客户端回应了 daemon 最近签发的随机值，
    // 不携带账号、控制权或 session role。
    Challenge(format!("termd-auth-challenge-{}", challenge_id.0))
}

fn challenge_key(challenge: &Challenge) -> &str {
    challenge.0.as_str()
}

fn nonce_key(nonce: &Nonce) -> &str {
    nonce.0.as_str()
}

fn daemon_wire_prefixed(bytes: &[u8]) -> String {
    format!(
        "{ED25519_WIRE_PREFIX}{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

fn validate_device_public_key_wire(public_key: &PublicKey) -> PairingResult<()> {
    let encoded = public_key
        .0
        .strip_prefix(ED25519_WIRE_PREFIX)
        .ok_or(PairingError::InvalidDevicePublicKey)?;
    let bytes = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| PairingError::InvalidDevicePublicKey)?;
    let public_key_bytes: [u8; ED25519_PUBLIC_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| PairingError::InvalidDevicePublicKey)?;
    VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|_| PairingError::InvalidDevicePublicKey)?;
    Ok(())
}

fn decode_daemon_signing_key(private_key: &str) -> Result<SigningKey, DaemonIdentityError> {
    let encoded = private_key
        .strip_prefix(ED25519_WIRE_PREFIX)
        .ok_or(DaemonIdentityError::UnsupportedPrivateKeyPrefix)?;
    let bytes = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| DaemonIdentityError::InvalidPrivateKeyEncoding)?;
    let actual = bytes.len();
    let bytes: [u8; ED25519_PRIVATE_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| DaemonIdentityError::InvalidPrivateKeyLength { actual })?;

    Ok(SigningKey::from_bytes(&bytes))
}

fn append_canonical_field(bytes: &mut Vec<u8>, name: &str, value: &str) {
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(b":");
    bytes.extend_from_slice(value.len().to_string().as_bytes());
    bytes.extend_from_slice(b":");
    bytes.extend_from_slice(value.as_bytes());
    bytes.extend_from_slice(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};

    fn public_key(value: &str) -> PublicKey {
        PublicKey(value.to_owned())
    }

    fn timestamp(value: u64) -> UnixTimestampMillis {
        UnixTimestampMillis(value)
    }

    fn nonce(value: &str) -> termd_proto::Nonce {
        termd_proto::Nonce(value.to_owned())
    }

    fn signature(value: &str) -> termd_proto::Signature {
        termd_proto::Signature(value.to_owned())
    }

    fn ed25519_public_key_wire(seed: u8) -> PublicKey {
        let signing_key = SigningKey::from_bytes(&[seed; ED25519_PRIVATE_KEY_LEN]);
        PublicKey(daemon_wire_prefixed(signing_key.verifying_key().as_bytes()))
    }

    fn decode_test_wire_bytes(value: &str, expected_len: usize) -> Vec<u8> {
        let encoded = value.strip_prefix(ED25519_WIRE_PREFIX).unwrap();
        let bytes = general_purpose::STANDARD.decode(encoded).unwrap();
        assert_eq!(bytes.len(), expected_len);
        bytes
    }

    #[test]
    fn daemon_identity_generates_real_static_ed25519_keypair() {
        let identity = DaemonIdentity::generate();
        let public_identity = identity.public_identity();
        let private_key = identity.private_key_for_persistence();
        let restored = DaemonIdentity::from_persisted_identity(
            identity.server_id(),
            identity.public_key().clone(),
            private_key.clone(),
        )
        .unwrap();

        assert_eq!(public_identity.server_id, identity.server_id());
        assert_eq!(&public_identity.public_key, identity.public_key());
        assert!(identity.public_key().0.starts_with(ED25519_WIRE_PREFIX));
        assert!(private_key.starts_with(ED25519_WIRE_PREFIX));
        assert!(
            !identity
                .public_key()
                .0
                .contains(&identity.server_id().0.to_string())
        );
        assert_eq!(restored.public_key(), identity.public_key());
        assert_eq!(
            restored.private_key_for_persistence(),
            identity.private_key_for_persistence()
        );
    }

    #[test]
    fn daemon_identity_rejects_mismatched_persisted_keypair_and_redacts_debug() {
        let identity = DaemonIdentity::generate();
        let other = DaemonIdentity::generate();
        let error = DaemonIdentity::from_persisted_identity(
            identity.server_id(),
            identity.public_key().clone(),
            other.private_key_for_persistence(),
        )
        .unwrap_err();
        let debug = format!("{identity:?}");

        assert_eq!(error, DaemonIdentityError::PublicKeyMismatch);
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&identity.private_key_for_persistence()));
    }

    #[test]
    fn daemon_identity_can_generate_real_keypair_for_existing_server_id() {
        let server_id = ServerId::new();
        let identity = DaemonIdentity::generate_for_server_id(server_id);

        assert_eq!(identity.server_id(), server_id);
        assert!(identity.public_key().0.starts_with(ED25519_WIRE_PREFIX));
    }

    #[test]
    fn new_device_is_untrusted_by_default() {
        let store = InMemoryTrustedDeviceStore::new();
        let device = DeviceIdentity::generate(public_key("device-a-public"));

        assert!(!store.is_trusted(&device.device_id()));
        assert!(!store.is_trusted_identity(&device));
        assert_eq!(
            store.require_trusted(&device.device_id()).unwrap_err(),
            AuthError::UntrustedDevice {
                device_id: device.device_id()
            }
        );
    }

    #[test]
    fn trusted_device_can_be_verified_by_device_id_and_public_key() {
        let mut store = InMemoryTrustedDeviceStore::new();
        let device = DeviceIdentity::generate(public_key("device-a-public"));
        let device_id = device.device_id();

        store.trust_device(device.clone(), timestamp(1000), Some("laptop".to_owned()));

        assert!(store.is_trusted(&device_id));
        assert!(store.is_trusted_identity(&device));
        assert_eq!(
            store.trusted_device(&device_id).unwrap().label(),
            Some("laptop")
        );
        assert_eq!(
            store.trusted_device(&device_id).unwrap().trusted_at_ms(),
            timestamp(1000)
        );
    }

    #[test]
    fn revoked_device_is_no_longer_trusted() {
        let mut store = InMemoryTrustedDeviceStore::new();
        let device = DeviceIdentity::generate(public_key("device-a-public"));
        let device_id = device.device_id();

        store.trust_device(device, timestamp(1000), None);
        let removed = store.revoke_device(&device_id);

        assert!(removed.is_some());
        assert!(!store.is_trusted(&device_id));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn repeated_trust_updates_metadata_without_creating_duplicate_record() {
        let mut store = InMemoryTrustedDeviceStore::new();
        let device_id = DeviceId::new();
        let original = DeviceIdentity::new(device_id, public_key("device-old-public"));
        let updated = DeviceIdentity::new(device_id, public_key("device-new-public"));

        store.trust_device(
            original.clone(),
            timestamp(1000),
            Some("old label".to_owned()),
        );
        store.mark_seen(&device_id, timestamp(1500)).unwrap();
        store.trust_device(
            updated.clone(),
            timestamp(2000),
            Some("  new label  ".to_owned()),
        );

        let trusted = store.trusted_device(&device_id).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(trusted.public_key(), updated.public_key());
        assert_eq!(trusted.trusted_at_ms(), timestamp(2000));
        assert_eq!(trusted.last_seen_at_ms(), Some(timestamp(1500)));
        assert_eq!(trusted.label(), Some("new label"));
        assert!(!store.is_trusted_identity(&original));
        assert!(store.is_trusted_identity(&updated));
    }

    #[test]
    fn mark_seen_rejects_untrusted_device() {
        let mut store = InMemoryTrustedDeviceStore::new();
        let device_id = DeviceId::new();

        assert_eq!(
            store.mark_seen(&device_id, timestamp(1000)).unwrap_err(),
            AuthError::UntrustedDevice { device_id }
        );
    }

    #[test]
    fn pairing_token_issue_records_expiration_time() {
        let mut manager = PairingTokenManager::new();

        let record = manager.issue(timestamp(1000), 60_000).unwrap();

        assert_eq!(record.issued_at_ms(), timestamp(1000));
        assert_eq!(record.expires_at_ms(), timestamp(61_000));
        assert!(!record.token().0.is_empty());
        assert!(manager.record(record.token()).is_some());
    }

    #[test]
    fn pairing_token_issue_prunes_expired_records() {
        let mut manager = PairingTokenManager::new();
        let expired = manager.issue(timestamp(1000), 500).unwrap().token().clone();

        let active = manager.issue(timestamp(1500), 500).unwrap().token().clone();

        assert!(manager.record(&expired).is_none());
        assert!(manager.record(&active).is_some());
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn pairing_token_can_be_consumed_only_once_before_expiration() {
        let mut manager = PairingTokenManager::new();
        let issued = manager.issue(timestamp(1000), 500).unwrap();
        let token = issued.token().clone();

        let consumed = manager.consume(&token, timestamp(1499)).unwrap();

        assert_eq!(consumed.state(), PairingTokenState::Consumed);
        assert_eq!(
            manager.consume(&token, timestamp(1499)).unwrap_err(),
            PairingError::AlreadyUsedToken
        );
    }

    #[test]
    fn pairing_token_consume_prunes_other_expired_records() {
        let mut manager = PairingTokenManager::new();
        let expired = manager.issue(timestamp(1000), 500).unwrap().token().clone();
        let active = manager
            .issue(timestamp(1000), 1000)
            .unwrap()
            .token()
            .clone();

        manager.consume(&active, timestamp(1500)).unwrap();

        assert!(manager.record(&expired).is_none());
        assert_eq!(
            manager.record(&active).unwrap().state(),
            PairingTokenState::Consumed
        );
    }

    #[test]
    fn pairing_token_rejects_expired_token() {
        let mut manager = PairingTokenManager::new();
        let issued = manager.issue(timestamp(1000), 500).unwrap();
        let token = issued.token().clone();

        assert_eq!(
            manager.consume(&token, timestamp(1500)).unwrap_err(),
            PairingError::ExpiredToken {
                expires_at_ms: timestamp(1500),
                now_ms: timestamp(1500),
            }
        );
        assert!(manager.record(&token).is_none());
    }

    #[test]
    fn pairing_token_rejects_revoked_token() {
        let mut manager = PairingTokenManager::new();
        let issued = manager.issue(timestamp(1000), 500).unwrap();
        let token = issued.token().clone();

        assert!(manager.revoke(&token));

        assert_eq!(
            manager.consume(&token, timestamp(1200)).unwrap_err(),
            PairingError::RevokedToken
        );
    }

    #[test]
    fn pairing_token_prunes_expired_records() {
        let mut manager = PairingTokenManager::new();
        let expired = manager.issue(timestamp(1000), 500).unwrap().token().clone();
        let active = manager
            .issue(timestamp(1000), 1000)
            .unwrap()
            .token()
            .clone();

        assert_eq!(manager.prune_expired(timestamp(1500)), 1);

        assert!(manager.record(&expired).is_none());
        assert!(manager.record(&active).is_some());
    }

    #[test]
    fn pairing_token_rejects_invalid_ttl() {
        let mut manager = PairingTokenManager::new();

        assert_eq!(
            manager.issue(timestamp(1000), 0).unwrap_err(),
            PairingError::InvalidTtl { ttl_ms: 0 }
        );
    }

    #[test]
    fn session_token_issue_records_expiration_time() {
        let mut manager = SessionTokenManager::new();
        let device_id = DeviceId::new();
        let server_id = ServerId::new();

        let record = manager
            .issue(server_id, device_id, timestamp(1000), 60_000)
            .unwrap();

        assert_eq!(record.server_id(), server_id);
        assert_eq!(record.device_id(), device_id);
        assert_eq!(record.issued_at_ms(), timestamp(1000));
        assert_eq!(record.expires_at_ms(), timestamp(61_000));
        assert!(!record.token().0.is_empty());
        assert!(manager.record(record.token()).is_some());
    }

    #[test]
    fn session_token_rejects_expired_token() {
        let mut manager = SessionTokenManager::new();
        let issued = manager
            .issue(ServerId::new(), DeviceId::new(), timestamp(1000), 500)
            .unwrap();
        let token = issued.token().clone();

        assert_eq!(
            manager.verify(&token, timestamp(1500)).unwrap_err(),
            SessionTokenError::ExpiredToken {
                expires_at_ms: timestamp(1500),
                now_ms: timestamp(1500),
            }
        );
        assert!(manager.record(&token).is_none());
    }

    #[test]
    fn session_token_verifies_server_and_device_binding() {
        let mut manager = SessionTokenManager::new();
        let server_id = ServerId::new();
        let device_id = DeviceId::new();
        let record = manager
            .issue(server_id, device_id, timestamp(1000), 500)
            .unwrap();

        let verified = manager.verify(record.token(), timestamp(1200)).unwrap();
        assert_eq!(verified.server_id(), server_id);
        assert_eq!(verified.device_id(), device_id);
    }

    #[test]
    fn challenge_issue_records_expiration_time() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();

        let record = manager.issue(device_id, timestamp(1000), 500).unwrap();

        assert_eq!(record.device_id(), device_id);
        assert_eq!(record.issued_at_ms(), timestamp(1000));
        assert_eq!(record.expires_at_ms(), timestamp(1500));
        assert_eq!(record.state(), AuthChallengeState::Active);
        assert!(!record.challenge().0.is_empty());
    }

    #[test]
    fn auth_challenge_issue_prunes_expired_records() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let expired = manager
            .issue(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();

        let active = manager
            .issue(device_id, timestamp(1500), 500)
            .unwrap()
            .challenge()
            .clone();

        assert!(manager.record(&expired).is_none());
        assert!(manager.record(&active).is_some());
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn auth_challenge_issue_prunes_consumed_records() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let consumed = manager
            .issue(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        manager
            .consume(&device_id, &consumed, timestamp(1100))
            .unwrap();

        let active = manager
            .issue(device_id, timestamp(1100), 500)
            .unwrap()
            .challenge()
            .clone();

        assert!(manager.record(&consumed).is_none());
        assert!(manager.record(&active).is_some());
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn auth_challenge_issue_caps_outstanding_records_per_device() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let other_device_id = DeviceId::new();
        let other_challenge = manager
            .issue(other_device_id, timestamp(1000), 60_000)
            .unwrap()
            .challenge()
            .clone();
        let mut challenges = Vec::new();

        for offset in 0..=AuthChallengeManager::MAX_OUTSTANDING_PER_DEVICE {
            challenges.push(
                manager
                    .issue(device_id, timestamp(1000 + offset as u64), 60_000)
                    .unwrap()
                    .challenge()
                    .clone(),
            );
        }

        assert!(manager.record(&challenges[0]).is_none());
        assert!(manager.record(challenges.last().unwrap()).is_some());
        assert!(manager.record(&other_challenge).is_some());
        assert_eq!(
            manager.len(),
            AuthChallengeManager::MAX_OUTSTANDING_PER_DEVICE + 1
        );
    }

    #[test]
    fn challenge_can_be_consumed_only_once_before_expiration() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let issued = manager.issue(device_id, timestamp(1000), 500).unwrap();
        let challenge = issued.challenge().clone();

        let consumed = manager
            .consume(&device_id, &challenge, timestamp(1499))
            .unwrap();

        assert_eq!(consumed.state(), AuthChallengeState::Consumed);
        assert_eq!(
            manager
                .consume(&device_id, &challenge, timestamp(1499))
                .unwrap_err(),
            ChallengeError::UsedChallenge
        );
    }

    #[test]
    fn challenge_rejects_expired_record() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let issued = manager.issue(device_id, timestamp(1000), 500).unwrap();
        let challenge = issued.challenge().clone();

        assert_eq!(
            manager
                .consume(&device_id, &challenge, timestamp(1500))
                .unwrap_err(),
            ChallengeError::ExpiredChallenge {
                expires_at_ms: timestamp(1500),
                now_ms: timestamp(1500),
            }
        );
        assert!(manager.record(&challenge).is_none());
    }

    #[test]
    fn challenge_rejects_revoked_record() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let issued = manager.issue(device_id, timestamp(1000), 500).unwrap();
        let challenge = issued.challenge().clone();

        assert!(manager.revoke(&device_id, &challenge));

        assert_eq!(
            manager
                .consume(&device_id, &challenge, timestamp(1200))
                .unwrap_err(),
            ChallengeError::RevokedChallenge
        );
    }

    #[test]
    fn challenge_device_mismatch_consumes_record() {
        let mut manager = AuthChallengeManager::new();
        let expected_device_id = DeviceId::new();
        let actual_device_id = DeviceId::new();
        let challenge = manager
            .issue(expected_device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();

        assert_eq!(
            manager
                .consume(&actual_device_id, &challenge, timestamp(1100))
                .unwrap_err(),
            ChallengeError::DeviceMismatch {
                expected_device_id,
                actual_device_id,
            }
        );
        assert_eq!(
            manager
                .consume(&expected_device_id, &challenge, timestamp(1100))
                .unwrap_err(),
            ChallengeError::UsedChallenge
        );
    }

    #[test]
    fn challenge_prunes_expired_records() {
        let mut manager = AuthChallengeManager::new();
        let device_id = DeviceId::new();
        let expired = manager
            .issue(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let active = manager
            .issue(device_id, timestamp(1000), 1000)
            .unwrap()
            .challenge()
            .clone();

        assert_eq!(manager.prune_expired(timestamp(1500)), 1);

        assert!(manager.record(&expired).is_none());
        assert!(manager.record(&active).is_some());
    }

    #[test]
    fn replay_rejects_timestamp_outside_allowed_window() {
        let mut protector = ReplayProtector::new(500);
        let device_id = DeviceId::new();

        assert_eq!(
            protector
                .check_and_record(&device_id, &nonce("old"), timestamp(499), timestamp(1000))
                .unwrap_err(),
            ReplayError::TimestampOutOfWindow {
                timestamp_ms: timestamp(499),
                now_ms: timestamp(1000),
                allowed_clock_skew_ms: 500,
            }
        );
        assert_eq!(
            protector
                .check_and_record(
                    &device_id,
                    &nonce("future"),
                    timestamp(1501),
                    timestamp(1000)
                )
                .unwrap_err(),
            ReplayError::TimestampOutOfWindow {
                timestamp_ms: timestamp(1501),
                now_ms: timestamp(1000),
                allowed_clock_skew_ms: 500,
            }
        );
    }

    #[test]
    fn replay_rejects_repeated_nonce_for_same_device() {
        let mut protector = ReplayProtector::new(500);
        let device_id = DeviceId::new();
        let nonce = nonce("same-nonce");

        protector
            .check_and_record(&device_id, &nonce, timestamp(1000), timestamp(1000))
            .unwrap();

        assert_eq!(
            protector
                .check_and_record(&device_id, &nonce, timestamp(1000), timestamp(1000))
                .unwrap_err(),
            ReplayError::ReplayedNonce { device_id }
        );
    }

    #[test]
    fn replay_allows_same_nonce_for_different_devices() {
        let mut protector = ReplayProtector::new(500);
        let first_device_id = DeviceId::new();
        let second_device_id = DeviceId::new();
        let nonce = nonce("shared-nonce");

        protector
            .check_and_record(&first_device_id, &nonce, timestamp(1000), timestamp(1000))
            .unwrap();
        protector
            .check_and_record(&second_device_id, &nonce, timestamp(1000), timestamp(1000))
            .unwrap();
    }

    #[test]
    fn replay_prunes_nonce_records_by_window() {
        let mut protector = ReplayProtector::new(500);
        let device_id = DeviceId::new();
        let nonce = nonce("pruned-nonce");

        protector
            .check_and_record(&device_id, &nonce, timestamp(1000), timestamp(1000))
            .unwrap();

        assert_eq!(protector.prune(timestamp(1499)), 0);
        assert_eq!(protector.prune(timestamp(1500)), 0);
        assert_eq!(protector.prune(timestamp(1501)), 1);
        protector
            .check_and_record(&device_id, &nonce, timestamp(1500), timestamp(1500))
            .unwrap();
    }

    #[test]
    fn replay_rejects_nonce_replay_at_inclusive_window_boundary() {
        let mut protector = ReplayProtector::new(500);
        let device_id = DeviceId::new();
        let nonce = nonce("boundary-nonce");

        protector
            .check_and_record(&device_id, &nonce, timestamp(1000), timestamp(1000))
            .unwrap();

        assert_eq!(
            protector
                .check_and_record(&device_id, &nonce, timestamp(1000), timestamp(1500))
                .unwrap_err(),
            ReplayError::ReplayedNonce { device_id }
        );
    }

    #[test]
    fn auth_rejects_unpaired_device() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::new(500),
        );
        let challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let payload = termd_proto::AuthPayload {
            device_id,
            challenge,
            nonce: nonce("auth-nonce"),
            timestamp_ms: timestamp(1100),
            signature: signature("sig"),
        };
        let verifier = RejectingVerifier;

        assert_eq!(
            service
                .authenticate(payload, timestamp(1100), &mut trusted_store, &verifier)
                .unwrap_err(),
            ChallengeAuthError::UntrustedDevice { device_id }
        );
        assert!(trusted_store.trusted_device(&device_id).is_none());
    }

    #[test]
    fn auth_invalid_signature_does_not_mark_seen_and_consumes_challenge() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = public_key("device-a-public");
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        trusted_store.trust_device(
            DeviceIdentity::new(device_id, device_public_key.clone()),
            timestamp(1000),
            None,
        );
        let mut service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::new(500),
        );
        let challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let payload = termd_proto::AuthPayload {
            device_id,
            challenge,
            nonce: nonce("auth-nonce"),
            timestamp_ms: timestamp(1100),
            signature: signature("bad-sig"),
        };
        let expected_input =
            AuthSigningInput::from_payload(&payload, service.daemon_public_identity()).to_bytes();
        let verifier =
            ExactSignatureVerifier::new(device_public_key, expected_input, signature("good-sig"));

        assert_eq!(
            service
                .authenticate(
                    payload.clone(),
                    timestamp(1100),
                    &mut trusted_store,
                    &verifier
                )
                .unwrap_err(),
            ChallengeAuthError::InvalidSignature
        );
        assert_eq!(
            trusted_store
                .trusted_device(&device_id)
                .unwrap()
                .last_seen_at_ms(),
            None
        );
        assert_eq!(
            service
                .authenticate(payload, timestamp(1100), &mut trusted_store, &verifier)
                .unwrap_err(),
            ChallengeAuthError::UsedChallenge
        );
    }

    #[test]
    fn auth_invalid_signature_does_not_consume_replay_nonce() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = public_key("device-a-public");
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        trusted_store.trust_device(
            DeviceIdentity::new(device_id, device_public_key.clone()),
            timestamp(1000),
            None,
        );
        let mut service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::new(500),
        );
        let first_challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let first_payload = termd_proto::AuthPayload {
            device_id,
            challenge: first_challenge,
            nonce: nonce("auth-reused-after-bad-signature"),
            timestamp_ms: timestamp(1100),
            signature: signature("bad-sig"),
        };
        let verifier = ExactSignatureVerifier::new(
            device_public_key.clone(),
            AuthSigningInput::from_payload(&first_payload, service.daemon_public_identity())
                .to_bytes(),
            signature("good-sig"),
        );

        assert_eq!(
            service
                .authenticate(
                    first_payload,
                    timestamp(1100),
                    &mut trusted_store,
                    &verifier
                )
                .unwrap_err(),
            ChallengeAuthError::InvalidSignature
        );

        let second_challenge = service
            .issue_challenge(device_id, timestamp(1101), 500)
            .unwrap()
            .challenge()
            .clone();
        let second_payload = termd_proto::AuthPayload {
            device_id,
            challenge: second_challenge,
            nonce: nonce("auth-reused-after-bad-signature"),
            timestamp_ms: timestamp(1101),
            signature: signature("good-sig"),
        };
        let verifier = ExactSignatureVerifier::new(
            device_public_key,
            AuthSigningInput::from_payload(&second_payload, service.daemon_public_identity())
                .to_bytes(),
            signature("good-sig"),
        );

        service
            .authenticate(
                second_payload,
                timestamp(1101),
                &mut trusted_store,
                &verifier,
            )
            .unwrap();
    }

    #[test]
    fn auth_verification_prunes_expired_and_consumed_challenges() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = public_key("device-a-public");
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        trusted_store.trust_device(
            DeviceIdentity::new(device_id, device_public_key.clone()),
            timestamp(1000),
            None,
        );
        let mut service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::new(500),
        );
        let current_challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let expired_challenge = service
            .issue_challenge(device_id, timestamp(1000), 100)
            .unwrap()
            .challenge()
            .clone();
        let consumed_challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        service
            .challenge_manager_mut()
            .consume(&device_id, &consumed_challenge, timestamp(1001))
            .unwrap();
        let payload = termd_proto::AuthPayload {
            device_id,
            challenge: current_challenge,
            nonce: nonce("auth-cleanup-nonce"),
            timestamp_ms: timestamp(1100),
            signature: signature("good-sig"),
        };
        let verifier = ExactSignatureVerifier::new(
            device_public_key,
            AuthSigningInput::from_payload(&payload, service.daemon_public_identity()).to_bytes(),
            signature("good-sig"),
        );

        service
            .authenticate(payload, timestamp(1100), &mut trusted_store, &verifier)
            .unwrap();

        assert!(
            service
                .challenge_manager()
                .record(&expired_challenge)
                .is_none()
        );
        assert!(
            service
                .challenge_manager()
                .record(&consumed_challenge)
                .is_none()
        );
    }

    #[test]
    fn auth_valid_signature_trusted_device_fresh_nonce_and_challenge_marks_seen() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = public_key("device-a-public");
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        trusted_store.trust_device(
            DeviceIdentity::new(device_id, device_public_key.clone()),
            timestamp(1000),
            None,
        );
        let mut service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::new(500),
        );
        let challenge = service
            .issue_challenge(device_id, timestamp(1000), 500)
            .unwrap()
            .challenge()
            .clone();
        let payload = termd_proto::AuthPayload {
            device_id,
            challenge,
            nonce: nonce("auth-nonce"),
            timestamp_ms: timestamp(1100),
            signature: signature("good-sig"),
        };
        let expected_input =
            AuthSigningInput::from_payload(&payload, service.daemon_public_identity()).to_bytes();
        let verifier =
            ExactSignatureVerifier::new(device_public_key, expected_input, signature("good-sig"));

        let authenticated = service
            .authenticate(payload, timestamp(1100), &mut trusted_store, &verifier)
            .unwrap();

        assert_eq!(authenticated.device_id, device_id);
        assert_eq!(
            trusted_store
                .trusted_device(&device_id)
                .unwrap()
                .last_seen_at_ms(),
            Some(timestamp(1100))
        );
    }

    #[test]
    fn auth_signing_input_has_stable_field_order() {
        let daemon_identity = DaemonIdentity::generate().public_identity();
        let payload = termd_proto::AuthPayload {
            device_id: DeviceId::new(),
            challenge: termd_proto::Challenge("challenge-a".to_owned()),
            nonce: nonce("nonce-a"),
            timestamp_ms: timestamp(1100),
            signature: signature("sig"),
        };

        let bytes = AuthSigningInput::from_payload(&payload, &daemon_identity).to_bytes();
        let text = String::from_utf8(bytes).unwrap();

        let server_pos = text.find("server_id:").unwrap();
        let daemon_key_pos = text.find("daemon_public_key:").unwrap();
        let device_pos = text.find("device_id:").unwrap();
        let challenge_pos = text.find("challenge:").unwrap();
        let nonce_pos = text.find("nonce:").unwrap();
        let timestamp_pos = text.find("timestamp_ms:").unwrap();

        assert!(server_pos < daemon_key_pos);
        assert!(daemon_key_pos < device_pos);
        assert!(device_pos < challenge_pos);
        assert!(challenge_pos < nonce_pos);
        assert!(nonce_pos < timestamp_pos);
    }

    #[test]
    fn daemon_identity_signs_e2ee_key_exchange_material() {
        let identity = DaemonIdentity::generate();
        let public_identity = identity.public_identity();
        let payload = E2eeKeyExchangePayload::new(
            public_identity.server_id,
            DeviceId::default(),
            public_key("x25519-v1:daemon-e2ee-public"),
            nonce("daemon-e2ee-nonce"),
            timestamp(1234),
        );
        let signing_input =
            DaemonE2eeSigningInput::from_payload(&payload, &public_identity).to_bytes();
        let signature = identity.sign_to_wire(&signing_input).unwrap();

        let public_key_bytes = decode_test_wire_bytes(&public_identity.public_key.0, 32);
        let signature_bytes = decode_test_wire_bytes(&signature.0, 64);
        let verifying_key =
            VerifyingKey::from_bytes(&public_key_bytes.try_into().unwrap()).unwrap();
        let signature = Ed25519Signature::from_slice(&signature_bytes).unwrap();

        verifying_key.verify(&signing_input, &signature).unwrap();
    }

    #[test]
    fn auth_signing_input_can_bind_current_e2ee_transcript() {
        let identity = DaemonIdentity::generate();
        let daemon = identity.public_identity();
        let server_exchange = E2eeKeyExchangePayload::new(
            daemon.server_id,
            DeviceId::default(),
            public_key("x25519-v1:daemon-session-key"),
            nonce("server-nonce"),
            timestamp(1000),
        )
        .with_signature(signature("ed25519-v1:server-signature"));
        let device_id = DeviceId::new();
        let device_exchange = E2eeKeyExchangePayload::new(
            daemon.server_id,
            device_id,
            public_key("x25519-v1:device-session-key"),
            nonce("device-nonce"),
            timestamp(1001),
        );
        let transcript =
            E2eeAuthTranscript::from_key_exchanges(&server_exchange, &device_exchange, &daemon);
        let payload = termd_proto::AuthPayload {
            device_id,
            challenge: termd_proto::Challenge("challenge-a".to_owned()),
            nonce: nonce("auth-nonce"),
            timestamp_ms: timestamp(1100),
            signature: signature("sig"),
        };

        let unbound = AuthSigningInput::from_payload(&payload, &daemon).to_bytes();
        let bound = AuthSigningInput::from_payload_with_e2ee_transcript(
            &payload,
            &daemon,
            Some(&transcript),
        )
        .to_bytes();
        let bound_text = String::from_utf8(bound.clone()).unwrap();

        assert_ne!(unbound, bound);
        assert!(bound_text.contains("e2ee_transcript_sha256:"));
    }

    #[test]
    fn auth_signing_input_field_changes_cause_signature_verification_failure() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = public_key("device-a-public");

        for mutation in [
            SigningInputMutation::Challenge,
            SigningInputMutation::Nonce,
            SigningInputMutation::Timestamp,
            SigningInputMutation::DeviceId,
        ] {
            let mut trusted_store = InMemoryTrustedDeviceStore::new();
            let auth_device_id = if mutation == SigningInputMutation::DeviceId {
                DeviceId::new()
            } else {
                device_id
            };

            trusted_store.trust_device(
                DeviceIdentity::new(auth_device_id, device_public_key.clone()),
                timestamp(1000),
                None,
            );

            let mut service = ChallengeResponseService::new(
                daemon_identity.public_identity(),
                AuthChallengeManager::new(),
                ReplayProtector::new(500),
            );
            let issued_challenge = service
                .issue_challenge(auth_device_id, timestamp(1000), 500)
                .unwrap()
                .challenge()
                .clone();

            let signed_payload = termd_proto::AuthPayload {
                device_id,
                challenge: if mutation == SigningInputMutation::Challenge {
                    termd_proto::Challenge("signed-different-challenge".to_owned())
                } else {
                    issued_challenge.clone()
                },
                nonce: nonce("signed-nonce"),
                timestamp_ms: timestamp(1100),
                signature: signature("good-sig"),
            };
            let signed_input =
                AuthSigningInput::from_payload(&signed_payload, &daemon_identity.public_identity())
                    .to_bytes();

            let mut payload = termd_proto::AuthPayload {
                device_id: auth_device_id,
                challenge: issued_challenge,
                nonce: signed_payload.nonce.clone(),
                timestamp_ms: signed_payload.timestamp_ms,
                signature: signed_payload.signature.clone(),
            };

            match mutation {
                SigningInputMutation::Challenge => {}
                SigningInputMutation::Nonce => {
                    payload.nonce = nonce("changed-nonce");
                }
                SigningInputMutation::Timestamp => {
                    payload.timestamp_ms = timestamp(1101);
                }
                SigningInputMutation::DeviceId => {}
            }

            let verifier = ExactSignatureVerifier::new(
                device_public_key.clone(),
                signed_input,
                signature("good-sig"),
            );

            assert_eq!(
                service
                    .authenticate(payload, timestamp(1100), &mut trusted_store, &verifier)
                    .unwrap_err(),
                ChallengeAuthError::InvalidSignature
            );
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SigningInputMutation {
        Challenge,
        Nonce,
        Timestamp,
        DeviceId,
    }

    struct RejectingVerifier;

    impl SignatureVerifier for RejectingVerifier {
        fn verify(
            &self,
            _device_public_key: &PublicKey,
            _signing_input: &[u8],
            _signature: &termd_proto::Signature,
        ) -> Result<(), SignatureError> {
            Err(SignatureError::InvalidSignature)
        }
    }

    struct ExactSignatureVerifier {
        expected_public_key: PublicKey,
        expected_input: Vec<u8>,
        expected_signature: termd_proto::Signature,
    }

    impl ExactSignatureVerifier {
        fn new(
            expected_public_key: PublicKey,
            expected_input: Vec<u8>,
            expected_signature: termd_proto::Signature,
        ) -> Self {
            Self {
                expected_public_key,
                expected_input,
                expected_signature,
            }
        }
    }

    impl SignatureVerifier for ExactSignatureVerifier {
        fn verify(
            &self,
            device_public_key: &PublicKey,
            signing_input: &[u8],
            signature: &termd_proto::Signature,
        ) -> Result<(), SignatureError> {
            if device_public_key == &self.expected_public_key
                && signing_input == self.expected_input.as_slice()
                && signature == &self.expected_signature
            {
                Ok(())
            } else {
                Err(SignatureError::InvalidSignature)
            }
        }
    }

    #[test]
    fn pair_request_trusts_device_and_returns_daemon_public_identity() {
        let daemon_identity = DaemonIdentity::generate();
        let daemon_public_identity = daemon_identity.public_identity();
        let device_id = DeviceId::new();
        let device_public_key = ed25519_public_key_wire(7);
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 500).unwrap();

        let accept = service
            .accept_pair_request(
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: device_public_key.clone(),
                    token: issued.token().clone(),
                    nonce: termd_proto::Nonce("pair-nonce".to_owned()),
                    timestamp_ms: timestamp(1100),
                },
                timestamp(1100),
                &daemon_identity,
                &mut trusted_store,
            )
            .unwrap();

        assert_eq!(accept.server_id, daemon_public_identity.server_id);
        assert_eq!(accept.daemon_public_key, daemon_public_identity.public_key);
        assert_eq!(accept.device_id, device_id);
        assert_eq!(accept.expires_at_ms, timestamp(1500));
        assert!(
            trusted_store.is_trusted_identity(&DeviceIdentity::new(device_id, device_public_key))
        );
    }

    #[test]
    fn consume_pair_request_waits_for_caller_to_trust_device() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = ed25519_public_key_wire(8);
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 500).unwrap();

        let pending = service
            .consume_pair_request(
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: device_public_key.clone(),
                    token: issued.token().clone(),
                    nonce: termd_proto::Nonce("pair-pending-nonce".to_owned()),
                    timestamp_ms: timestamp(1100),
                },
                timestamp(1100),
                &daemon_identity,
            )
            .unwrap();

        let identity = DeviceIdentity::new(device_id, device_public_key);
        assert!(!trusted_store.is_trusted_identity(&identity));

        PairingService::trust_pending_pairing(&pending, timestamp(1100), &mut trusted_store);

        assert!(trusted_store.is_trusted_identity(&identity));
        assert_eq!(pending.accepted().device_id, device_id);
    }

    #[test]
    fn pairing_reservation_can_be_released_and_retried_before_expiry() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = ed25519_public_key_wire(18);
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 500).unwrap();
        let request = termd_proto::PairRequestPayload {
            device_id,
            device_public_key,
            token: issued.token().clone(),
            nonce: termd_proto::Nonce("pair-reservation".to_owned()),
            timestamp_ms: timestamp(1100),
        };

        let first = service
            .reserve_pair_request(request.clone(), timestamp(1100), &daemon_identity)
            .unwrap();
        assert_eq!(
            service
                .reserve_pair_request(request.clone(), timestamp(1101), &daemon_identity)
                .unwrap_err(),
            PairingError::ReservedToken
        );
        assert!(service.release_pairing_reservation(&first));
        let second = service
            .reserve_pair_request(request, timestamp(1102), &daemon_identity)
            .unwrap();
        service
            .commit_pairing_reservation(&second, timestamp(1103))
            .unwrap();
        assert_eq!(
            service
                .token_manager()
                .record(issued.token())
                .unwrap()
                .state(),
            PairingTokenState::Consumed
        );
    }

    #[test]
    fn pairing_reservation_cannot_commit_after_expiry() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 100).unwrap();
        let pending = service
            .reserve_pair_request(
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: ed25519_public_key_wire(19),
                    token: issued.token().clone(),
                    nonce: termd_proto::Nonce("pair-expiry".to_owned()),
                    timestamp_ms: timestamp(1050),
                },
                timestamp(1050),
                &daemon_identity,
            )
            .unwrap();

        assert_eq!(
            service
                .commit_pairing_reservation(&pending, timestamp(1100))
                .unwrap_err(),
            PairingError::ExpiredToken {
                expires_at_ms: timestamp(1100),
                now_ms: timestamp(1100),
            }
        );
    }

    #[test]
    fn pair_request_with_invalid_token_does_not_trust_device() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = ed25519_public_key_wire(9);
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = PairingService::new(PairingTokenManager::new());

        let result = service.accept_pair_request(
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key,
                token: termd_proto::PairingToken("unknown-token".to_owned()),
                nonce: termd_proto::Nonce("pair-nonce".to_owned()),
                timestamp_ms: timestamp(1100),
            },
            timestamp(1100),
            &daemon_identity,
            &mut trusted_store,
        );

        assert_eq!(result.unwrap_err(), PairingError::InvalidToken);
        assert!(!trusted_store.is_trusted(&device_id));
    }

    #[test]
    fn pair_request_with_expired_token_does_not_trust_device() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let device_public_key = ed25519_public_key_wire(10);
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 500).unwrap();

        let result = service.accept_pair_request(
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key,
                token: issued.token().clone(),
                nonce: termd_proto::Nonce("pair-nonce".to_owned()),
                timestamp_ms: timestamp(1600),
            },
            timestamp(1600),
            &daemon_identity,
            &mut trusted_store,
        );

        assert_eq!(
            result.unwrap_err(),
            PairingError::ExpiredToken {
                expires_at_ms: timestamp(1500),
                now_ms: timestamp(1600),
            }
        );
        assert!(!trusted_store.is_trusted(&device_id));
    }

    #[test]
    fn pair_request_with_invalid_device_public_key_is_rejected_without_consuming_token() {
        let daemon_identity = DaemonIdentity::generate();
        let device_id = DeviceId::new();
        let valid_device_public_key = ed25519_public_key_wire(11);
        let mut trusted_store = InMemoryTrustedDeviceStore::new();
        let mut service = PairingService::new(PairingTokenManager::new());
        let issued = service.issue_token(timestamp(1000), 500).unwrap();

        let error = service
            .accept_pair_request(
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: public_key("ed25519-v1:not-base64"),
                    token: issued.token().clone(),
                    nonce: termd_proto::Nonce("pair-nonce".to_owned()),
                    timestamp_ms: timestamp(1100),
                },
                timestamp(1100),
                &daemon_identity,
                &mut trusted_store,
            )
            .unwrap_err();

        assert_eq!(error, PairingError::InvalidDevicePublicKey);
        assert!(!trusted_store.is_trusted(&device_id));

        let accept = service
            .accept_pair_request(
                termd_proto::PairRequestPayload {
                    device_id,
                    device_public_key: valid_device_public_key.clone(),
                    token: issued.token().clone(),
                    nonce: termd_proto::Nonce("pair-nonce-2".to_owned()),
                    timestamp_ms: timestamp(1101),
                },
                timestamp(1101),
                &daemon_identity,
                &mut trusted_store,
            )
            .unwrap();

        assert_eq!(accept.device_id, device_id);
        assert!(
            trusted_store
                .is_trusted_identity(&DeviceIdentity::new(device_id, valid_device_public_key,))
        );
    }
}
