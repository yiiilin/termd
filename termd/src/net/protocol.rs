//! termd daemon 的 WebSocket 协议状态机核心。
//!
//! 本模块不依赖真实 socket，便于单元测试直接驱动 hello、E2EE、pair/auth 和 session
//! 操作。Axum 只负责把网络帧转成这里的统一 envelope。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::{Engine as _, engine::general_purpose};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd_proto::{
    AttachRole, AuthChallengePayload, AuthPayload, ClientId, ControlGrantPayload,
    ControlRequestPayload, DaemonClientSummaryPayload, DaemonClientsPayload,
    DaemonClientsResultPayload, DeviceId, E2eeKeyExchangePayload, EncryptedFramePayload, Envelope,
    ErrorPayload, HelloPayload, MessageType, Nonce, PairRequestPayload, PingPayload, PongPayload,
    ProtocolVersion, ServerId, SessionAttachPayload, SessionAttachedPayload, SessionClosePayload,
    SessionClosedPayload, SessionCreatePayload, SessionCreatedPayload, SessionCursorPayload,
    SessionDataPayload, SessionFileDeletePayload, SessionFileDeletedPayload,
    SessionFileEntryPayload, SessionFileKind, SessionFileReadPayload, SessionFileReadResultPayload,
    SessionFileWritePayload, SessionFileWrittenPayload, SessionFilesPayload,
    SessionFilesResultPayload, SessionId, SessionListPayload, SessionListResultPayload,
    SessionRenamePayload, SessionRenamedPayload, SessionResizePayload, SessionState,
    SessionSummaryPayload, TerminalSize, UnixTimestampMillis,
};
use thiserror::Error;
use tokio::sync::watch;

use crate::auth::{
    AuthChallengeManager, ChallengeResponseService, DaemonIdentity, DaemonPublicIdentity,
    DeviceIdentity, InMemoryTrustedDeviceStore, PairingService, PairingTokenManager,
    ReplayProtector, SignatureVerifier, TrustedDevice, TrustedDeviceStore,
    current_unix_timestamp_millis,
};
use crate::config::DaemonConfig;
use crate::pty::{CommandSpec, PtyBackend};
use crate::runtime::{RuntimeError, SessionRuntime};
use crate::session::{
    AttachRole as RuntimeAttachRole, SessionState as RuntimeSessionState,
    TerminalSize as RuntimeTerminalSize,
};
use crate::state::{
    DaemonIdentitySnapshot, DaemonState, StateError, StateStore, TrustedDeviceState,
    client_history::{ClientHistoryRecord, ClientHistoryStore},
};

use super::{
    E2eeError, E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
};

const AUTH_CHALLENGE_TTL_MS: u64 = 60_000;
const SESSION_OUTPUT_HISTORY_MAX_BYTES: usize = 1024 * 1024;

/// 协议层统一使用的 JSON envelope。
pub type JsonEnvelope = Envelope<Value>;

/// 单个已配对客户端在当前 daemon 上的可见状态。
///
/// 这是个人使用场景里的连接清单，不是审计日志；relay 不参与生成或解释这些字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonClientRecord {
    client_id: ClientId,
    device_id: DeviceId,
    peer_ip: Option<String>,
    online: bool,
    connected_at_ms: UnixTimestampMillis,
    last_seen_at_ms: UnixTimestampMillis,
    active_connections: HashMap<ClientId, HashSet<SessionId>>,
    cursor_session_id: Option<SessionId>,
    cursor_row: Option<u16>,
    cursor_col: Option<u16>,
}

/// session 级输出缓冲。
///
/// PTY 输出只能被读取一次；这里先按 session 保留，再按每条连接自己的 offset 加密发送，
/// 避免重新 attach 或多个客户端同时 attach 时丢失已经读过的终端内容。
#[derive(Debug, Clone)]
struct SessionOutputHistory {
    base_offset: u64,
    bytes: VecDeque<u8>,
}

impl SessionOutputHistory {
    fn base_offset(&self) -> u64 {
        self.base_offset
    }

    fn end_offset(&self) -> u64 {
        self.base_offset + self.bytes.len() as u64
    }

    fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());

        while self.bytes.len() > SESSION_OUTPUT_HISTORY_MAX_BYTES {
            self.bytes.pop_front();
            self.base_offset = self.base_offset.saturating_add(1);
        }
    }

    fn read_from(&self, cursor: u64, max_bytes: usize) -> (Vec<u8>, u64) {
        let end_offset = self.end_offset();
        let start_offset = cursor.max(self.base_offset).min(end_offset);

        if max_bytes == 0 || start_offset >= end_offset {
            return (Vec::new(), start_offset);
        }

        let start_index = (start_offset - self.base_offset) as usize;
        let take = max_bytes.min(self.bytes.len() - start_index);
        let bytes = self
            .bytes
            .iter()
            .skip(start_index)
            .take(take)
            .copied()
            .collect();

        (bytes, start_offset + take as u64)
    }
}

impl Default for SessionOutputHistory {
    fn default() -> Self {
        Self {
            base_offset: 0,
            bytes: VecDeque::new(),
        }
    }
}

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
    #[error("runtime operation failed")]
    RuntimeFailed,
    #[error("daemon state persistence failed")]
    StateFailed,
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
            Self::RuntimeFailed => "runtime_failed",
            Self::StateFailed => "state_failed",
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
            Self::RuntimeFailed => "runtime operation failed",
            Self::StateFailed => "daemon state persistence failed",
        }
    }
}

impl From<E2eeError> for ProtocolError {
    fn from(_: E2eeError) -> Self {
        Self::E2eeFailed
    }
}

impl From<StateError> for ProtocolError {
    fn from(_: StateError) -> Self {
        Self::StateFailed
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
    session_names: HashMap<SessionId, String>,
    session_roots: HashMap<SessionId, PathBuf>,
    daemon_clients: HashMap<DeviceId, DaemonClientRecord>,
    client_history: ClientHistoryStore,
    session_output_history: HashMap<SessionId, SessionOutputHistory>,
}

impl<B, V> DaemonProtocol<B, V>
where
    B: PtyBackend,
    V: SignatureVerifier,
{
    /// 创建可测试的协议服务，调用方显式注入 PTY backend 和签名 verifier。
    pub fn new(config: DaemonConfig, backend: B, verifier: V) -> Result<Self, StateError> {
        let daemon_identity = DaemonIdentity::generate();
        Self::from_identity_and_store(
            config,
            backend,
            verifier,
            daemon_identity,
            InMemoryTrustedDeviceStore::new(),
        )
    }

    /// 基于本地状态文件快照创建协议服务。
    ///
    /// 快照只恢复 daemon 公开身份和可信设备；PTY session 是进程内资源，daemon 重启后不会从
    /// JSON 中伪造恢复。
    pub fn from_state(
        config: DaemonConfig,
        backend: B,
        verifier: V,
        state: DaemonState,
    ) -> Result<Self, StateError> {
        let daemon_identity = state
            .daemon_identity
            .map(|identity| {
                DaemonIdentity::from_persisted_public_identity(
                    identity.server_id,
                    identity.public_key,
                )
            })
            .unwrap_or_else(DaemonIdentity::generate);
        let trusted_store = InMemoryTrustedDeviceStore::from_trusted_devices(
            state
                .trusted_devices
                .into_iter()
                .map(trusted_device_from_state),
        );
        Self::from_identity_and_store(config, backend, verifier, daemon_identity, trusted_store)
    }

    fn from_identity_and_store(
        config: DaemonConfig,
        backend: B,
        verifier: V,
        daemon_identity: DaemonIdentity,
        trusted_store: InMemoryTrustedDeviceStore,
    ) -> Result<Self, StateError> {
        let client_history = ClientHistoryStore::open(&config.state_path)?;
        let auth_service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::default(),
        );
        Ok(Self {
            config,
            daemon_identity,
            e2ee_keypair: E2eeKeyPair::generate(),
            pairing_service: PairingService::new(PairingTokenManager::new()),
            auth_service,
            trusted_store,
            runtime: SessionRuntime::new(backend),
            verifier,
            session_index: HashMap::new(),
            session_names: HashMap::new(),
            session_roots: HashMap::new(),
            daemon_clients: HashMap::new(),
            client_history,
            session_output_history: HashMap::new(),
        })
    }

    /// 生成可写入本地 JSON 的最小状态快照。
    ///
    /// 不保存 pairing token、auth challenge、E2EE 临时密钥、PTY 输出或终端输入。
    pub fn snapshot_state(&self) -> DaemonState {
        let mut trusted_devices: Vec<_> = self
            .trusted_store
            .trusted_devices()
            .map(trusted_device_to_state)
            .collect();
        trusted_devices.sort_by_key(|device| device.device_id.0);

        DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: Some(DaemonIdentitySnapshot {
                server_id: self.daemon_identity.server_id(),
                public_key: self.daemon_identity.public_key().clone(),
            }),
            trusted_devices,
            // 运行中 PTY 进程无法通过 JSON 安全恢复；这里保持空列表，避免制造假 session。
            sessions: Vec::new(),
        }
    }

    /// 将当前最小持久状态保存到配置指定的位置。
    pub fn persist_state(&self) -> Result<(), StateError> {
        StateStore::save(&self.config.state_path, &self.snapshot_state())
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
        self.start_connection_for_peer(None)
    }

    /// 创建带来源 IP 的协议连接。
    ///
    /// `peer_ip` 只用于本地 Web UI 展示连接来源；它不参与认证、控制权或 relay 路由判断。
    pub fn start_connection_for_peer(
        &self,
        peer_ip: Option<String>,
    ) -> (ProtocolConnection, Vec<JsonEnvelope>) {
        let connection = ProtocolConnection::new(peer_ip);
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

        self.persist_state()?;
        connection.authenticated_device_id = Some(accepted.device_id);
        connection.state = ProtocolConnectionState::Authenticated;
        self.record_daemon_client_connection(connection, accepted.device_id);

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
        self.record_daemon_client_connection(connection, authenticated.device_id);
        let _ = self.persist_state();
        Ok(Vec::new())
    }

    fn create_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionCreatePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let command = command_spec_from_payload(&payload.command, &self.config)?;
        let session_root = session_root_from_command(&command)?;
        let runtime_size = proto_size_to_runtime(payload.size);
        let internal_session_id = self
            .runtime
            .create_session(command, runtime_size)
            .map_err(map_runtime_error)?;
        let wire_session_id = SessionId::new();

        self.session_index
            .insert(wire_session_id, internal_session_id.clone());
        self.session_roots.insert(wire_session_id, session_root);
        self.session_output_history
            .entry(wire_session_id)
            .or_default();

        let role = self
            .runtime
            .attach(&internal_session_id, device_key(device_id))
            .map_err(map_runtime_error)?;
        let wire_role = runtime_role_to_proto(role);
        connection.attach(
            wire_session_id,
            self.output_history_base_offset(wire_session_id),
        );
        self.record_daemon_client_attach(wire_session_id, connection, device_id);

        let response = SessionCreatedPayload {
            session_id: wire_session_id,
            role: wire_role,
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
        let wire_role = runtime_role_to_proto(role);
        connection.attach(
            payload.session_id,
            self.output_history_base_offset(payload.session_id),
        );
        self.record_daemon_client_attach(payload.session_id, connection, device_id);
        connection.state = ProtocolConnectionState::Attached;

        let response = SessionAttachedPayload {
            session_id: payload.session_id,
            role: wire_role,
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

    fn record_session_cursor(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionCursorPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        if payload.row == 0 || payload.col == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        connection.ensure_attached_to(payload.session_id)?;

        let now_ms = current_unix_timestamp_millis();
        let record = self
            .daemon_clients
            .entry(device_id)
            .or_insert_with(|| DaemonClientRecord {
                client_id: stable_client_id_for_device(device_id),
                device_id,
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections: HashMap::new(),
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
            });
        record.cursor_session_id = Some(payload.session_id);
        record.cursor_row = Some(payload.row);
        record.cursor_col = Some(payload.col);
        record.last_seen_at_ms = now_ms;
        record.online = true;

        Ok(Vec::new())
    }

    fn rename_session(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionRenamePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let name = sanitize_session_name(payload.name)?;
        self.session_names.insert(payload.session_id, name.clone());

        Ok(vec![envelope_value(
            MessageType::SessionRenamed,
            SessionRenamedPayload {
                session_id: payload.session_id,
                name,
            },
        )?])
    }

    fn close_session(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionClosePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;

        self.runtime
            .close(&internal_session_id)
            .map_err(map_runtime_error)?;
        self.session_index.remove(&payload.session_id);
        self.session_names.remove(&payload.session_id);
        self.session_roots.remove(&payload.session_id);
        self.session_output_history.remove(&payload.session_id);
        for record in self.daemon_clients.values_mut() {
            for sessions in record.active_connections.values_mut() {
                sessions.remove(&payload.session_id);
            }
        }
        if let Err(error) = self
            .client_history
            .remove_session_attachments(payload.session_id)
        {
            tracing::warn!(%error, "failed to remove closed session attachments from sqlite history");
        }

        Ok(vec![envelope_value(
            MessageType::SessionClosed,
            SessionClosedPayload {
                session_id: payload.session_id,
            },
        )?])
    }

    fn list_session_files(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFilesPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }

        let root = self
            .session_roots
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let (target, normalized_path) = resolve_session_file_target(root, payload.path)?;
        let entries = read_session_file_entries(root, &target)?;

        Ok(vec![envelope_value(
            MessageType::SessionFilesResult,
            SessionFilesResultPayload {
                session_id: payload.session_id,
                path: normalized_path,
                entries,
            },
        )?])
    }

    fn read_session_file(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileReadPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let root = self
            .session_roots
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let target = resolve_existing_session_file_target(root, &payload.path)?;
        let metadata = fs::metadata(&target).map_err(map_file_path_error)?;
        if metadata.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let bytes = fs::read(&target).map_err(map_file_path_error)?;

        Ok(vec![envelope_value(
            MessageType::SessionFileReadResult,
            SessionFileReadResultPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
                data_base64: general_purpose::STANDARD.encode(bytes),
                size_bytes: metadata.len(),
                modified_at_ms: metadata_modified_at_ms(&metadata),
            },
        )?])
    }

    fn write_session_file(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileWritePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let root = self
            .session_roots
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let target = resolve_writable_session_file_target(root, &payload.path)?;
        let bytes = general_purpose::STANDARD
            .decode(payload.data_base64)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;

        if target.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        fs::write(&target, &bytes).map_err(map_file_path_error)?;
        let metadata = fs::metadata(&target).map_err(map_file_path_error)?;

        Ok(vec![envelope_value(
            MessageType::SessionFileWritten,
            SessionFileWrittenPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
                size_bytes: metadata.len(),
                modified_at_ms: metadata_modified_at_ms(&metadata),
            },
        )?])
    }

    fn delete_session_file(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDeletePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let root = self
            .session_roots
            .get(&payload.session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let target = resolve_writable_session_file_target(root, &payload.path)?;
        let metadata = fs::symlink_metadata(&target).map_err(map_file_path_error)?;

        // 删除目录只删除空目录；递归删除风险过高，后续需要单独交互确认再扩展。
        if metadata.file_type().is_dir() {
            fs::remove_dir(&target).map_err(map_file_path_error)?;
        } else {
            fs::remove_file(&target).map_err(map_file_path_error)?;
        }

        Ok(vec![envelope_value(
            MessageType::SessionFileDeleted,
            SessionFileDeletedPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
            },
        )?])
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
                    name: self.session_names.get(wire_id).cloned(),
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

    fn list_daemon_clients(
        &mut self,
        connection: &ProtocolConnection,
        _payload: DaemonClientsPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;

        // SQLite 是持久历史，内存里的活跃连接只负责补当前在线状态和活跃 attach。
        let mut clients_by_device: HashMap<DeviceId, ClientHistoryRecord> =
            match self.client_history.list_clients() {
                Ok(records) => records
                    .into_iter()
                    .map(|record| (record.device_id, record))
                    .collect(),
                Err(error) => {
                    tracing::warn!(%error, "failed to list daemon clients from sqlite history");
                    HashMap::new()
                }
            };

        for record in self.daemon_clients.values() {
            let entry = clients_by_device
                .entry(record.device_id)
                .or_insert_with(|| ClientHistoryRecord {
                    device_id: record.device_id,
                    peer_ip: record.peer_ip.clone(),
                    online: record.online,
                    connected_at_ms: record.connected_at_ms,
                    last_seen_at_ms: record.last_seen_at_ms,
                    attached_session_ids: Vec::new(),
                });

            let mut active_session_ids: Vec<_> = record
                .active_connections
                .values()
                .flat_map(|sessions| sessions.iter().copied())
                .collect();
            active_session_ids.sort_by_key(|session_id| session_id.0);
            active_session_ids.dedup();

            if record.peer_ip.is_some() {
                entry.peer_ip = record.peer_ip.clone();
            }
            entry.online = record.online;
            if record.connected_at_ms.0 < entry.connected_at_ms.0 {
                entry.connected_at_ms = record.connected_at_ms;
            }
            if record.last_seen_at_ms.0 > entry.last_seen_at_ms.0 {
                entry.last_seen_at_ms = record.last_seen_at_ms;
            }

            if record.online {
                let mut attached_session_ids: HashSet<_> =
                    entry.attached_session_ids.iter().copied().collect();
                attached_session_ids.extend(active_session_ids);
                entry.attached_session_ids = attached_session_ids.into_iter().collect();
            } else {
                entry.attached_session_ids = active_session_ids;
            }
        }

        let mut clients: Vec<_> = clients_by_device
            .into_values()
            .map(|record| daemon_client_to_payload_from_history(record))
            .collect();
        for client in &mut clients {
            let Some(record) = self.daemon_clients.get(&client.device_id) else {
                continue;
            };
            let cursor_is_for_active_session = record
                .cursor_session_id
                .map(|session_id| client.attached_session_ids.contains(&session_id))
                .unwrap_or(false);
            if record.online && cursor_is_for_active_session {
                client.cursor_session_id = record.cursor_session_id;
                client.cursor_row = record.cursor_row;
                client.cursor_col = record.cursor_col;
            }
        }
        clients.sort_by_key(|client| client.connected_at_ms);

        Ok(vec![envelope_value(
            MessageType::DaemonClientsResult,
            DaemonClientsResultPayload { clients },
        )?])
    }

    fn record_daemon_client_connection(
        &mut self,
        connection: &ProtocolConnection,
        device_id: DeviceId,
    ) {
        let now_ms = current_unix_timestamp_millis();
        let stable_client_id = stable_client_id_for_device(device_id);

        if let Err(error) =
            self.client_history
                .record_connection(device_id, connection.peer_ip.as_deref(), now_ms)
        {
            tracing::warn!(%error, "failed to persist daemon client connection");
        }

        if let Some(record) = self.daemon_clients.get_mut(&device_id) {
            record.peer_ip = connection.peer_ip.clone();
            record.online = true;
            record.last_seen_at_ms = now_ms;
            record
                .active_connections
                .entry(connection.client_id)
                .or_default();
            return;
        }

        let mut active_connections = HashMap::new();
        active_connections.insert(connection.client_id, HashSet::new());
        self.daemon_clients.insert(
            device_id,
            DaemonClientRecord {
                client_id: stable_client_id,
                device_id,
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections,
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
            },
        );
    }

    fn record_daemon_client_attach(
        &mut self,
        session_id: SessionId,
        connection: &ProtocolConnection,
        device_id: DeviceId,
    ) {
        let now_ms = current_unix_timestamp_millis();
        if let Err(error) =
            self.client_history
                .record_attach(device_id, connection.client_id, session_id, now_ms)
        {
            tracing::warn!(%error, "failed to persist daemon client attach");
        }
        if let Some(record) = self.daemon_clients.get_mut(&device_id) {
            record
                .active_connections
                .entry(connection.client_id)
                .or_default()
                .insert(session_id);
            record.last_seen_at_ms = now_ms;
            record.online = true;
            return;
        }

        let mut active_connections = HashMap::new();
        active_connections.insert(connection.client_id, std::iter::once(session_id).collect());
        self.daemon_clients.insert(
            device_id,
            DaemonClientRecord {
                client_id: stable_client_id_for_device(device_id),
                device_id,
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections,
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
            },
        );
    }

    fn mark_daemon_client_connection_offline(
        &mut self,
        device_id: DeviceId,
        client_id: ClientId,
        now_ms: UnixTimestampMillis,
    ) {
        let persisted = match self
            .client_history
            .record_disconnect(device_id, client_id, now_ms)
        {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "failed to persist daemon client disconnect");
                false
            }
        };

        let should_remove = {
            let Some(record) = self.daemon_clients.get_mut(&device_id) else {
                return;
            };

            record.active_connections.remove(&client_id);
            record.last_seen_at_ms = now_ms;
            record.online = !record.active_connections.is_empty();
            if !record.online {
                record.cursor_session_id = None;
                record.cursor_row = None;
                record.cursor_col = None;
            }
            persisted && !record.online
        };

        if should_remove {
            self.daemon_clients.remove(&device_id);
        }
    }

    fn daemon_client_has_active_session(
        &self,
        device_id: DeviceId,
        session_id: SessionId,
        excluding_connection_id: ClientId,
    ) -> bool {
        self.daemon_clients
            .get(&device_id)
            .map(|record| {
                record
                    .active_connections
                    .iter()
                    .filter(|(client_id, _)| **client_id != excluding_connection_id)
                    .any(|(_, sessions)| sessions.contains(&session_id))
            })
            .unwrap_or(false)
    }

    fn output_history_base_offset(&mut self, session_id: SessionId) -> u64 {
        self.session_output_history
            .entry(session_id)
            .or_default()
            .base_offset()
    }

    fn drain_runtime_output_to_history(
        &mut self,
        session_id: SessionId,
        internal_session_id: &str,
        max_chunk_bytes: usize,
    ) -> Result<(), ProtocolError> {
        if max_chunk_bytes == 0 {
            return Ok(());
        }

        // 每个 session 每轮只拉一个 chunk，避免批量 flush 多个已 attach session 时，
        // 一个 session 把后续 session 的待读输出都消费掉。
        let mut buffer = vec![0_u8; max_chunk_bytes];
        let read = self
            .runtime
            .read_output(internal_session_id, &mut buffer)
            .map_err(map_runtime_error)?;
        if read == 0 {
            return Ok(());
        }

        buffer.truncate(read);
        self.session_output_history
            .entry(session_id)
            .or_default()
            .append(&buffer);
        Ok(())
    }

    fn retained_output_chunk(
        &self,
        session_id: SessionId,
        cursor: u64,
        max_chunk_bytes: usize,
    ) -> (Vec<u8>, u64) {
        self.session_output_history
            .get(&session_id)
            .map(|history| history.read_from(cursor, max_chunk_bytes))
            .unwrap_or_else(|| (Vec::new(), cursor))
    }

    fn detach_connection(&mut self, connection: &mut ProtocolConnection) {
        let Some(device_id) = connection.authenticated_device_id else {
            connection.state = ProtocolConnectionState::Closed;
            return;
        };
        let device_key = device_key(device_id);
        let now_ms = current_unix_timestamp_millis();
        let attached_sessions = std::mem::take(&mut connection.attached_sessions);
        let remaining_sessions: HashSet<_> = attached_sessions
            .iter()
            .copied()
            .filter(|session_id| {
                self.daemon_client_has_active_session(device_id, *session_id, connection.client_id)
            })
            .collect();
        connection.output_offsets.clear();
        self.mark_daemon_client_connection_offline(device_id, connection.client_id, now_ms);

        // 断开 WebSocket 只 detach 当前连接关联的 session，不 close/terminate PTY。
        // 同一浏览器/设备如果还有另一条 attach 连接在线，不能撤掉设备级 operator 角色。
        for wire_session_id in attached_sessions {
            if remaining_sessions.contains(&wire_session_id) {
                continue;
            }
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

    fn output_signal(
        &self,
        session_id: SessionId,
    ) -> Result<Option<watch::Receiver<u64>>, ProtocolError> {
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .output_signal(internal_session_id)
            .map_err(map_runtime_error)
    }
}

/// Web UI 里的“客户端”是已配对浏览器/设备，不是每次 attach 新建的 WebSocket。
fn stable_client_id_for_device(device_id: DeviceId) -> ClientId {
    ClientId(device_id.0)
}

/// 单条 WebSocket 连接的状态。E2EE session 只属于当前连接。
pub struct ProtocolConnection {
    client_id: ClientId,
    peer_ip: Option<String>,
    state: ProtocolConnectionState,
    device_id: Option<DeviceId>,
    authenticated_device_id: Option<DeviceId>,
    e2ee: Option<E2eeSession>,
    attached_sessions: Vec<SessionId>,
    output_offsets: HashMap<SessionId, u64>,
}

impl ProtocolConnection {
    fn new(peer_ip: Option<String>) -> Self {
        Self {
            client_id: ClientId::new(),
            peer_ip,
            state: ProtocolConnectionState::Init,
            device_id: None,
            authenticated_device_id: None,
            e2ee: None,
            attached_sessions: Vec::new(),
            output_offsets: HashMap::new(),
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
    /// server 层在 attach/create 后调用本方法，把 watcher 注册前已经缓存的 PTY 输出立即发走；
    /// 后续持续输出由 `attached_output_signals` 驱动的 WebSocket 推送路径负责。
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

    /// 返回当前连接已 attach session 的输出信号，供 WebSocket 层注册主动推送 watcher。
    pub fn attached_output_signals<B, V>(
        &self,
        protocol: &DaemonProtocol<B, V>,
    ) -> Vec<(SessionId, watch::Receiver<u64>)>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.attached_sessions
            .iter()
            .filter_map(|session_id| {
                protocol
                    .output_signal(*session_id)
                    .ok()
                    .flatten()
                    .map(|signal| (*session_id, signal))
            })
            .collect()
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

        protocol.drain_runtime_output_to_history(session_id, &internal_session_id, max_bytes)?;

        let mut chunks = Vec::new();
        loop {
            let cursor = self
                .output_offsets
                .get(&session_id)
                .copied()
                .unwrap_or_else(|| protocol.output_history_base_offset(session_id));
            let (bytes, next_cursor) =
                protocol.retained_output_chunk(session_id, cursor, max_bytes);
            self.output_offsets.insert(session_id, next_cursor);
            if bytes.is_empty() {
                break;
            }
            chunks.push(bytes);
        }

        let mut messages = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            messages.push(envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id,
                    data_base64: general_purpose::STANDARD.encode(chunk),
                },
            )?);
        }

        self.encrypt_inner_messages(messages)
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
            MessageType::SessionCursor => {
                let payload = decode_payload(envelope.payload)?;
                protocol.record_session_cursor(self, payload)
            }
            MessageType::SessionResize => {
                let payload = decode_payload(envelope.payload)?;
                protocol.resize_session(self, payload)
            }
            MessageType::SessionRename => {
                let payload = decode_payload(envelope.payload)?;
                protocol.rename_session(self, payload)
            }
            MessageType::SessionClose => {
                let payload = decode_payload(envelope.payload)?;
                protocol.close_session(self, payload)
            }
            MessageType::SessionFiles => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_session_files(self, payload)
            }
            MessageType::SessionFileRead => {
                let payload = decode_payload(envelope.payload)?;
                protocol.read_session_file(self, payload)
            }
            MessageType::SessionFileWrite => {
                let payload = decode_payload(envelope.payload)?;
                protocol.write_session_file(self, payload)
            }
            MessageType::SessionFileDelete => {
                let payload = decode_payload(envelope.payload)?;
                protocol.delete_session_file(self, payload)
            }
            MessageType::ControlRequest => {
                let payload = decode_payload(envelope.payload)?;
                protocol.request_control(self, payload)
            }
            MessageType::SessionList => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_sessions(self, payload)
            }
            MessageType::DaemonClients => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_daemon_clients(self, payload)
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
        // 这样同一设备新开的第二条连接不能借用旧连接在 runtime 中留下的 operator 角色。
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

    fn attach(&mut self, session_id: SessionId, output_base_offset: u64) {
        if !self.attached_sessions.contains(&session_id) {
            self.attached_sessions.push(session_id);
            self.output_offsets.insert(session_id, output_base_offset);
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

    let mut command = CommandSpec::new(program).args(argv);
    if let Some(cwd) = &config.default_working_directory {
        command = command.cwd(cwd.clone());
    }

    Ok(command)
}

fn session_root_from_command(command: &CommandSpec) -> Result<PathBuf, ProtocolError> {
    let root = match command.cwd_path() {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|_| ProtocolError::RuntimeFailed)?,
    };

    root.canonicalize()
        .map_err(|_| ProtocolError::RuntimeFailed)
}

fn resolve_session_file_target(
    root: &Path,
    requested_path: Option<String>,
) -> Result<(PathBuf, String), ProtocolError> {
    let target = match requested_path {
        Some(path) if !path.trim().is_empty() => resolve_existing_session_file_target(root, &path)?,
        _ => root.to_path_buf(),
    };
    let normalized_path = absolute_path_string(&target);
    Ok((target, normalized_path))
}

fn resolve_existing_session_file_target(
    root: &Path,
    requested_path: &str,
) -> Result<PathBuf, ProtocolError> {
    let raw = requested_path.trim();
    if raw.is_empty() {
        return Ok(root.to_path_buf());
    }

    let requested = Path::new(raw);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    candidate.canonicalize().map_err(map_file_path_error)
}

fn resolve_writable_session_file_target(
    root: &Path,
    requested_path: &str,
) -> Result<PathBuf, ProtocolError> {
    let raw = requested_path.trim();
    if raw.is_empty() {
        return Err(ProtocolError::InvalidEnvelope);
    }

    let requested = Path::new(raw);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let Some(file_name) = candidate.file_name() else {
        return Err(ProtocolError::InvalidEnvelope);
    };
    let Some(parent) = candidate.parent() else {
        return Err(ProtocolError::InvalidEnvelope);
    };
    let parent = parent.canonicalize().map_err(map_file_path_error)?;

    Ok(parent.join(file_name))
}

fn read_session_file_entries(
    _root: &Path,
    target: &Path,
) -> Result<Vec<SessionFileEntryPayload>, ProtocolError> {
    let metadata = fs::metadata(target).map_err(map_file_path_error)?;
    if !metadata.is_dir() {
        return Err(ProtocolError::InvalidEnvelope);
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(target).map_err(map_file_path_error)? {
        let entry = entry.map_err(|_| ProtocolError::RuntimeFailed)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| ProtocolError::RuntimeFailed)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }

        entries.push(SessionFileEntryPayload {
            name,
            path: absolute_path_string(&path),
            kind: session_file_kind(&metadata),
            size_bytes: metadata.len(),
            modified_at_ms: metadata_modified_at_ms(&metadata),
        });
    }

    // 文件 panel 面向人工浏览：目录优先，其余按名称稳定排序，避免每次刷新跳动。
    entries.sort_by(|left, right| {
        session_file_kind_rank(left.kind)
            .cmp(&session_file_kind_rank(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(entries)
}

fn map_file_path_error(error: std::io::Error) -> ProtocolError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return ProtocolError::InvalidEnvelope;
    }

    ProtocolError::RuntimeFailed
}

fn session_file_kind(metadata: &fs::Metadata) -> SessionFileKind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        SessionFileKind::Directory
    } else if file_type.is_file() {
        SessionFileKind::File
    } else if file_type.is_symlink() {
        SessionFileKind::Symlink
    } else {
        SessionFileKind::Other
    }
}

fn session_file_kind_rank(kind: SessionFileKind) -> u8 {
    match kind {
        SessionFileKind::Directory => 0,
        SessionFileKind::File => 1,
        SessionFileKind::Symlink => 2,
        SessionFileKind::Other => 3,
    }
}

fn metadata_modified_at_ms(metadata: &fs::Metadata) -> Option<UnixTimestampMillis> {
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let millis = duration.as_millis().min(u128::from(u64::MAX)) as u64;
    Some(UnixTimestampMillis(millis))
}

fn absolute_path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn sanitize_session_name(raw_name: String) -> Result<String, ProtocolError> {
    let name = raw_name.trim();
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(ProtocolError::InvalidEnvelope);
    }
    Ok(name.to_owned())
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
        RuntimeAttachRole::Operator => AttachRole::Operator,
    }
}

fn daemon_client_to_payload_from_history(
    mut record: ClientHistoryRecord,
) -> DaemonClientSummaryPayload {
    let mut attached_session_ids = std::mem::take(&mut record.attached_session_ids);
    attached_session_ids.sort_by_key(|session_id| session_id.0);
    attached_session_ids.dedup();

    DaemonClientSummaryPayload {
        client_id: stable_client_id_for_device(record.device_id),
        device_id: record.device_id,
        peer_ip: record.peer_ip,
        online: record.online,
        connected_at_ms: record.connected_at_ms,
        last_seen_at_ms: record.last_seen_at_ms,
        attached_session_ids,
        cursor_session_id: None,
        cursor_row: None,
        cursor_col: None,
    }
}

fn trusted_device_from_state(state: TrustedDeviceState) -> TrustedDevice {
    TrustedDevice::restore(
        DeviceIdentity::new(state.device_id, state.public_key),
        state.trusted_at_ms,
        state.last_seen_at_ms,
        state.label,
    )
}

fn trusted_device_to_state(device: &TrustedDevice) -> TrustedDeviceState {
    TrustedDeviceState {
        device_id: device.device_id(),
        public_key: device.public_key().clone(),
        trusted_at_ms: device.trusted_at_ms(),
        last_seen_at_ms: device.last_seen_at_ms(),
        label: device.label().map(ToOwned::to_owned),
    }
}

fn map_runtime_error(error: RuntimeError) -> ProtocolError {
    match error {
        RuntimeError::SessionNotFound => ProtocolError::SessionNotFound,
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    use termd_proto::{
        PairAcceptPayload, PairingToken, PublicKey, SessionFileDeletePayload,
        SessionFileDeletedPayload, SessionFileKind, SessionFileReadPayload,
        SessionFileReadResultPayload, SessionFileWritePayload, SessionFileWrittenPayload,
        SessionFilesPayload, SessionFilesResultPayload, Signature,
    };

    use super::*;
    use crate::auth::AuthSigningInput;
    use crate::net::signature::Ed25519SignatureVerifier;
    use crate::pty::{PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize};
    use crate::session::TerminalSize as RuntimeTerminalSize;
    use crate::state::StateStore;

    static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let config = DaemonConfig::default_for_state_path(temp_state_path("protocol.json"));
        (
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap(),
            backend,
        )
    }

    fn temp_state_path(name: &str) -> std::path::PathBuf {
        let counter = TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "termd-protocol-test-{}-{}-{}-{name}",
            std::process::id(),
            current_unix_timestamp_millis().0,
            counter
        ))
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
    fn paired_device_can_authenticate_after_protocol_state_reload() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("paired-device.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let mut protocol =
            DaemonProtocol::new(config.clone(), backend.clone(), Ed25519SignatureVerifier).unwrap();
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
            public_key,
        );
        let server_id = protocol.server_id();
        let daemon_public_key = protocol.daemon_public_identity().public_key.clone();
        StateStore::save(&state_path, &protocol.snapshot_state()).unwrap();

        let restored = StateStore::load(&state_path).unwrap();
        let mut restarted =
            DaemonProtocol::from_state(config, backend, Ed25519SignatureVerifier, restored)
                .unwrap();
        let (mut auth_connection, _) = restarted.start_connection();

        assert_eq!(restarted.server_id(), server_id);
        assert_eq!(
            restarted.daemon_public_identity().public_key,
            daemon_public_key
        );
        authenticate_paired_connection(
            &mut restarted,
            &mut auth_connection,
            device_id,
            &signing_key,
        );

        std::fs::remove_file(state_path).ok();
    }

    #[test]
    fn daemon_clients_list_keeps_offline_connection_history() {
        let (mut protocol, _) = protocol();
        let (mut controller, _) = protocol.start_connection_for_peer(Some("192.0.2.10".to_owned()));
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut controller_crypto) = open_e2ee(&mut protocol, &mut controller, device_id);
        pair_device(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            device_id,
            public_key,
        );
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut controller_crypto, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let list_responses = send_encrypted(
            &mut protocol,
            &mut controller,
            &mut controller_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut controller_crypto, list_responses);
        let list_payload: DaemonClientsResultPayload = decode_payload(list.payload).unwrap();

        assert_eq!(list.kind, MessageType::DaemonClientsResult);
        assert_eq!(list_payload.clients.len(), 1);
        assert_eq!(list_payload.clients[0].device_id, device_id);
        assert_eq!(
            list_payload.clients[0].peer_ip.as_deref(),
            Some("192.0.2.10")
        );
        assert_eq!(
            list_payload.clients[0].attached_session_ids,
            vec![created_payload.session_id]
        );
        assert!(list_payload.clients[0].online);

        controller.close(&mut protocol);
        let (mut inspector, _) = protocol.start_connection();
        let inspector_device_id = DeviceId::new();
        let inspector_signing_key = SigningKey::generate(&mut OsRng);
        let inspector_public_key =
            PublicKey(wire(inspector_signing_key.verifying_key().as_bytes()));
        let (_, mut inspector_crypto) =
            open_e2ee(&mut protocol, &mut inspector, inspector_device_id);
        pair_device(
            &mut protocol,
            &mut inspector,
            &mut inspector_crypto,
            inspector_device_id,
            inspector_public_key,
        );
        let offline_responses = send_encrypted(
            &mut protocol,
            &mut inspector,
            &mut inspector_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let offline_list = decrypt_first(&mut inspector_crypto, offline_responses);
        let offline_payload: DaemonClientsResultPayload =
            decode_payload(offline_list.payload).unwrap();
        let controller_client = offline_payload
            .clients
            .iter()
            .find(|client| client.device_id == device_id)
            .expect("controller device should stay in daemon client history");

        assert_eq!(
            controller_client.client_id,
            list_payload.clients[0].client_id
        );
        assert_eq!(
            controller_client.last_seen_at_ms.0 >= list_payload.clients[0].connected_at_ms.0,
            true
        );
        assert!(!controller_client.online);
        assert!(controller_client.attached_session_ids.is_empty());
    }

    #[test]
    fn same_device_reconnect_updates_one_daemon_client_record() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));

        let (mut first_connection, _) =
            protocol.start_connection_for_peer(Some("192.0.2.10".to_owned()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let created_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_crypto, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let first_list = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let first_list = decrypt_first(&mut first_crypto, first_list);
        let first_payload: DaemonClientsResultPayload = decode_payload(first_list.payload).unwrap();
        assert_eq!(first_payload.clients.len(), 1);
        let stable_client_id = first_payload.clients[0].client_id;

        first_connection.close(&mut protocol);

        let (mut second_connection, _) =
            protocol.start_connection_for_peer(Some("192.0.2.10".to_owned()));
        let mut second_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut second_connection,
            device_id,
            &signing_key,
        );
        let attach_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_crypto, attach_responses);
        assert_eq!(attached.kind, MessageType::SessionAttached);

        let second_list = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let second_list = decrypt_first(&mut second_crypto, second_list);
        let second_payload: DaemonClientsResultPayload =
            decode_payload(second_list.payload).unwrap();

        assert_eq!(second_payload.clients.len(), 1);
        assert_eq!(second_payload.clients[0].client_id, stable_client_id);
        assert_eq!(second_payload.clients[0].device_id, device_id);
        assert!(second_payload.clients[0].online);
        assert_eq!(
            second_payload.clients[0].attached_session_ids,
            vec![created_payload.session_id]
        );
    }

    #[test]
    fn daemon_client_list_includes_attached_operator_cursor() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection_for_peer(Some("192.0.2.44".to_owned()));
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

        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let cursor_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCursor,
                SessionCursorPayload {
                    session_id: created_payload.session_id,
                    row: 12,
                    col: 8,
                },
            )
            .unwrap(),
        );
        assert!(cursor_responses.is_empty());

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let payload: DaemonClientsResultPayload = decode_payload(list.payload).unwrap();

        assert_eq!(payload.clients.len(), 1);
        assert_eq!(
            payload.clients[0].cursor_session_id,
            Some(created_payload.session_id)
        );
        assert_eq!(payload.clients[0].cursor_row, Some(12));
        assert_eq!(payload.clients[0].cursor_col, Some(8));
    }

    #[test]
    fn restored_trusted_devices_are_listed_as_offline_daemon_clients() {
        let historical_device_id = DeviceId::new();
        let historical_signing_key = SigningKey::generate(&mut OsRng);
        let inspector_device_id = DeviceId::new();
        let inspector_signing_key = SigningKey::generate(&mut OsRng);
        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: vec![
                TrustedDeviceState {
                    device_id: historical_device_id,
                    public_key: PublicKey(wire(historical_signing_key.verifying_key().as_bytes())),
                    trusted_at_ms: UnixTimestampMillis(1_710_000_000_000),
                    last_seen_at_ms: Some(UnixTimestampMillis(1_710_000_030_000)),
                    label: None,
                },
                TrustedDeviceState {
                    device_id: inspector_device_id,
                    public_key: PublicKey(wire(inspector_signing_key.verifying_key().as_bytes())),
                    trusted_at_ms: UnixTimestampMillis(1_710_000_010_000),
                    last_seen_at_ms: Some(UnixTimestampMillis(1_710_000_040_000)),
                    label: None,
                },
            ],
            sessions: Vec::new(),
        };
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restored-clients.json");
        let config = DaemonConfig::default_for_state_path(&state_path);

        {
            let mut bootstrap_protocol = DaemonProtocol::from_state(
                config.clone(),
                backend.clone(),
                Ed25519SignatureVerifier,
                state.clone(),
            )
            .unwrap();
            let (mut historical_connection, _) =
                bootstrap_protocol.start_connection_for_peer(Some("192.0.2.10".to_owned()));
            let historical_crypto = authenticate_paired_connection(
                &mut bootstrap_protocol,
                &mut historical_connection,
                historical_device_id,
                &historical_signing_key,
            );
            drop(historical_crypto);
            historical_connection.close(&mut bootstrap_protocol);
        }

        let mut protocol =
            DaemonProtocol::from_state(config, backend, Ed25519SignatureVerifier, state).unwrap();
        let (mut inspector, _) =
            protocol.start_connection_for_peer(Some("198.51.100.44".to_owned()));
        let mut inspector_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut inspector,
            inspector_device_id,
            &inspector_signing_key,
        );

        let responses = send_encrypted(
            &mut protocol,
            &mut inspector,
            &mut inspector_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let response = decrypt_first(&mut inspector_crypto, responses);
        let payload: DaemonClientsResultPayload = decode_payload(response.payload).unwrap();
        let historical_client = payload
            .clients
            .iter()
            .find(|client| client.device_id == historical_device_id)
            .expect("restored trusted device should remain visible in daemon client list");
        let inspector_client = payload
            .clients
            .iter()
            .find(|client| client.device_id == inspector_device_id)
            .expect("authenticated inspector should be visible in daemon client list");

        assert_eq!(payload.clients.len(), 2);
        assert_eq!(
            historical_client.client_id,
            stable_client_id_for_device(historical_device_id)
        );
        assert_eq!(historical_client.peer_ip.as_deref(), Some("192.0.2.10"));
        assert!(!historical_client.online);
        assert!(historical_client.attached_session_ids.is_empty());
        assert_eq!(inspector_client.peer_ip.as_deref(), Some("198.51.100.44"));
        assert!(inspector_client.online);
    }

    #[test]
    fn reattached_connection_replays_retained_session_output() {
        let (mut protocol, backend) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));

        let (mut first_connection, _) = protocol.start_connection();
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let created_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_crypto, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        backend.push_output(b"original screen\n".to_vec());
        let first_output =
            first_connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        let first_output = decrypt_first(&mut first_crypto, first_output);
        let first_data: SessionDataPayload = decode_payload(first_output.payload).unwrap();
        assert_eq!(
            general_purpose::STANDARD
                .decode(first_data.data_base64)
                .unwrap(),
            b"original screen\n"
        );

        first_connection.close(&mut protocol);

        let (mut second_connection, _) = protocol.start_connection();
        let mut second_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut second_connection,
            device_id,
            &signing_key,
        );
        let attach_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_crypto, attach_responses);
        assert_eq!(attached.kind, MessageType::SessionAttached);

        let replayed_output =
            second_connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        let replayed_output = decrypt_first(&mut second_crypto, replayed_output);
        let replayed_data: SessionDataPayload = decode_payload(replayed_output.payload).unwrap();

        assert_eq!(replayed_output.kind, MessageType::SessionData);
        assert_eq!(replayed_data.session_id, created_payload.session_id);
        assert_eq!(
            general_purpose::STANDARD
                .decode(replayed_data.data_base64)
                .unwrap(),
            b"original screen\n"
        );
    }

    #[test]
    fn authenticated_operator_can_create_session_and_write_input() {
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
        assert_eq!(created_payload.role, AttachRole::Operator);

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
    fn attached_session_can_list_files_from_session_root() {
        let root = temp_state_path("files-root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("alpha.txt"), b"hello world!").unwrap();
        fs::write(root.join("src").join("main.rs"), b"fn main() {}\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path("files-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
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
        let create_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut device_session, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let (mut file_connection, _) = protocol.start_connection();
        let mut file_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut file_connection,
            device_id,
            &signing_key,
        );

        let list_responses = send_encrypted(
            &mut protocol,
            &mut file_connection,
            &mut file_crypto,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let listed = decrypt_first(&mut file_crypto, list_responses);
        let payload: SessionFilesResultPayload = decode_payload(listed.payload).unwrap();
        let entries: Vec<_> = payload
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.path.clone(), entry.kind))
            .collect();

        assert_eq!(listed.kind, MessageType::SessionFilesResult);
        assert_eq!(payload.session_id, created_payload.session_id);
        assert_eq!(payload.path, root.to_string_lossy());
        assert_eq!(
            entries,
            vec![
                (
                    "src",
                    root.join("src").to_string_lossy().to_string(),
                    SessionFileKind::Directory,
                ),
                (
                    "alpha.txt",
                    root.join("alpha.txt").to_string_lossy().to_string(),
                    SessionFileKind::File,
                ),
            ]
        );
        assert_eq!(
            payload
                .entries
                .iter()
                .find(|entry| entry.name == "alpha.txt")
                .unwrap()
                .size_bytes,
            12
        );
        assert!(backend.writes().is_empty());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn session_file_transfer_can_navigate_parent_read_write_and_delete() {
        let base = temp_state_path("files-base");
        let root = base.join("project");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("readme.txt"), b"outside file\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path("files-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
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
        let create_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut device_session, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let (mut file_connection, _) = protocol.start_connection();
        let mut file_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut file_connection,
            device_id,
            &signing_key,
        );

        let list_responses = send_encrypted(
            &mut protocol,
            &mut file_connection,
            &mut file_crypto,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: Some("../outside".to_owned()),
                },
            )
            .unwrap(),
        );
        let listed = decrypt_first(&mut file_crypto, list_responses);
        let listed_payload: SessionFilesResultPayload = decode_payload(listed.payload).unwrap();

        assert_eq!(listed.kind, MessageType::SessionFilesResult);
        assert_eq!(listed_payload.path, outside.to_string_lossy());
        assert_eq!(listed_payload.entries[0].name, "readme.txt");

        let read_responses = send_encrypted(
            &mut protocol,
            &mut file_connection,
            &mut file_crypto,
            envelope_value(
                MessageType::SessionFileRead,
                SessionFileReadPayload {
                    session_id: created_payload.session_id,
                    path: outside.join("readme.txt").to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let read = decrypt_first(&mut file_crypto, read_responses);
        let read_payload: SessionFileReadResultPayload = decode_payload(read.payload).unwrap();
        assert_eq!(read.kind, MessageType::SessionFileReadResult);
        assert_eq!(
            general_purpose::STANDARD
                .decode(read_payload.data_base64)
                .unwrap(),
            b"outside file\n"
        );

        let upload_path = root.join("upload.txt");
        let write_responses = send_encrypted(
            &mut protocol,
            &mut file_connection,
            &mut file_crypto,
            envelope_value(
                MessageType::SessionFileWrite,
                SessionFileWritePayload {
                    session_id: created_payload.session_id,
                    path: upload_path.to_string_lossy().to_string(),
                    data_base64: general_purpose::STANDARD.encode(b"uploaded\n"),
                },
            )
            .unwrap(),
        );
        let written = decrypt_first(&mut file_crypto, write_responses);
        let written_payload: SessionFileWrittenPayload = decode_payload(written.payload).unwrap();
        assert_eq!(written.kind, MessageType::SessionFileWritten);
        assert_eq!(written_payload.size_bytes, 9);
        assert_eq!(fs::read(&upload_path).unwrap(), b"uploaded\n");

        let delete_responses = send_encrypted(
            &mut protocol,
            &mut file_connection,
            &mut file_crypto,
            envelope_value(
                MessageType::SessionFileDelete,
                SessionFileDeletePayload {
                    session_id: created_payload.session_id,
                    path: upload_path.to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let deleted = decrypt_first(&mut file_crypto, delete_responses);
        let deleted_payload: SessionFileDeletedPayload = decode_payload(deleted.payload).unwrap();

        assert_eq!(deleted.kind, MessageType::SessionFileDeleted);
        assert_eq!(deleted_payload.path, upload_path.to_string_lossy());
        assert!(!upload_path.exists());
        assert!(backend.writes().is_empty());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn reselecting_session_keeps_operator_input_enabled() {
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
        let create_responses =
            send_encrypted(&mut protocol, &mut connection, &mut device_session, create);
        let created = decrypt_first(&mut device_session, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        assert_eq!(created_payload.role, AttachRole::Operator);

        let attach = envelope_value(
            MessageType::SessionAttach,
            SessionAttachPayload {
                session_id: created_payload.session_id,
            },
        )
        .unwrap();
        let attach_responses =
            send_encrypted(&mut protocol, &mut connection, &mut device_session, attach);
        let attached = decrypt_first(&mut device_session, attach_responses);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert_eq!(attached.kind, MessageType::SessionAttached);
        assert_eq!(attached_payload.role, AttachRole::Operator);

        let shared_input = envelope_value(
            MessageType::SessionData,
            SessionDataPayload {
                session_id: created_payload.session_id,
                data_base64: general_purpose::STANDARD.encode(b"shared-reselect\n"),
            },
        )
        .unwrap();
        let input_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            shared_input,
        );
        assert!(input_responses.is_empty());
        assert_eq!(backend.writes(), vec![b"shared-reselect\n".to_vec()]);
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
    fn additional_operator_input_is_accepted_and_close_only_detaches() {
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
        assert_eq!(attached_payload.role, AttachRole::Operator);

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
        assert!(responses.is_empty());
        assert_eq!(backend.writes(), vec![b"blocked\n".to_vec()]);

        viewer.close(&mut protocol);
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn reattached_operator_can_write_and_control_request_is_noop() {
        let (mut protocol, backend) = protocol();
        let signing_a = SigningKey::generate(&mut OsRng);
        let signing_b = SigningKey::generate(&mut OsRng);
        let device_a = DeviceId::new();
        let device_b = DeviceId::new();

        let (mut creator, _) = protocol.start_connection();
        let (_, mut creator_crypto) = open_e2ee(&mut protocol, &mut creator, device_a);
        pair_device(
            &mut protocol,
            &mut creator,
            &mut creator_crypto,
            device_a,
            PublicKey(wire(signing_a.verifying_key().as_bytes())),
        );
        let created_responses = send_encrypted(
            &mut protocol,
            &mut creator,
            &mut creator_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut creator_crypto, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        creator.close(&mut protocol);

        let (mut viewer, _) = protocol.start_connection();
        let (_, mut viewer_crypto) = open_e2ee(&mut protocol, &mut viewer, device_b);
        pair_device(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            device_b,
            PublicKey(wire(signing_b.verifying_key().as_bytes())),
        );
        let attach_responses = send_encrypted(
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
        let attached = decrypt_first(&mut viewer_crypto, attach_responses);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert_eq!(attached_payload.role, AttachRole::Operator);

        let input_responses = send_encrypted(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id: created_payload.session_id,
                    data_base64: general_purpose::STANDARD.encode(b"should-not-write\n"),
                },
            )
            .unwrap(),
        );
        assert!(input_responses.is_empty());
        assert_eq!(backend.writes(), vec![b"should-not-write\n".to_vec()]);

        let grant_responses = send_encrypted(
            &mut protocol,
            &mut viewer,
            &mut viewer_crypto,
            envelope_value(
                MessageType::ControlRequest,
                ControlRequestPayload {
                    session_id: created_payload.session_id,
                    device_id: device_b,
                },
            )
            .unwrap(),
        );
        let grant = decrypt_first(&mut viewer_crypto, grant_responses);
        assert_eq!(grant.kind, MessageType::ControlGrant);
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
    fn session_can_be_renamed_and_closed_over_protocol() {
        let (mut protocol, backend) = protocol();
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let rename_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionRename,
                SessionRenamePayload {
                    session_id: created_payload.session_id,
                    name: "  work shell  ".to_owned(),
                },
            )
            .unwrap(),
        );
        let renamed = decrypt_first(&mut device_session, rename_responses);
        let renamed_payload: SessionRenamedPayload = decode_payload(renamed.payload).unwrap();
        assert_eq!(renamed.kind, MessageType::SessionRenamed);
        assert_eq!(renamed_payload.name, "work shell");

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();
        assert_eq!(list_payload.sessions[0].name.as_deref(), Some("work shell"));

        let close_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionClose,
                SessionClosePayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let closed = decrypt_first(&mut device_session, close_responses);
        let closed_payload: SessionClosedPayload = decode_payload(closed.payload).unwrap();
        assert_eq!(closed.kind, MessageType::SessionClosed);
        assert_eq!(closed_payload.session_id, created_payload.session_id);
        assert_eq!(backend.terminate_count(), 1);

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();
        assert!(list_payload.sessions.is_empty());
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

    #[test]
    fn command_spec_uses_configured_default_working_directory() {
        let mut config = DaemonConfig::default_for_state_path(temp_state_path("cwd.json"));
        config.default_command = vec!["/bin/bash".to_owned()];
        config.default_working_directory = Some(std::path::PathBuf::from("/home/termd-user"));

        let default_command = command_spec_from_payload(&[], &config).unwrap();
        let requested_command =
            command_spec_from_payload(&["/usr/bin/env".to_owned()], &config).unwrap();

        assert_eq!(default_command.program(), "/bin/bash");
        assert_eq!(
            default_command.cwd_path(),
            Some(std::path::Path::new("/home/termd-user"))
        );
        assert_eq!(
            requested_command.cwd_path(),
            Some(std::path::Path::new("/home/termd-user"))
        );
    }
}
