//! termd daemon 的 WebSocket 协议状态机核心。
//!
//! 本模块不依赖真实 socket，便于单元测试直接驱动 hello、E2EE、pair/auth 和 session
//! 操作。Axum 只负责把网络帧转成这里的统一 envelope。

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd_proto::{
    AttachRole, AuthChallengePayload, AuthPayload, ControlGrantPayload, ControlRequestPayload,
    DeviceId, E2eeKeyExchangePayload, EncryptedFramePayload, Envelope, ErrorPayload, HelloPayload,
    MessageType, Nonce, PairRequestPayload, PingPayload, PongPayload, ProtocolVersion, ServerId,
    SessionAttachPayload, SessionAttachedPayload, SessionCreatePayload, SessionCreatedPayload,
    SessionDataPayload, SessionId, SessionListPayload, SessionListResultPayload,
    SessionResizePayload, SessionState, SessionSummaryPayload, TerminalSize, UnixTimestampMillis,
};
use thiserror::Error;

use crate::auth::{
    AuthChallengeManager, ChallengeResponseService, DaemonIdentity, DaemonPublicIdentity,
    InMemoryTrustedDeviceStore, PairingService, PairingTokenManager, ReplayProtector,
    SignatureVerifier, TrustedDeviceStore, current_unix_timestamp_millis,
};
use crate::config::DaemonConfig;
use crate::pty::{CommandSpec, PtyBackend};
use crate::runtime::{RuntimeError, SessionRuntime};
use crate::session::{
    AttachRole as RuntimeAttachRole, SessionState as RuntimeSessionState,
    TerminalSize as RuntimeTerminalSize,
};

use super::{
    E2eeError, E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
};

const AUTH_CHALLENGE_TTL_MS: u64 = 60_000;

/// 协议层统一使用的 JSON envelope。
pub type JsonEnvelope = Envelope<Value>;

/// WebSocket 连接的协议阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolConnectionState {
    Init,
    Auth,
    Authenticated,
    Attached,
    Closed,
}

/// 协议错误会被映射为脱敏 `error` payload。
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("connection is in an invalid protocol state")]
    InvalidState,
    #[error("message envelope is invalid")]
    InvalidEnvelope,
    #[error("E2EE frame processing failed")]
    E2eeFailed,
    #[error("device must authenticate before session operations")]
    Unauthenticated,
    #[error("device authentication failed")]
    AuthFailed,
    #[error("pairing failed")]
    PairingFailed,
    #[error("session was not found")]
    SessionNotFound,
    #[error("session input requires controller")]
    ControllerRequired,
    #[error("runtime operation failed")]
    RuntimeFailed,
}

impl ProtocolError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidState => "invalid_state",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::E2eeFailed => "e2ee_failed",
            Self::Unauthenticated => "unauthenticated",
            Self::AuthFailed => "auth_failed",
            Self::PairingFailed => "pairing_failed",
            Self::SessionNotFound => "session_not_found",
            Self::ControllerRequired => "controller_required",
            Self::RuntimeFailed => "runtime_failed",
        }
    }

    pub fn safe_message(&self) -> &'static str {
        match self {
            Self::InvalidState => "connection is in an invalid protocol state",
            Self::InvalidEnvelope => "message envelope is invalid",
            Self::E2eeFailed => "E2EE frame processing failed",
            Self::Unauthenticated => "device must authenticate before session operations",
            Self::AuthFailed => "device authentication failed",
            Self::PairingFailed => "pairing failed",
            Self::SessionNotFound => "session was not found",
            Self::ControllerRequired => "session input requires controller",
            Self::RuntimeFailed => "runtime operation failed",
        }
    }
}

impl From<E2eeError> for ProtocolError {
    fn from(_: E2eeError) -> Self {
        Self::E2eeFailed
    }
}

/// daemon 单进程协议状态。该结构只属于 termd，不会放入 relay。
pub struct DaemonProtocol<B: PtyBackend, V> {
    config: DaemonConfig,
    daemon_identity: DaemonIdentity,
    e2ee_keypair: E2eeKeyPair,
    pairing_service: PairingService,
    auth_service: ChallengeResponseService,
    trusted_store: InMemoryTrustedDeviceStore,
    runtime: SessionRuntime<B>,
    verifier: V,
    session_index: HashMap<SessionId, String>,
}

impl<B, V> DaemonProtocol<B, V>
where
    B: PtyBackend,
    V: SignatureVerifier,
{
    /// 创建可测试的协议服务，调用方显式注入 PTY backend 和签名 verifier。
    pub fn new(config: DaemonConfig, backend: B, verifier: V) -> Self {
        let daemon_identity = DaemonIdentity::generate();
        let auth_service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::default(),
        );

        Self {
            config,
            daemon_identity,
            e2ee_keypair: E2eeKeyPair::generate(),
            pairing_service: PairingService::new(PairingTokenManager::new()),
            auth_service,
            trusted_store: InMemoryTrustedDeviceStore::new(),
            runtime: SessionRuntime::new(backend),
            verifier,
            session_index: HashMap::new(),
        }
    }

    pub fn server_id(&self) -> ServerId {
        self.daemon_identity.server_id()
    }

    pub fn daemon_public_identity(&self) -> &DaemonPublicIdentity {
        self.auth_service.daemon_public_identity()
    }

    pub fn e2ee_public_key(&self) -> E2eePeerPublicKey {
        self.e2ee_keypair.public_key()
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// 本地 CLI 或测试可通过服务层签发 token；WebSocket 不暴露 token 签发入口。
    pub fn issue_pairing_token(
        &mut self,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::PairingResult<crate::auth::PairingTokenRecord> {
        self.pairing_service
            .issue_token(now_ms, self.config.pairing_token_ttl_ms)
    }

    /// 创建一条新的协议连接，并返回 daemon 立即发送的明文握手消息。
    pub fn start_connection(&self) -> (ProtocolConnection, Vec<JsonEnvelope>) {
        let connection = ProtocolConnection::new();
        let now_ms = current_unix_timestamp_millis();
        let messages = vec![
            envelope_value(
                MessageType::Hello,
                HelloPayload {
                    protocol_version: ProtocolVersion::default(),
                    nonce: nonce(),
                    timestamp_ms: now_ms,
                    server_id: Some(self.server_id()),
                    device_id: None,
                },
            )
            .expect("hello payload should serialize"),
            envelope_value(
                MessageType::E2eeKeyExchange,
                E2eeKeyExchangePayload {
                    server_id: self.server_id(),
                    // server 尚不知道真实 device id；该字段在客户端回应时才作为 E2EE context 使用。
                    device_id: DeviceId::default(),
                    public_key: self.e2ee_keypair.public_key_wire(),
                    nonce: nonce(),
                    timestamp_ms: now_ms,
                },
            )
            .expect("key exchange payload should serialize"),
        ];

        (connection, messages)
    }

    fn accept_e2ee_key_exchange(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: E2eeKeyExchangePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        if connection.state != ProtocolConnectionState::Init {
            return Err(ProtocolError::InvalidState);
        }
        if payload.server_id != self.server_id() {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let peer_public_key = E2eePeerPublicKey::try_from(&payload.public_key)?;
        let context = E2eeSessionContext::new(
            self.server_id(),
            payload.device_id,
            self.e2ee_keypair.public_key(),
            peer_public_key,
        );
        let e2ee_session = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &self.e2ee_keypair,
            peer_public_key,
            context,
        )?;

        connection.device_id = Some(payload.device_id);
        connection.e2ee = Some(e2ee_session);
        connection.state = ProtocolConnectionState::Auth;

        if self.trusted_store.is_trusted(&payload.device_id) {
            let challenge = self
                .auth_service
                .issue_challenge(
                    payload.device_id,
                    current_unix_timestamp_millis(),
                    AUTH_CHALLENGE_TTL_MS,
                )
                .map_err(|_| ProtocolError::AuthFailed)?;
            let auth_challenge = envelope_value(
                MessageType::AuthChallenge,
                AuthChallengePayload {
                    device_id: payload.device_id,
                    challenge: challenge.challenge().clone(),
                    expires_at_ms: challenge.expires_at_ms(),
                },
            )?;

            return connection.encrypt_inner_messages(vec![auth_challenge]);
        }

        Ok(Vec::new())
    }

    fn handle_pair_request(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: PairRequestPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        if connection.state != ProtocolConnectionState::Auth {
            return Err(ProtocolError::InvalidState);
        }
        if Some(payload.device_id) != connection.device_id {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let accepted = self
            .pairing_service
            .accept_pair_request(
                payload,
                current_unix_timestamp_millis(),
                &self.daemon_identity,
                &mut self.trusted_store,
            )
            .map_err(|_| ProtocolError::PairingFailed)?;

        connection.authenticated_device_id = Some(accepted.device_id);
        connection.state = ProtocolConnectionState::Authenticated;

        Ok(vec![envelope_value(MessageType::PairAccept, accepted)?])
    }

    fn handle_auth(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: AuthPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        if connection.state != ProtocolConnectionState::Auth {
            return Err(ProtocolError::InvalidState);
        }
        if Some(payload.device_id) != connection.device_id {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let authenticated = self
            .auth_service
            .authenticate(
                payload,
                current_unix_timestamp_millis(),
                &mut self.trusted_store,
                &self.verifier,
            )
            .map_err(|_| ProtocolError::AuthFailed)?;

        connection.authenticated_device_id = Some(authenticated.device_id);
        connection.state = ProtocolConnectionState::Authenticated;
        Ok(Vec::new())
    }

    fn create_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionCreatePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let command = command_spec_from_payload(&payload.command, &self.config)?;
        let runtime_size = proto_size_to_runtime(payload.size);
        let internal_session_id = self
            .runtime
            .create_session(command, runtime_size)
            .map_err(map_runtime_error)?;
        let wire_session_id = SessionId::new();

        self.session_index
            .insert(wire_session_id, internal_session_id.clone());

        let role = self
            .runtime
            .attach(&internal_session_id, device_key(device_id))
            .map_err(map_runtime_error)?;
        connection.attach(wire_session_id);

        let response = SessionCreatedPayload {
            session_id: wire_session_id,
            role: runtime_role_to_proto(role),
            state: self.runtime_state_proto(&internal_session_id)?,
            size: self.runtime_size_proto(&internal_session_id)?,
        };

        connection.state = ProtocolConnectionState::Attached;
        Ok(vec![envelope_value(MessageType::SessionCreated, response)?])
    }

    fn attach_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionAttachPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        let role = self
            .runtime
            .attach(&internal_session_id, device_key(device_id))
            .map_err(map_runtime_error)?;
        connection.attach(payload.session_id);
        connection.state = ProtocolConnectionState::Attached;

        let response = SessionAttachedPayload {
            session_id: payload.session_id,
            role: runtime_role_to_proto(role),
            state: self.runtime_state_proto(&internal_session_id)?,
            size: self.runtime_size_proto(&internal_session_id)?,
        };

        Ok(vec![envelope_value(
            MessageType::SessionAttached,
            response,
        )?])
    }

    fn write_session_data(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionDataPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        connection.ensure_attached_to(payload.session_id)?;
        let bytes = general_purpose::STANDARD
            .decode(payload.data_base64)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;

        self.runtime
            .write_input(internal_session_id, &device_key(device_id), &bytes)
            .map_err(map_runtime_error)?;

        Ok(Vec::new())
    }

    fn resize_session(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionResizePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        connection.ensure_attached_to(payload.session_id)?;

        self.runtime
            .resize(internal_session_id, proto_size_to_runtime(payload.size))
            .map_err(map_runtime_error)?;

        Ok(Vec::new())
    }

    fn request_control(
        &mut self,
        connection: &ProtocolConnection,
        payload: ControlRequestPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        if payload.device_id != device_id {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        connection.ensure_attached_to(payload.session_id)?;
        self.runtime
            .steal_control(internal_session_id, &device_key(device_id))
            .map_err(map_runtime_error)?;

        let response = ControlGrantPayload {
            session_id: payload.session_id,
            device_id,
        };

        Ok(vec![envelope_value(MessageType::ControlGrant, response)?])
    }

    fn list_sessions(
        &mut self,
        connection: &ProtocolConnection,
        _payload: SessionListPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;

        let sessions = self
            .session_index
            .iter()
            .filter_map(|(wire_id, internal_id)| {
                let state = self.runtime.state(internal_id).ok()?;
                let size = self.runtime.size(internal_id).ok()?;
                Some(SessionSummaryPayload {
                    session_id: *wire_id,
                    state: runtime_state_to_proto(state),
                    size: runtime_size_to_proto(size),
                })
            })
            .collect();

        Ok(vec![envelope_value(
            MessageType::SessionListResult,
            SessionListResultPayload { sessions },
        )?])
    }

    fn detach_connection(&mut self, connection: &mut ProtocolConnection) {
        let Some(device_id) = connection.authenticated_device_id else {
            connection.state = ProtocolConnectionState::Closed;
            return;
        };
        let device_key = device_key(device_id);

        // 断开 WebSocket 只 detach 当前连接关联的 session，不 close/terminate PTY。
        for wire_session_id in connection.attached_sessions.drain(..) {
            if let Some(internal_session_id) = self.session_index.get(&wire_session_id) {
                let _ = self.runtime.detach(internal_session_id, &device_key);
            }
        }

        connection.state = ProtocolConnectionState::Closed;
    }

    fn runtime_state_proto(
        &self,
        internal_session_id: &str,
    ) -> Result<SessionState, ProtocolError> {
        self.runtime
            .state(internal_session_id)
            .map(runtime_state_to_proto)
            .map_err(map_runtime_error)
    }

    fn runtime_size_proto(&self, internal_session_id: &str) -> Result<TerminalSize, ProtocolError> {
        self.runtime
            .size(internal_session_id)
            .map(runtime_size_to_proto)
            .map_err(map_runtime_error)
    }
}

/// 单条 WebSocket 连接的状态。E2EE session 只属于当前连接。
pub struct ProtocolConnection {
    state: ProtocolConnectionState,
    device_id: Option<DeviceId>,
    authenticated_device_id: Option<DeviceId>,
    e2ee: Option<E2eeSession>,
    attached_sessions: Vec<SessionId>,
}

impl ProtocolConnection {
    fn new() -> Self {
        Self {
            state: ProtocolConnectionState::Init,
            device_id: None,
            authenticated_device_id: None,
            e2ee: None,
            attached_sessions: Vec::new(),
        }
    }

    pub fn state(&self) -> ProtocolConnectionState {
        self.state
    }

    pub fn authenticated_device_id(&self) -> Result<DeviceId, ProtocolError> {
        self.authenticated_device_id
            .ok_or(ProtocolError::Unauthenticated)
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated_device_id.is_some()
    }

    /// 处理来自 socket 的一条外层 envelope。错误会自动转换为可发送的 error envelope。
    pub fn handle_wire_envelope<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        envelope: JsonEnvelope,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_handle_wire_envelope(protocol, envelope) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response(error)],
        }
    }

    /// 从当前连接已 attach 的 runtime session 读取 PTY 输出，并按本连接 E2EE 会话加密。
    ///
    /// 这里是 daemon -> client 的核心输出路径：PTY 明文只在 daemon 内部 buffer 中短暂停留，
    /// 发送前会先封装为内层 `session_data` envelope，再加密成外层 `encrypted_frame`。
    pub fn read_session_output<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        max_bytes: usize,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_output(protocol, session_id, max_bytes) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response(error)],
        }
    }

    /// 批量 flush 当前连接已 attach 的所有 session 输出。
    ///
    /// server 层可以在处理完一条入站业务消息后调用本方法，完成 direct WebSocket 的最小
    /// 输出接线；持续后台 reader 可以后续在同一边界上扩展。
    pub fn read_attached_outputs<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        max_bytes_per_session: usize,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let session_ids = self.attached_sessions.clone();
        let mut outputs = Vec::new();

        for session_id in session_ids {
            outputs.extend(self.read_session_output(protocol, session_id, max_bytes_per_session));
        }

        outputs
    }

    pub fn close<B, V>(&mut self, protocol: &mut DaemonProtocol<B, V>)
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        protocol.detach_connection(self);
    }

    fn try_handle_wire_envelope<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        envelope: JsonEnvelope,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match envelope.kind {
            MessageType::E2eeKeyExchange => {
                let payload = decode_payload(envelope.payload)?;
                protocol.accept_e2ee_key_exchange(self, payload)
            }
            MessageType::EncryptedFrame => {
                let frame = decode_payload(envelope.payload)?;
                let inner: JsonEnvelope = self.e2ee_mut()?.decrypt_json_payload(&frame)?;
                let inner_responses = self.handle_inner_envelope(protocol, inner)?;
                self.encrypt_inner_messages(inner_responses)
            }
            _ => Err(ProtocolError::InvalidState),
        }
    }

    fn try_read_session_output<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        max_bytes: usize,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;

        let internal_session_id = protocol
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;

        if !self.attached_sessions.contains(&session_id) {
            return Err(ProtocolError::InvalidState);
        }

        if max_bytes == 0 {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0_u8; max_bytes];
        let read = protocol
            .runtime
            .read_output(&internal_session_id, &mut buffer)
            .map_err(map_runtime_error)?;
        if read == 0 {
            return Ok(Vec::new());
        }

        buffer.truncate(read);
        let inner = envelope_value(
            MessageType::SessionData,
            SessionDataPayload {
                session_id,
                data_base64: general_purpose::STANDARD.encode(buffer),
            },
        )?;

        self.encrypt_inner_messages(vec![inner])
    }

    fn handle_inner_envelope<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        envelope: JsonEnvelope,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match envelope.kind {
            MessageType::PairRequest => {
                let payload = decode_payload(envelope.payload)?;
                protocol.handle_pair_request(self, payload)
            }
            MessageType::Auth => {
                let payload = decode_payload(envelope.payload)?;
                protocol.handle_auth(self, payload)
            }
            MessageType::SessionCreate => {
                let payload = decode_payload(envelope.payload)?;
                protocol.create_session(self, payload)
            }
            MessageType::SessionAttach => {
                let payload = decode_payload(envelope.payload)?;
                protocol.attach_session(self, payload)
            }
            MessageType::SessionData => {
                let payload = decode_payload(envelope.payload)?;
                protocol.write_session_data(self, payload)
            }
            MessageType::SessionResize => {
                let payload = decode_payload(envelope.payload)?;
                protocol.resize_session(self, payload)
            }
            MessageType::ControlRequest => {
                let payload = decode_payload(envelope.payload)?;
                protocol.request_control(self, payload)
            }
            MessageType::SessionList => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_sessions(self, payload)
            }
            MessageType::Ping => {
                let payload: PingPayload = decode_payload(envelope.payload)?;
                Ok(vec![envelope_value(
                    MessageType::Pong,
                    PongPayload {
                        nonce: payload.nonce,
                        timestamp_ms: current_unix_timestamp_millis(),
                    },
                )?])
            }
            _ => Err(ProtocolError::InvalidState),
        }
    }

    fn e2ee_mut(&mut self) -> Result<&mut E2eeSession, ProtocolError> {
        self.e2ee.as_mut().ok_or(ProtocolError::InvalidState)
    }

    fn ensure_attached_to(&self, session_id: SessionId) -> Result<(), ProtocolError> {
        // 认证只证明 device key，有 session 作用域的操作还必须来自当前已 attach 的连接。
        // 这样同一设备新开的第二条连接不能借用旧连接在 runtime 中留下的 controller/viewer 角色。
        if self.attached_sessions.contains(&session_id) {
            return Ok(());
        }

        Err(ProtocolError::InvalidState)
    }

    fn encrypt_inner_messages(
        &mut self,
        messages: Vec<JsonEnvelope>,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        messages
            .into_iter()
            .map(|message| {
                let frame = self.e2ee_mut()?.encrypt_json_payload(&message)?;
                envelope_value(MessageType::EncryptedFrame, frame)
            })
            .collect()
    }

    fn error_response(&mut self, error: ProtocolError) -> JsonEnvelope {
        let error_envelope = envelope_value(
            MessageType::Error,
            ErrorPayload {
                code: error.code().to_owned(),
                message: error.safe_message().to_owned(),
            },
        )
        .expect("error payload should serialize");

        if self.e2ee.is_some() {
            if let Ok(mut encrypted) = self.encrypt_inner_messages(vec![error_envelope.clone()]) {
                if let Some(message) = encrypted.pop() {
                    return message;
                }
            }
        }

        error_envelope
    }

    fn attach(&mut self, session_id: SessionId) {
        if !self.attached_sessions.contains(&session_id) {
            self.attached_sessions.push(session_id);
        }
    }
}

pub fn envelope_value<T>(kind: MessageType, payload: T) -> Result<JsonEnvelope, ProtocolError>
where
    T: Serialize,
{
    Ok(Envelope::new(
        kind,
        serde_json::to_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)?,
    ))
}

pub fn decode_payload<T>(payload: Value) -> Result<T, ProtocolError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)
}

pub fn encrypted_frame_from_envelope(
    envelope: JsonEnvelope,
) -> Result<EncryptedFramePayload, ProtocolError> {
    if envelope.kind != MessageType::EncryptedFrame {
        return Err(ProtocolError::InvalidEnvelope);
    }

    decode_payload(envelope.payload)
}

fn command_spec_from_payload(
    requested_command: &[String],
    config: &DaemonConfig,
) -> Result<CommandSpec, ProtocolError> {
    let mut argv = if requested_command.is_empty() {
        config.default_command.clone()
    } else {
        requested_command.to_vec()
    };

    if argv.is_empty() {
        argv.push(config.default_shell.clone());
    }

    let program = argv.remove(0);
    if program.trim().is_empty() {
        return Err(ProtocolError::InvalidEnvelope);
    }

    Ok(CommandSpec::new(program).args(argv))
}

fn proto_size_to_runtime(size: TerminalSize) -> RuntimeTerminalSize {
    RuntimeTerminalSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn runtime_size_to_proto(size: RuntimeTerminalSize) -> TerminalSize {
    TerminalSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn runtime_state_to_proto(state: RuntimeSessionState) -> SessionState {
    match state {
        RuntimeSessionState::Created => SessionState::Created,
        RuntimeSessionState::Running => SessionState::Running,
        RuntimeSessionState::Closed => SessionState::Closed,
    }
}

fn runtime_role_to_proto(role: RuntimeAttachRole) -> AttachRole {
    match role {
        RuntimeAttachRole::Controller => AttachRole::Controller,
        RuntimeAttachRole::Viewer => AttachRole::Viewer,
    }
}

fn map_runtime_error(error: RuntimeError) -> ProtocolError {
    match error {
        RuntimeError::SessionNotFound => ProtocolError::SessionNotFound,
        RuntimeError::InputRequiresController => ProtocolError::ControllerRequired,
        RuntimeError::SessionAlreadyExists
        | RuntimeError::SessionClosed
        | RuntimeError::DeviceNotAttached
        | RuntimeError::InvalidSize
        | RuntimeError::Pty(_) => ProtocolError::RuntimeFailed,
    }
}

fn device_key(device_id: DeviceId) -> String {
    device_id.0.to_string()
}

fn nonce() -> Nonce {
    Nonce(format!("nonce-{}", ServerId::new().0))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    use termd_proto::{PairAcceptPayload, PairingToken, PublicKey, Signature};

    use super::*;
    use crate::auth::AuthSigningInput;
    use crate::net::signature::Ed25519SignatureVerifier;
    use crate::pty::{PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize};
    use crate::session::TerminalSize as RuntimeTerminalSize;

    #[derive(Clone, Default)]
    struct FakePtyBackend {
        state: Arc<Mutex<FakePtyState>>,
    }

    #[derive(Debug, Default)]
    struct FakePtyState {
        outputs: VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
        terminate_count: usize,
    }

    impl FakePtyBackend {
        fn push_output(&self, bytes: impl Into<Vec<u8>>) {
            self.state.lock().unwrap().outputs.push_back(bytes.into());
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        fn terminate_count(&self) -> usize {
            self.state.lock().unwrap().terminate_count
        }
    }

    impl PtyBackend for FakePtyBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
            }))
        }
    }

    struct FakePtySession {
        state: Arc<Mutex<FakePtyState>>,
    }

    impl PtySession for FakePtySession {
        fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
            let mut state = self.state.lock().unwrap();
            let Some(output) = state.outputs.pop_front() else {
                return Ok(0);
            };
            let read = output.len().min(buffer.len());
            buffer[..read].copy_from_slice(&output[..read]);

            if read < output.len() {
                // fake PTY 也保留短读后的剩余输出，便于测试协议层按 buffer 大小读取。
                state.outputs.push_front(output[read..].to_vec());
            }

            Ok(read)
        }

        fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
            self.state.lock().unwrap().writes.push(bytes.to_vec());
            Ok(())
        }

        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }

        fn terminate(&mut self) -> PtyResult<()> {
            self.state.lock().unwrap().terminate_count += 1;
            Ok(())
        }

        fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> PtyResult<PtyExitStatus> {
            Err(PtyError::Backend("fake wait is not used".to_owned()))
        }

        fn process_id(&self) -> Option<u32> {
            Some(7)
        }
    }

    fn protocol() -> (
        DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        FakePtyBackend,
    ) {
        let backend = FakePtyBackend::default();
        (
            DaemonProtocol::new(
                DaemonConfig::default(),
                backend.clone(),
                Ed25519SignatureVerifier,
            ),
            backend,
        )
    }

    fn wire(bytes: &[u8]) -> String {
        format!("ed25519-v1:{}", general_purpose::STANDARD.encode(bytes))
    }

    fn open_e2ee(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
    ) -> (E2eeKeyPair, E2eeSession) {
        let device_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            protocol.server_id(),
            device_id,
            protocol.e2ee_public_key(),
            device_keypair.public_key(),
        );
        let device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            protocol.e2ee_public_key(),
            context,
        )
        .unwrap();
        let handshake = envelope_value(
            MessageType::E2eeKeyExchange,
            E2eeKeyExchangePayload {
                server_id: protocol.server_id(),
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: nonce(),
                timestamp_ms: UnixTimestampMillis(1_000),
            },
        )
        .unwrap();

        let responses = connection.handle_wire_envelope(protocol, handshake);
        assert!(responses.is_empty());

        (device_keypair, device_session)
    }

    fn send_encrypted(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
        inner: JsonEnvelope,
    ) -> Vec<JsonEnvelope> {
        let frame = device_session.encrypt_json_payload(&inner).unwrap();
        let outer = envelope_value(MessageType::EncryptedFrame, frame).unwrap();

        connection.handle_wire_envelope(protocol, outer)
    }

    fn decrypt_first(
        device_session: &mut E2eeSession,
        messages: Vec<JsonEnvelope>,
    ) -> JsonEnvelope {
        let frame = encrypted_frame_from_envelope(messages.into_iter().next().unwrap()).unwrap();
        device_session.decrypt_json_payload(&frame).unwrap()
    }

    fn pair_device(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
        device_id: DeviceId,
        public_key: PublicKey,
    ) {
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_request = envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id,
                device_public_key: public_key,
                token,
                nonce: nonce(),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();

        let responses = send_encrypted(protocol, connection, device_session, pair_request);
        let response = decrypt_first(device_session, responses);
        let accepted: PairAcceptPayload = decode_payload(response.payload).unwrap();

        assert_eq!(response.kind, MessageType::PairAccept);
        assert_eq!(accepted.device_id, device_id);
        assert!(connection.is_authenticated());
    }

    fn authenticate_paired_connection(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
        signing_key: &SigningKey,
    ) -> E2eeSession {
        let device_e2ee_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            protocol.server_id(),
            device_id,
            protocol.e2ee_public_key(),
            device_e2ee_keypair.public_key(),
        );
        let mut device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_e2ee_keypair,
            protocol.e2ee_public_key(),
            context,
        )
        .unwrap();
        let handshake = envelope_value(
            MessageType::E2eeKeyExchange,
            E2eeKeyExchangePayload {
                server_id: protocol.server_id(),
                device_id,
                public_key: device_e2ee_keypair.public_key_wire(),
                nonce: nonce(),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let challenge_response = connection.handle_wire_envelope(protocol, handshake);
        let challenge_envelope = decrypt_first(&mut device_session, challenge_response);
        let challenge: AuthChallengePayload = decode_payload(challenge_envelope.payload).unwrap();
        let mut auth_payload = AuthPayload {
            device_id,
            challenge: challenge.challenge,
            nonce: nonce(),
            timestamp_ms: current_unix_timestamp_millis(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input =
            AuthSigningInput::from_payload(&auth_payload, protocol.daemon_public_identity())
                .to_bytes();
        auth_payload.signature = Signature(wire(&signing_key.sign(&signing_input).to_bytes()));

        let responses = send_encrypted(
            protocol,
            connection,
            &mut device_session,
            envelope_value(MessageType::Auth, auth_payload).unwrap(),
        );

        assert!(responses.is_empty());
        assert!(connection.is_authenticated());
        device_session
    }

    #[test]
    fn protocol_rejects_session_create_before_e2ee_or_auth() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let create = envelope_value(
            MessageType::SessionCreate,
            SessionCreatePayload {
                command: Vec::new(),
                size: TerminalSize::default(),
            },
        )
        .unwrap();

        let response = connection.handle_wire_envelope(&mut protocol, create);

        assert_eq!(response[0].kind, MessageType::Error);
        let error: ErrorPayload = decode_payload(response[0].payload.clone()).unwrap();
        assert_eq!(error.code, "invalid_state");
    }

    #[test]
    fn pair_request_must_be_inside_encrypted_frame_and_authenticates_device() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let pair_request = envelope_value(
            MessageType::PairRequest,
            PairRequestPayload {
                device_id,
                device_public_key: PublicKey(wire(signing_key.verifying_key().as_bytes())),
                token: PairingToken("plain-token-is-rejected".to_owned()),
                nonce: nonce(),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();

        let response = connection.handle_wire_envelope(&mut protocol, pair_request);
        assert_eq!(response[0].kind, MessageType::Error);

        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey(wire(signing_key.verifying_key().as_bytes())),
        );
    }

    #[test]
    fn unpaired_e2ee_connection_cannot_use_session_or_control_messages() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        let unknown_session_id = SessionId::new();

        let session_messages = vec![
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: unknown_session_id,
                },
            )
            .unwrap(),
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id: unknown_session_id,
                    data_base64: general_purpose::STANDARD.encode(b"must-not-write\n"),
                },
            )
            .unwrap(),
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: unknown_session_id,
                    size: TerminalSize::new(40, 120),
                },
            )
            .unwrap(),
            envelope_value(
                MessageType::ControlRequest,
                ControlRequestPayload {
                    session_id: unknown_session_id,
                    device_id,
                },
            )
            .unwrap(),
        ];

        for message in session_messages {
            let responses =
                send_encrypted(&mut protocol, &mut connection, &mut device_session, message);
            let error = decrypt_first(&mut device_session, responses);
            let payload: ErrorPayload = decode_payload(error.payload).unwrap();

            assert_eq!(error.kind, MessageType::Error);
            assert_eq!(payload.code, "unauthenticated");
            assert!(!payload.message.contains("must-not-write"));
        }

        assert!(backend.writes().is_empty());
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn paired_device_receives_auth_challenge_and_can_authenticate() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));

        let (mut pair_connection, _) = protocol.start_connection();
        let (_, mut pair_device_session) =
            open_e2ee(&mut protocol, &mut pair_connection, device_id);
        pair_device(
            &mut protocol,
            &mut pair_connection,
            &mut pair_device_session,
            device_id,
            public_key.clone(),
        );

        let (mut auth_connection, _) = protocol.start_connection();
        let device_e2ee_keypair = E2eeKeyPair::generate();
        let context = E2eeSessionContext::new(
            protocol.server_id(),
            device_id,
            protocol.e2ee_public_key(),
            device_e2ee_keypair.public_key(),
        );
        let mut device_session = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_e2ee_keypair,
            protocol.e2ee_public_key(),
            context,
        )
        .unwrap();
        let handshake = envelope_value(
            MessageType::E2eeKeyExchange,
            E2eeKeyExchangePayload {
                server_id: protocol.server_id(),
                device_id,
                public_key: device_e2ee_keypair.public_key_wire(),
                nonce: nonce(),
                timestamp_ms: UnixTimestampMillis(2_000),
            },
        )
        .unwrap();
        let challenge_response = auth_connection.handle_wire_envelope(&mut protocol, handshake);
        let challenge_envelope = decrypt_first(&mut device_session, challenge_response);
        let challenge: AuthChallengePayload = decode_payload(challenge_envelope.payload).unwrap();
        let mut auth_payload = AuthPayload {
            device_id,
            challenge: challenge.challenge,
            nonce: nonce(),
            timestamp_ms: current_unix_timestamp_millis(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input =
            AuthSigningInput::from_payload(&auth_payload, protocol.daemon_public_identity())
                .to_bytes();
        auth_payload.signature = Signature(wire(&signing_key.sign(&signing_input).to_bytes()));
        let responses = send_encrypted(
            &mut protocol,
            &mut auth_connection,
            &mut device_session,
            envelope_value(MessageType::Auth, auth_payload).unwrap(),
        );

        assert!(responses.is_empty());
        assert!(auth_connection.is_authenticated());
    }

    #[test]
    fn authenticated_controller_can_create_session_and_write_input() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let create = envelope_value(
            MessageType::SessionCreate,
            SessionCreatePayload {
                command: vec!["sh".to_owned()],
                size: TerminalSize::new(24, 80),
            },
        )
        .unwrap();
        let responses = send_encrypted(&mut protocol, &mut connection, &mut device_session, create);
        let created = decrypt_first(&mut device_session, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        assert_eq!(created_payload.role, AttachRole::Controller);

        let data = envelope_value(
            MessageType::SessionData,
            SessionDataPayload {
                session_id: created_payload.session_id,
                data_base64: general_purpose::STANDARD.encode(b"echo ok\n"),
            },
        )
        .unwrap();
        let responses = send_encrypted(&mut protocol, &mut connection, &mut device_session, data);

        assert!(responses.is_empty());
        assert_eq!(backend.writes(), vec![b"echo ok\n".to_vec()]);
    }

    #[test]
    fn attached_connection_receives_encrypted_session_output() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );
        let create = envelope_value(
            MessageType::SessionCreate,
            SessionCreatePayload {
                command: vec!["sh".to_owned()],
                size: TerminalSize::new(24, 80),
            },
        )
        .unwrap();
        let responses = send_encrypted(&mut protocol, &mut connection, &mut device_session, create);
        let created = decrypt_first(&mut device_session, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        backend.push_output(b"terminal secret\n".to_vec());
        let responses =
            connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].kind, MessageType::EncryptedFrame);

        let wire_json = serde_json::to_string(&responses[0]).unwrap();
        assert!(!wire_json.contains("terminal secret"));
        assert!(!wire_json.contains("session_data"));

        let inner = decrypt_first(&mut device_session, responses);
        let payload: SessionDataPayload = decode_payload(inner.payload).unwrap();
        let output = general_purpose::STANDARD
            .decode(payload.data_base64)
            .unwrap();

        assert_eq!(inner.kind, MessageType::SessionData);
        assert_eq!(payload.session_id, created_payload.session_id);
        assert_eq!(output, b"terminal secret\n");
    }

    #[test]
    fn read_attached_outputs_batches_encrypted_outputs_for_connection() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let first_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let first_created = decrypt_first(&mut device_session, first_responses);
        let first_payload: SessionCreatedPayload = decode_payload(first_created.payload).unwrap();

        let second_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let second_created = decrypt_first(&mut device_session, second_responses);
        let second_payload: SessionCreatedPayload = decode_payload(second_created.payload).unwrap();

        backend.push_output(b"first output\n".to_vec());
        backend.push_output(b"second output\n".to_vec());
        let responses = connection.read_attached_outputs(&mut protocol, 4096);

        assert_eq!(responses.len(), 2);
        for response in &responses {
            let wire_json = serde_json::to_string(response).unwrap();
            assert_eq!(response.kind, MessageType::EncryptedFrame);
            assert!(!wire_json.contains("first output"));
            assert!(!wire_json.contains("second output"));
            assert!(!wire_json.contains("session_data"));
        }

        let first_output = decrypt_first(&mut device_session, vec![responses[0].clone()]);
        let first_data: SessionDataPayload = decode_payload(first_output.payload).unwrap();
        let second_output = decrypt_first(&mut device_session, vec![responses[1].clone()]);
        let second_data: SessionDataPayload = decode_payload(second_output.payload).unwrap();

        assert_eq!(first_output.kind, MessageType::SessionData);
        assert_eq!(first_data.session_id, first_payload.session_id);
        assert_eq!(
            general_purpose::STANDARD
                .decode(first_data.data_base64)
                .unwrap(),
            b"first output\n"
        );
        assert_eq!(second_output.kind, MessageType::SessionData);
        assert_eq!(second_data.session_id, second_payload.session_id);
        assert_eq!(
            general_purpose::STANDARD
                .decode(second_data.data_base64)
                .unwrap(),
            b"second output\n"
        );
    }

    #[test]
    fn output_read_rejects_unattached_and_unknown_sessions_safely() {
        let (mut protocol, _) = protocol();
        let signing_a = SigningKey::generate(&mut OsRng);
        let signing_b = SigningKey::generate(&mut OsRng);
        let device_a = DeviceId::new();
        let device_b = DeviceId::new();

        let (mut controller, _) = protocol.start_connection();
        let (_, mut controller_crypto) = open_e2ee(&mut protocol, &mut controller, device_a);
        pair_device(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            device_a,
            PublicKey(wire(signing_a.verifying_key().as_bytes())),
        );
        let responses = send_encrypted(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut controller_crypto, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let unknown = controller.read_session_output(&mut protocol, SessionId::new(), 4096);
        let unknown_error = decrypt_first(&mut controller_crypto, unknown);
        let unknown_payload: ErrorPayload = decode_payload(unknown_error.payload).unwrap();
        assert_eq!(unknown_error.kind, MessageType::Error);
        assert_eq!(unknown_payload.code, "session_not_found");

        let (mut unattached, _) = protocol.start_connection();
        let (_, mut unattached_crypto) = open_e2ee(&mut protocol, &mut unattached, device_b);
        pair_device(
            &mut protocol,
            &mut unattached,
            &mut unattached_crypto,
            device_b,
            PublicKey(wire(signing_b.verifying_key().as_bytes())),
        );
        let response =
            unattached.read_session_output(&mut protocol, created_payload.session_id, 4096);
        let unattached_error = decrypt_first(&mut unattached_crypto, response);
        let unattached_payload: ErrorPayload = decode_payload(unattached_error.payload).unwrap();

        assert_eq!(unattached_error.kind, MessageType::Error);
        assert_eq!(unattached_payload.code, "invalid_state");
    }

    #[test]
    fn viewer_input_is_rejected_and_close_only_detaches() {
        let (mut protocol, backend) = protocol();
        let signing_a = SigningKey::generate(&mut OsRng);
        let signing_b = SigningKey::generate(&mut OsRng);
        let device_a = DeviceId::new();
        let device_b = DeviceId::new();

        let (mut controller, _) = protocol.start_connection();
        let (_, mut controller_crypto) = open_e2ee(&mut protocol, &mut controller, device_a);
        pair_device(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            device_a,
            PublicKey(wire(signing_a.verifying_key().as_bytes())),
        );
        let responses = send_encrypted(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut controller_crypto, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let (mut viewer, _) = protocol.start_connection();
        let (_, mut viewer_crypto) = open_e2ee(&mut protocol, &mut viewer, device_b);
        pair_device(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            device_b,
            PublicKey(wire(signing_b.verifying_key().as_bytes())),
        );
        let responses = send_encrypted(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut viewer_crypto, responses);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert_eq!(attached_payload.role, AttachRole::Viewer);

        let responses = send_encrypted(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id: created_payload.session_id,
                    data_base64: general_purpose::STANDARD.encode(b"blocked\n"),
                },
            )
            .unwrap(),
        );
        let error = decrypt_first(&mut viewer_crypto, responses);
        let error_payload: ErrorPayload = decode_payload(error.payload).unwrap();
        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error_payload.code, "controller_required");

        viewer.close(&mut protocol);
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn authenticated_but_unattached_same_device_cannot_operate_on_attached_session() {
        let (mut protocol, backend) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));

        let (mut controller, _) = protocol.start_connection();
        let (_, mut controller_crypto) = open_e2ee(&mut protocol, &mut controller, device_id);
        pair_device(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            device_id,
            public_key,
        );
        let responses = send_encrypted(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut controller_crypto, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let (mut second_connection, _) = protocol.start_connection();
        let mut second_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut second_connection,
            device_id,
            &signing_key,
        );
        assert_eq!(
            second_connection.state(),
            ProtocolConnectionState::Authenticated
        );

        // 同一设备可以开第二条已认证连接，但这条连接没有 attach 到该 session。
        // session 作用域操作必须绑定当前连接，不能借用另一条连接留下的 device 角色。
        let write_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id: created_payload.session_id,
                    data_base64: general_purpose::STANDARD.encode(b"must-not-write\n"),
                },
            )
            .unwrap(),
        );
        let write_error = decrypt_first(&mut second_crypto, write_responses);
        let write_payload: ErrorPayload = decode_payload(write_error.payload).unwrap();
        assert_eq!(write_error.kind, MessageType::Error);
        assert_eq!(write_payload.code, "invalid_state");
        assert!(backend.writes().is_empty());

        let resize_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: created_payload.session_id,
                    size: TerminalSize::new(40, 120),
                },
            )
            .unwrap(),
        );
        let resize_error = decrypt_first(&mut second_crypto, resize_responses);
        let resize_payload: ErrorPayload = decode_payload(resize_error.payload).unwrap();
        assert_eq!(resize_error.kind, MessageType::Error);
        assert_eq!(resize_payload.code, "invalid_state");

        let control_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::ControlRequest,
                ControlRequestPayload {
                    session_id: created_payload.session_id,
                    device_id,
                },
            )
            .unwrap(),
        );
        let control_error = decrypt_first(&mut second_crypto, control_responses);
        let control_payload: ErrorPayload = decode_payload(control_error.payload).unwrap();
        assert_eq!(control_error.kind, MessageType::Error);
        assert_eq!(control_payload.code, "invalid_state");
    }

    #[test]
    fn session_list_uses_wire_ids_not_runtime_internal_ids() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey(wire(signing_key.verifying_key().as_bytes())),
        );
        let responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut device_session, responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();

        assert_eq!(list_payload.sessions.len(), 1);
        assert_eq!(
            list_payload.sessions[0].session_id,
            created_payload.session_id
        );
    }

    #[test]
    fn runtime_size_conversion_preserves_pixels() {
        let runtime_size = RuntimeTerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 800,
            pixel_height: 600,
        };

        assert_eq!(
            runtime_size_to_proto(runtime_size),
            TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 800,
                pixel_height: 600,
            }
        );
    }
}
