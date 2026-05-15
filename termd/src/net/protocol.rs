//! termd daemon 的 WebSocket 协议状态机核心。
//!
//! 本模块不依赖真实 socket，便于单元测试直接驱动 hello、E2EE、pair/auth 和 session
//! 操作。Axum 只负责把网络帧转成这里的统一 envelope。

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::{
    Engine as _,
    engine::general_purpose::{self, URL_SAFE_NO_PAD},
};
use rand_core::{OsRng, RngCore};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd_proto::{
    AttachRole, AuthChallengePayload, AuthPayload, ClientHelloPayload, ClientId,
    ControlGrantPayload, ControlRequestPayload, DaemonClientForgetPayload,
    DaemonClientForgotPayload, DaemonClientSummaryPayload, DaemonClientsPayload,
    DaemonClientsResultPayload, DaemonStatusPayload, DaemonStatusResultPayload, DeviceId,
    E2eeKeyExchangePayload, EncryptedFramePayload, Envelope, ErrorPayload, HelloPayload,
    MessageType, Nonce, PairRequestPayload, PingPayload, PongPayload, ProtocolVersion, ServerId,
    SessionActivityPayload, SessionAttachPayload, SessionAttachedPayload, SessionClosePayload,
    SessionClosedPayload, SessionCreatePayload, SessionCreatedPayload, SessionCursorPayload,
    SessionDataPayload, SessionFileDeletePayload, SessionFileDeletedPayload,
    SessionFileDownloadChunkPayload, SessionFileDownloadChunkResultPayload,
    SessionFileDownloadPreparePayload, SessionFileDownloadReadyPayload, SessionFileEntryPayload,
    SessionFileKind, SessionFileReadPayload, SessionFileReadResultPayload, SessionFileWritePayload,
    SessionFileWrittenPayload, SessionFilesPayload, SessionFilesResultPayload, SessionId,
    SessionListPayload, SessionListResultPayload, SessionRenamePayload, SessionRenamedPayload,
    SessionReorderPayload, SessionReorderedPayload, SessionResizePayload, SessionResizedPayload,
    SessionState, SessionSummaryPayload, TerminalSize, UnixTimestampMillis,
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
use crate::pty::{CommandSpec, PtyBackend, PtyRestoreInfo, PtySupervisorStatus};
use crate::runtime::{RuntimeError, SessionRuntime};
use crate::session::{
    AttachRole as RuntimeAttachRole, SessionState as RuntimeSessionState,
    TerminalSize as RuntimeTerminalSize,
};
use crate::state::{
    DaemonIdentitySnapshot, DaemonState, SessionStateRecord, StateError, StateStore,
    TrustedDeviceState,
    client_history::{ClientHistoryRecord, ClientHistoryStore, SessionHistoryRecord},
};

use super::screen::TerminalScreen;
use super::{
    E2eeError, E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
};

const AUTH_CHALLENGE_TTL_MS: u64 = 60_000;
const LIVE_OUTPUT_MIN_BYTES: usize = 16 * 1024;
const LIVE_OUTPUT_BYTES_PER_CELL: usize = 8;
const LIVE_OUTPUT_DRAIN_MAX_CHUNKS: usize = 64;
const SESSION_FILE_DOWNLOAD_TOKEN_TTL_MS: u64 = 60_000;
const SESSION_FILE_DOWNLOAD_GRANT_LIMIT: usize = 128;
const SESSION_FILE_DOWNLOAD_CHUNK_MAX_BYTES: u32 = 256 * 1024;

/// 协议层统一使用的 JSON envelope。
pub type JsonEnvelope = Envelope<Value>;

/// 单个已配对客户端在当前 daemon 上的可见状态。
///
/// 这是个人使用场景里的连接清单，不是审计日志；relay 不参与生成或解释这些字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonClientRecord {
    client_id: ClientId,
    device_id: DeviceId,
    name: Option<String>,
    peer_ip: Option<String>,
    online: bool,
    connected_at_ms: UnixTimestampMillis,
    last_seen_at_ms: UnixTimestampMillis,
    active_connections: HashMap<ClientId, HashSet<SessionId>>,
    cursor_session_id: Option<SessionId>,
    cursor_row: Option<u16>,
    cursor_col: Option<u16>,
    cursor_focused: Option<bool>,
}

/// supervisor 恢复后补给协议层的可见 session 元数据。
#[derive(Debug, Clone)]
struct RestoredSessionMetadata {
    name: Option<String>,
    root_path: PathBuf,
}

/// daemon 启动时暂时没接回的 Running session。
///
/// 这类记录不能立刻标记为 Closed：supervisor 进程可能仍在，只是 socket 文件或 IPC
/// 短暂不可达。保留它可以让后续 list/attach 或下一次 daemon 重启继续尝试恢复。
#[derive(Debug, Clone)]
struct PendingRestoreSession {
    record: SessionStateRecord,
    metadata: RestoredSessionMetadata,
}

/// HTTP 文件下载的一次性授权。
///
/// token 只存在 daemon 内存中，必须先通过 E2EE WebSocket 申请；HTTP 层只消费 token
/// 并流式读取文件，不接收任意路径参数，避免把文件权限逻辑散落到 Axum handler。
#[derive(Debug, Clone)]
pub struct SessionFileDownloadGrant {
    pub path: PathBuf,
    pub download_name: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<UnixTimestampMillis>,
    pub expires_at_ms: UnixTimestampMillis,
}

/// session 级输出回放窗口。
///
/// PTY 输出只能被读取一次；这里先按 session 保留，再按每条连接自己的 offset 加密发送，
/// 避免重新 attach 或多个客户端同时 attach 时丢失已经读过的终端内容。
///
/// 新 attach 使用按逻辑行维护的 terminal snapshot，最多回放最近 1000 行实际行内容和样式。
#[derive(Debug, Clone)]
struct SessionOutputHistory {
    base_offset: u64,
    bytes: VecDeque<u8>,
    screen: TerminalScreen,
}

impl SessionOutputHistory {
    fn new(size: TerminalSize) -> Self {
        Self {
            base_offset: 0,
            bytes: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
        }
    }

    fn base_offset(&self) -> u64 {
        self.base_offset
    }

    fn end_offset(&self) -> u64 {
        self.base_offset + self.bytes.len() as u64
    }

    fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());
        self.screen.apply(bytes);
        self.trim_to_live_output_limit();
    }

    fn resize(&mut self, size: TerminalSize) {
        self.screen.resize(size.rows, size.cols);
        self.trim_to_live_output_limit();
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        self.screen.snapshot_bytes()
    }

    fn trim_to_live_output_limit(&mut self) {
        // raw bytes 只是给已连接客户端做增量 fanout；新 attach 使用 screen snapshot，不再缓存长 scrollback。
        let max_bytes =
            LIVE_OUTPUT_MIN_BYTES.max(self.screen.cell_count() * LIVE_OUTPUT_BYTES_PER_CELL);
        while self.bytes.len() > max_bytes {
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
    session_terminal_cwds: HashMap<SessionId, PathBuf>,
    session_file_downloads: HashMap<String, SessionFileDownloadGrant>,
    daemon_clients: HashMap<DeviceId, DaemonClientRecord>,
    client_history: ClientHistoryStore,
    session_output_history: HashMap<SessionId, SessionOutputHistory>,
    session_file_tree_signals: HashMap<SessionId, watch::Sender<u64>>,
    session_resize_signals: HashMap<SessionId, watch::Sender<TerminalSize>>,
    session_resize_owners: HashMap<SessionId, ClientId>,
    pending_restore_sessions: HashMap<SessionId, PendingRestoreSession>,
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
        let persisted_sessions = state.sessions.clone();
        let daemon_identity = match state.daemon_identity {
            Some(identity) => match identity.private_key {
                Some(private_key) => DaemonIdentity::from_persisted_identity(
                    identity.server_id,
                    identity.public_key,
                    private_key,
                )
                .map_err(|source| StateError::InvalidDaemonIdentity {
                    source: source.to_string(),
                })?,
                None => DaemonIdentity::generate_for_server_id(identity.server_id),
            },
            None => DaemonIdentity::generate(),
        };
        let trusted_store = InMemoryTrustedDeviceStore::from_trusted_devices(
            state
                .trusted_devices
                .into_iter()
                .map(trusted_device_from_state),
        );
        let mut protocol = Self::from_identity_and_store(
            config,
            backend,
            verifier,
            daemon_identity,
            trusted_store,
        )?;
        protocol.restore_runtime_sessions(persisted_sessions);
        Ok(protocol)
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
            session_terminal_cwds: HashMap::new(),
            session_file_downloads: HashMap::new(),
            daemon_clients: HashMap::new(),
            client_history,
            session_output_history: HashMap::new(),
            session_file_tree_signals: HashMap::new(),
            session_resize_signals: HashMap::new(),
            session_resize_owners: HashMap::new(),
            pending_restore_sessions: HashMap::new(),
        })
    }

    /// 生成可写入本地 SQLite 的最小状态快照。
    ///
    /// 不保存 pairing token、auth challenge、E2EE 临时密钥、PTY 输出或终端输入。
    pub fn snapshot_state(&self) -> DaemonState {
        let mut trusted_devices: Vec<_> = self
            .trusted_store
            .trusted_devices()
            .map(trusted_device_to_state)
            .collect();
        trusted_devices.sort_by_key(|device| device.device_id.0);

        let mut sessions = Vec::new();
        for (wire_session_id, internal_session_id) in &self.session_index {
            let Ok(state) = self.runtime.state(internal_session_id) else {
                continue;
            };
            let Ok(size) = self.runtime.size(internal_session_id) else {
                continue;
            };
            let Ok(Some(restore_info)) = self.runtime.restore_info(internal_session_id) else {
                continue;
            };
            let history = self.client_history_session_record(*wire_session_id);
            // session 元数据表可能被旧安装脚本清空过；runtime supervisor 仍是事实来源。
            // 这里不能因为缺少展示元数据就跳过持久化，否则下一次重启会彻底失去可恢复记录。
            let now_ms = current_unix_timestamp_millis();
            sessions.push(SessionStateRecord {
                session_id: *wire_session_id,
                state: runtime_state_to_proto(state),
                size: runtime_size_to_proto(size),
                created_at_ms: history
                    .as_ref()
                    .map(|record| record.created_at_ms)
                    .unwrap_or(now_ms),
                updated_at_ms: history
                    .as_ref()
                    .map(|record| record.updated_at_ms)
                    .unwrap_or(now_ms),
                restore_info: Some(restore_info),
            });
        }
        for pending in self.pending_restore_sessions.values() {
            sessions.push(pending.record.clone());
        }

        DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: Some(DaemonIdentitySnapshot {
                server_id: self.daemon_identity.server_id(),
                public_key: self.daemon_identity.public_key().clone(),
                private_key: Some(self.daemon_identity.private_key_for_persistence()),
            }),
            trusted_devices,
            sessions,
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
        self.record_daemon_client_connection(connection, accepted.device_id, None);

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
        self.record_daemon_client_connection(connection, authenticated.device_id, None);
        let _ = self.persist_state();
        Ok(Vec::new())
    }

    fn record_client_hello(
        &mut self,
        connection: &ProtocolConnection,
        payload: ClientHelloPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let name = sanitize_client_name(payload.name)?;
        let now_ms = current_unix_timestamp_millis();

        if let Err(error) = self
            .client_history
            .record_client_name(device_id, &name, now_ms)
        {
            tracing::warn!(%error, "failed to persist daemon client display name");
        }
        if let Some(record) = self.daemon_clients.get_mut(&device_id) {
            record.name = Some(name);
            record.last_seen_at_ms = now_ms;
        }
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
        let wire_session_id = SessionId::new();
        self.runtime
            .create_session_with_id(&wire_session_id.0.to_string(), command, runtime_size)
            .map_err(map_runtime_error)?;
        let internal_session_id = wire_session_id.0.to_string();
        let session_name = self.default_created_session_name(wire_session_id);

        self.session_index
            .insert(wire_session_id, internal_session_id.clone());
        self.session_names
            .insert(wire_session_id, session_name.clone());
        self.session_roots
            .insert(wire_session_id, session_root.clone());
        self.session_output_history_mut(wire_session_id, payload.size);
        let (file_tree_signal, _) = watch::channel(0);
        self.session_file_tree_signals
            .insert(wire_session_id, file_tree_signal);
        let (resize_signal, _) = watch::channel(payload.size);
        self.session_resize_signals
            .insert(wire_session_id, resize_signal);
        self.client_history.record_session_created(
            wire_session_id,
            self.runtime_state_proto(&internal_session_id)?,
            self.runtime_size_proto(&internal_session_id)?,
            Some(session_name.as_str()),
            &session_root,
            current_unix_timestamp_millis(),
        )?;

        let role = self
            .runtime
            .attach(&internal_session_id, device_key(device_id))
            .map_err(map_runtime_error)?;
        let wire_role = runtime_role_to_proto(role);
        self.drain_runtime_output_to_history_until_empty(
            wire_session_id,
            &internal_session_id,
            16 * 1024,
        )?;
        let response_size = self.runtime_size_proto(&internal_session_id)?;
        let (output_offset, initial_output) =
            self.output_history_attach_snapshot(wire_session_id, response_size);
        connection.attach(wire_session_id, output_offset, initial_output);
        self.record_daemon_client_attach(wire_session_id, connection, device_id);
        let resize_owner = self.assign_resize_owner_if_missing(wire_session_id, connection);

        let response = SessionCreatedPayload {
            session_id: wire_session_id,
            name: Some(session_name),
            role: wire_role,
            state: self.runtime_state_proto(&internal_session_id)?,
            size: response_size,
            resize_owner,
        };

        self.persist_state()?;
        connection.state = ProtocolConnectionState::Attached;
        Ok(vec![envelope_value(MessageType::SessionCreated, response)?])
    }

    fn attach_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionAttachPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        if !self.session_index.contains_key(&payload.session_id) {
            match self.reconnect_pending_session(payload.session_id) {
                Ok(true) => self.persist_state()?,
                Ok(false) => {}
                Err(error) => return Err(map_runtime_error(error)),
            }
        }
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
        self.drain_runtime_output_to_history_until_empty(
            payload.session_id,
            &internal_session_id,
            16 * 1024,
        )?;
        let response_size = self.runtime_size_proto(&internal_session_id)?;
        let (output_offset, initial_output) =
            self.output_history_attach_snapshot(payload.session_id, response_size);
        connection.attach(payload.session_id, output_offset, initial_output);
        self.record_daemon_client_attach(payload.session_id, connection, device_id);
        let resize_owner = self.assign_resize_owner_if_missing(payload.session_id, connection);
        connection.state = ProtocolConnectionState::Attached;

        let response = SessionAttachedPayload {
            session_id: payload.session_id,
            role: wire_role,
            state: self.runtime_state_proto(&internal_session_id)?,
            size: response_size,
            resize_owner,
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
        if !self.connection_is_resize_owner(payload.session_id, connection) {
            return Err(ProtocolError::InvalidState);
        }

        self.runtime
            .resize(internal_session_id, proto_size_to_runtime(payload.size))
            .map_err(map_runtime_error)?;
        if let Some(history) = self.session_output_history.get_mut(&payload.session_id) {
            history.resize(payload.size);
        }
        self.notify_session_resized(payload.session_id, payload.size);
        self.client_history.record_session_resized(
            payload.session_id,
            payload.size,
            current_unix_timestamp_millis(),
        )?;
        self.persist_state()?;

        Ok(vec![envelope_value(
            MessageType::SessionResized,
            SessionResizedPayload {
                session_id: payload.session_id,
                size: payload.size,
                resize_owner: true,
            },
        )?])
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
                name: None,
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections: HashMap::new(),
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
                cursor_focused: None,
            });
        record.cursor_session_id = Some(payload.session_id);
        record.cursor_row = Some(payload.row);
        record.cursor_col = Some(payload.col);
        record.cursor_focused = Some(payload.focused);
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
        connection.ensure_attached_to(payload.session_id)?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let name = sanitize_session_name(payload.name)?;
        self.session_names.insert(payload.session_id, name.clone());
        self.client_history.record_session_renamed(
            payload.session_id,
            Some(&name),
            current_unix_timestamp_millis(),
        )?;

        Ok(vec![envelope_value(
            MessageType::SessionRenamed,
            SessionRenamedPayload {
                session_id: payload.session_id,
                name,
            },
        )?])
    }

    fn reorder_sessions(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionReorderPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        self.repair_visible_session_metadata();
        let visible_session_ids = self.visible_session_ids();
        if visible_session_ids.is_empty() {
            return Ok(vec![envelope_value(
                MessageType::SessionReordered,
                SessionReorderedPayload {
                    session_ids: Vec::new(),
                },
            )?]);
        }

        let requested_ids = payload.session_ids.into_iter().collect::<Vec<_>>();
        let requested_set = requested_ids.iter().copied().collect::<HashSet<_>>();
        if requested_set.len() != requested_ids.len()
            || !requested_set.is_subset(&visible_session_ids)
        {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let ordered = self
            .client_history
            .record_session_order(&requested_ids, current_unix_timestamp_millis())?;

        Ok(vec![envelope_value(
            MessageType::SessionReordered,
            SessionReorderedPayload {
                session_ids: ordered,
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

        let close_result = self.runtime.close(&internal_session_id);
        if let Err(error) = &close_result {
            tracing::warn!(
                %error,
                session_id = %payload.session_id.0,
                "failed to terminate runtime session during explicit close"
            );
            let _ = self.runtime.discard(&internal_session_id);
        }
        self.close_visible_session_state(payload.session_id);
        if let Err(error) = self
            .client_history
            .record_session_closed(payload.session_id, current_unix_timestamp_millis())
        {
            tracing::warn!(%error, "failed to mark closed session in sqlite history");
        }
        if let Err(error) = self
            .client_history
            .remove_session_attachments(payload.session_id)
        {
            tracing::warn!(%error, "failed to remove closed session attachments from sqlite history");
        }
        self.persist_state()?;

        Ok(vec![envelope_value(
            MessageType::SessionClosed,
            SessionClosedPayload {
                session_id: payload.session_id,
            },
        )?])
    }

    fn close_visible_session_state(&mut self, session_id: SessionId) {
        self.session_index.remove(&session_id);
        self.session_names.remove(&session_id);
        self.session_roots.remove(&session_id);
        self.session_terminal_cwds.remove(&session_id);
        self.session_output_history.remove(&session_id);
        self.session_file_tree_signals.remove(&session_id);
        self.session_resize_signals.remove(&session_id);
        self.session_resize_owners.remove(&session_id);
        self.pending_restore_sessions.remove(&session_id);
        for record in self.daemon_clients.values_mut() {
            for sessions in record.active_connections.values_mut() {
                sessions.remove(&session_id);
            }
        }
    }

    fn list_session_files(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFilesPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        connection.ensure_attached_to(payload.session_id)?;
        if !self.session_index.contains_key(&payload.session_id) {
            return Err(ProtocolError::SessionNotFound);
        }

        let has_explicit_path = payload
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .is_some();
        let requested_path = if has_explicit_path {
            payload.path.clone()
        } else {
            self.default_session_files_path(payload.session_id)?
        };
        let result =
            self.session_files_result(payload.session_id, requested_path, !has_explicit_path)?;
        self.notify_session_file_tree_changed(payload.session_id);

        Ok(vec![envelope_value(
            MessageType::SessionFilesResult,
            result,
        )?])
    }

    fn read_session_file(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileReadPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        connection.ensure_attached_to(payload.session_id)?;
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
        connection.ensure_attached_to(payload.session_id)?;
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
        self.notify_session_file_tree_changed(payload.session_id);

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
        connection.ensure_attached_to(payload.session_id)?;
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
        self.notify_session_file_tree_changed(payload.session_id);

        Ok(vec![envelope_value(
            MessageType::SessionFileDeleted,
            SessionFileDeletedPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
            },
        )?])
    }

    fn prepare_session_file_download(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDownloadPreparePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        connection.ensure_attached_to(payload.session_id)?;
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

        let now_ms = current_unix_timestamp_millis();
        self.expire_session_file_downloads(now_ms);
        if self.session_file_downloads.len() >= SESSION_FILE_DOWNLOAD_GRANT_LIMIT {
            // token 只作为兼容旧前端的准备信号；限制数量，避免认证客户端刷爆内存。
            return Err(ProtocolError::InvalidState);
        }
        let token = session_file_download_token();
        let expires_at_ms =
            UnixTimestampMillis(now_ms.0.saturating_add(SESSION_FILE_DOWNLOAD_TOKEN_TTL_MS));
        let modified_at_ms = metadata_modified_at_ms(&metadata);
        let path = absolute_path_string(&target);
        self.session_file_downloads.insert(
            token.clone(),
            SessionFileDownloadGrant {
                path: target.clone(),
                download_name: session_file_download_name(&target),
                size_bytes: metadata.len(),
                modified_at_ms,
                expires_at_ms,
            },
        );

        Ok(vec![envelope_value(
            MessageType::SessionFileDownloadReady,
            SessionFileDownloadReadyPayload {
                session_id: payload.session_id,
                path,
                token,
                size_bytes: metadata.len(),
                modified_at_ms,
                expires_at_ms,
            },
        )?])
    }

    fn read_session_file_download_chunk(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDownloadChunkPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        connection.ensure_attached_to(payload.session_id)?;
        if payload.max_bytes == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
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

        let read_len = payload.max_bytes.min(SESSION_FILE_DOWNLOAD_CHUNK_MAX_BYTES) as usize;
        let mut file = fs::File::open(&target).map_err(map_file_path_error)?;
        file.seek(SeekFrom::Start(payload.offset_bytes))
            .map_err(map_file_path_error)?;
        let mut bytes = vec![0_u8; read_len];
        let read = file.read(&mut bytes).map_err(map_file_path_error)?;
        bytes.truncate(read);
        let next_offset_bytes = payload.offset_bytes.saturating_add(read as u64);
        let size_bytes = metadata.len();

        Ok(vec![envelope_value(
            MessageType::SessionFileDownloadChunkResult,
            SessionFileDownloadChunkResultPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
                offset_bytes: payload.offset_bytes,
                data_base64: general_purpose::STANDARD.encode(bytes),
                next_offset_bytes,
                size_bytes,
                eof: read == 0 || next_offset_bytes >= size_bytes,
                modified_at_ms: metadata_modified_at_ms(&metadata),
            },
        )?])
    }

    pub fn consume_session_file_download(
        &mut self,
        token: &str,
        now_ms: UnixTimestampMillis,
    ) -> Result<SessionFileDownloadGrant, ProtocolError> {
        self.expire_session_file_downloads(now_ms);
        let grant = self
            .session_file_downloads
            .remove(token)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        if grant.expires_at_ms <= now_ms {
            return Err(ProtocolError::InvalidEnvelope);
        }
        Ok(grant)
    }

    fn expire_session_file_downloads(&mut self, now_ms: UnixTimestampMillis) {
        self.session_file_downloads
            .retain(|_, grant| grant.expires_at_ms > now_ms);
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
        self.retry_pending_restore_sessions();
        self.repair_visible_session_metadata();

        let sessions_by_id: HashMap<SessionId, _> = match self.client_history.list_sessions() {
            Ok(sessions) => sessions
                .into_iter()
                .map(|session| (session.session_id, session))
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "failed to list session metadata from sqlite history");
                HashMap::new()
            }
        };

        let mut sessions: Vec<_> = self
            .session_index
            .iter()
            .filter_map(|(wire_id, internal_id)| {
                let state = self.runtime.state(internal_id).ok()?;
                let size = self.runtime.size(internal_id).ok()?;
                let persisted = sessions_by_id.get(wire_id);
                Some(SessionSummaryPayload {
                    session_id: *wire_id,
                    name: self
                        .session_names
                        .get(wire_id)
                        .cloned()
                        .or_else(|| persisted.and_then(|session| session.name.clone())),
                    state: runtime_state_to_proto(state),
                    size: runtime_size_to_proto(size),
                    files_path: persisted.and_then(|session| session.files_path.clone()),
                    created_at_ms: persisted.map(|session| session.created_at_ms),
                })
            })
            .collect();
        sessions.extend(self.pending_restore_sessions.values().map(|pending| {
            let persisted = sessions_by_id.get(&pending.record.session_id);
            SessionSummaryPayload {
                session_id: pending.record.session_id,
                name: pending
                    .metadata
                    .name
                    .clone()
                    .or_else(|| persisted.and_then(|session| session.name.clone())),
                state: pending.record.state,
                size: pending.record.size,
                files_path: persisted
                    .and_then(|session| session.files_path.clone())
                    .or_else(|| Some(pending.metadata.root_path.to_string_lossy().to_string())),
                created_at_ms: Some(pending.record.created_at_ms),
            }
        }));
        // HashMap 迭代没有业务顺序；session 列表固定按 daemon 持久化顺序返回。
        sessions.sort_by(|left, right| {
            let left_order = sessions_by_id
                .get(&left.session_id)
                .map(|record| record.display_order)
                .unwrap_or(i64::MAX);
            let right_order = sessions_by_id
                .get(&right.session_id)
                .map(|record| record.display_order)
                .unwrap_or(i64::MAX);
            left_order
                .cmp(&right_order)
                .then_with(|| session_created_at(left).cmp(&session_created_at(right)))
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });

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
                    name: record.name.clone(),
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
            if record.name.is_some() {
                entry.name = record.name.clone();
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
            let cursor_is_for_attached_session = record
                .cursor_session_id
                .map(|session_id| client.attached_session_ids.contains(&session_id))
                .unwrap_or(false);
            if record.online && cursor_is_for_attached_session {
                client.cursor_session_id = record.cursor_session_id;
                client.cursor_row = record.cursor_row;
                client.cursor_col = record.cursor_col;
                client.cursor_focused = record.cursor_focused;
            }
        }
        clients.sort_by_key(|client| client.connected_at_ms);

        Ok(vec![envelope_value(
            MessageType::DaemonClientsResult,
            DaemonClientsResultPayload { clients },
        )?])
    }

    fn forget_daemon_client(
        &mut self,
        connection: &ProtocolConnection,
        payload: DaemonClientForgetPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        if self
            .daemon_clients
            .get(&payload.device_id)
            .map(|record| record.online)
            .unwrap_or(false)
        {
            return Err(ProtocolError::InvalidState);
        }

        let forgotten = self
            .client_history
            .forget_offline_client(payload.device_id)
            .map_err(|error| {
                tracing::warn!(%error, "failed to forget offline daemon client");
                ProtocolError::RuntimeFailed
            })?;
        if !forgotten {
            // 删除离线客户端是 UI 上的清理操作。用户连点或多个浏览器同时删除同一条历史记录时，
            // 第二个请求看到的就是“已经不存在”，应按幂等删除处理，而不是把竞态暴露成协议错误。
            tracing::debug!(
                device_id = %payload.device_id.0,
                "offline daemon client was already forgotten"
            );
        }
        self.daemon_clients.remove(&payload.device_id);

        Ok(vec![envelope_value(
            MessageType::DaemonClientForgot,
            DaemonClientForgotPayload {
                device_id: payload.device_id,
            },
        )?])
    }

    fn daemon_status(
        &mut self,
        connection: &ProtocolConnection,
        _payload: DaemonStatusPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        // daemon 状态暴露主机资源信息，只允许已认证设备在 E2EE 内层读取。
        connection.authenticated_device_id()?;

        Ok(vec![envelope_value(
            MessageType::DaemonStatusResult,
            collect_daemon_status(),
        )?])
    }

    fn record_daemon_client_connection(
        &mut self,
        connection: &ProtocolConnection,
        device_id: DeviceId,
        name: Option<&str>,
    ) {
        let now_ms = current_unix_timestamp_millis();
        let stable_client_id = stable_client_id_for_device(device_id);

        if let Err(error) = self.client_history.record_connection(
            device_id,
            name,
            connection.peer_ip.as_deref(),
            now_ms,
        ) {
            tracing::warn!(%error, "failed to persist daemon client connection");
        }

        if let Some(record) = self.daemon_clients.get_mut(&device_id) {
            if let Some(name) = name {
                record.name = Some(name.to_owned());
            }
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
                name: name.map(str::to_owned),
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections,
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
                cursor_focused: None,
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
                name: None,
                peer_ip: connection.peer_ip.clone(),
                online: true,
                connected_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                active_connections,
                cursor_session_id: None,
                cursor_row: None,
                cursor_col: None,
                cursor_focused: None,
            },
        );
    }

    fn assign_resize_owner_if_missing(
        &mut self,
        session_id: SessionId,
        connection: &ProtocolConnection,
    ) -> bool {
        // resize owner 是连接级状态，同一设备的两个浏览器标签页也不能同时改 PTY 尺寸。
        let owner = self
            .session_resize_owners
            .entry(session_id)
            .or_insert(connection.client_id);
        *owner == connection.client_id
    }

    fn connection_is_resize_owner(
        &self,
        session_id: SessionId,
        connection: &ProtocolConnection,
    ) -> bool {
        self.session_resize_owners
            .get(&session_id)
            .map(|owner| *owner == connection.client_id)
            .unwrap_or(false)
    }

    fn first_attached_connection_for_session(&self, session_id: SessionId) -> Option<ClientId> {
        let mut candidates: Vec<_> = self
            .daemon_clients
            .values()
            .filter(|record| record.online)
            .flat_map(|record| {
                record
                    .active_connections
                    .iter()
                    .filter(move |(_, sessions)| sessions.contains(&session_id))
                    .map(move |(client_id, _)| (*client_id, record.connected_at_ms))
            })
            .collect();
        candidates.sort_by_key(|(client_id, connected_at_ms)| (connected_at_ms.0, client_id.0));
        candidates.first().map(|(client_id, _)| *client_id)
    }

    fn refresh_resize_owner_after_detach(&mut self, session_id: SessionId, old_owner: ClientId) {
        if self.session_resize_owners.get(&session_id) != Some(&old_owner) {
            return;
        }
        match self.first_attached_connection_for_session(session_id) {
            Some(next_owner) => {
                self.session_resize_owners.insert(session_id, next_owner);
                self.notify_session_resized_with_current_size(session_id);
            }
            None => {
                self.session_resize_owners.remove(&session_id);
            }
        }
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
                record.cursor_focused = None;
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

    fn session_output_history_mut(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> &mut SessionOutputHistory {
        self.session_output_history
            .entry(session_id)
            .or_insert_with(|| SessionOutputHistory::new(size))
    }

    fn output_history_base_offset(&mut self, session_id: SessionId, size: TerminalSize) -> u64 {
        self.session_output_history_mut(session_id, size)
            .base_offset()
    }

    fn output_history_attach_snapshot(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> (u64, Vec<u8>) {
        let history = self.session_output_history_mut(session_id, size);
        history.resize(size);
        (history.end_offset(), history.snapshot_bytes())
    }

    fn drain_runtime_output_to_history(
        &mut self,
        session_id: SessionId,
        internal_session_id: &str,
        max_chunk_bytes: usize,
    ) -> Result<bool, ProtocolError> {
        if max_chunk_bytes == 0 {
            return Ok(false);
        }

        // 每个 session 每轮只拉一个 chunk，避免批量 flush 多个已 attach session 时，
        // 一个 session 把后续 session 的待读输出都消费掉。
        let mut buffer = vec![0_u8; max_chunk_bytes];
        let read = self
            .runtime
            .read_output(internal_session_id, &mut buffer)
            .map_err(map_runtime_error)?;
        if read == 0 {
            return Ok(false);
        }

        buffer.truncate(read);
        let size = self.runtime_size_proto(internal_session_id)?;
        self.session_output_history_mut(session_id, size)
            .append(&buffer);
        if self.sync_session_terminal_cwd(session_id)? {
            self.notify_session_file_tree_changed(session_id);
        }
        Ok(true)
    }

    fn drain_runtime_output_to_history_until_empty(
        &mut self,
        session_id: SessionId,
        internal_session_id: &str,
        max_chunk_bytes: usize,
    ) -> Result<(), ProtocolError> {
        // attach 前尽量把 PTY 已有输出折叠进 screen snapshot；设置上限避免持续刷屏进程拖住握手。
        for _ in 0..256 {
            if !self.drain_runtime_output_to_history(
                session_id,
                internal_session_id,
                max_chunk_bytes,
            )? {
                break;
            }
        }
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

    fn default_session_files_path(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<String>, ProtocolError> {
        self.sync_session_terminal_cwd(session_id)?;
        if let Some(cwd) = self.session_terminal_cwds.get(&session_id) {
            return Ok(Some(absolute_path_string(cwd)));
        }

        self.client_history
            .session_files_path(session_id)
            .map_err(ProtocolError::from)
    }

    fn sync_session_terminal_cwd(&mut self, session_id: SessionId) -> Result<bool, ProtocolError> {
        let Some(cwd) = self.read_session_terminal_cwd(session_id)? else {
            return Ok(false);
        };
        if self
            .session_terminal_cwds
            .get(&session_id)
            .is_some_and(|cached| cached == &cwd)
        {
            return Ok(false);
        }

        self.session_terminal_cwds.insert(session_id, cwd.clone());
        self.client_history.record_session_files_path(
            session_id,
            &cwd,
            current_unix_timestamp_millis(),
        )?;
        Ok(true)
    }

    fn read_session_terminal_cwd(
        &self,
        session_id: SessionId,
    ) -> Result<Option<PathBuf>, ProtocolError> {
        let Some(internal_session_id) = self.session_index.get(&session_id) else {
            return Ok(None);
        };
        let Some(cwd) = self
            .runtime
            .current_working_directory(internal_session_id)
            .map_err(map_runtime_error)?
        else {
            return Ok(None);
        };
        let cwd = match cwd.canonicalize() {
            Ok(cwd) => cwd,
            Err(error) => {
                tracing::debug!(%error, session_id = %session_id.0, path = %cwd.display(), "terminal cwd is not readable; keeping previous file tree path");
                return Ok(None);
            }
        };

        Ok(Some(cwd))
    }

    fn session_files_result(
        &mut self,
        session_id: SessionId,
        requested_path: Option<String>,
        fallback_to_root: bool,
    ) -> Result<SessionFilesResultPayload, ProtocolError> {
        let root = self
            .session_roots
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        let (target, normalized_path) = match resolve_session_file_target(&root, requested_path) {
            Ok(resolved) => resolved,
            Err(error) if fallback_to_root => {
                tracing::warn!(%error, session_id = %session_id.0, "persisted session file tree path is no longer readable; falling back to root");
                resolve_session_file_target(&root, None)?
            }
            Err(error) => return Err(error),
        };
        let entries = read_session_file_entries(&root, &target)?;
        self.client_history.record_session_files_path(
            session_id,
            &normalized_path,
            current_unix_timestamp_millis(),
        )?;

        Ok(SessionFilesResultPayload {
            session_id,
            path: normalized_path,
            entries,
        })
    }

    fn session_file_tree_update(
        &mut self,
        session_id: SessionId,
    ) -> Result<JsonEnvelope, ProtocolError> {
        if !self.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let requested_path = self.default_session_files_path(session_id)?;
        let payload = self.session_files_result(session_id, requested_path, true)?;
        envelope_value(MessageType::SessionFilesResult, payload)
    }

    fn notify_session_file_tree_changed(&self, session_id: SessionId) {
        let Some(signal) = self.session_file_tree_signals.get(&session_id) else {
            return;
        };
        let next_version = signal.borrow().saturating_add(1);
        // 没有 watcher 时 send 会返回错误；文件树状态已经写入 SQLite，可以安全忽略。
        let _ = signal.send(next_version);
    }

    fn notify_session_resized(&self, session_id: SessionId, size: TerminalSize) {
        let Some(signal) = self.session_resize_signals.get(&session_id) else {
            return;
        };
        // resize 是 session 元数据，不含终端明文；推送给已 attach 连接可避免多窗口尺寸认知分叉。
        let _ = signal.send(size);
    }

    fn notify_session_resized_with_current_size(&self, session_id: SessionId) {
        let Some(internal_session_id) = self.session_index.get(&session_id) else {
            return;
        };
        let Ok(size) = self.runtime_size_proto(internal_session_id) else {
            return;
        };
        self.notify_session_resized(session_id, size);
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
        connection.pending_outputs.clear();
        self.mark_daemon_client_connection_offline(device_id, connection.client_id, now_ms);

        // 断开 WebSocket 只 detach 当前连接关联的 session，不 close/terminate PTY。
        // 同一浏览器/设备如果还有另一条 attach 连接在线，不能撤掉设备级 operator 角色。
        for wire_session_id in attached_sessions {
            self.refresh_resize_owner_after_detach(wire_session_id, connection.client_id);
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

    fn file_tree_signal(
        &self,
        session_id: SessionId,
    ) -> Result<Option<watch::Receiver<u64>>, ProtocolError> {
        if !self.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        Ok(self
            .session_file_tree_signals
            .get(&session_id)
            .map(watch::Sender::subscribe))
    }

    fn resize_signal(
        &self,
        session_id: SessionId,
    ) -> Result<Option<watch::Receiver<TerminalSize>>, ProtocolError> {
        if !self.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        Ok(self
            .session_resize_signals
            .get(&session_id)
            .map(watch::Sender::subscribe))
    }

    fn visible_session_ids(&self) -> HashSet<SessionId> {
        self.session_index
            .keys()
            .copied()
            .chain(self.pending_restore_sessions.keys().copied())
            .collect()
    }

    fn repair_visible_session_metadata(&mut self) {
        let session_ids = self.visible_session_ids().into_iter().collect::<Vec<_>>();
        for session_id in session_ids {
            if let Err(error) = self.repair_visible_session_metadata_for(session_id) {
                tracing::warn!(%error, session_id = %session_id.0, "failed to repair visible session metadata");
            }
        }
    }

    fn repair_visible_session_metadata_for(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), ProtocolError> {
        let current_record = self
            .client_history
            .session_record_including_closed(session_id)?;
        if matches!(
            current_record.as_ref().map(|record| record.state),
            Some(SessionState::Running | SessionState::Created)
        ) {
            return Ok(());
        }

        if let Some(internal_id) = self.session_index.get(&session_id).cloned() {
            let state = self.runtime_state_proto(&internal_id)?;
            let size = self.runtime_size_proto(&internal_id)?;
            let root_path = self
                .session_roots
                .get(&session_id)
                .cloned()
                .or_else(|| {
                    current_record
                        .as_ref()
                        .map(|record| PathBuf::from(&record.root_path))
                })
                .unwrap_or_else(|| self.default_restored_session_root());
            let files_path = current_record
                .as_ref()
                .and_then(|record| record.files_path.as_ref().map(PathBuf::from))
                .unwrap_or_else(|| root_path.clone());
            let default_name = self
                .session_names
                .get(&session_id)
                .cloned()
                .or_else(|| {
                    current_record
                        .as_ref()
                        .and_then(|record| record.name.clone())
                })
                .unwrap_or_else(|| default_restored_session_name(session_id));
            let created_at_ms = current_record
                .as_ref()
                .map(|record| record.created_at_ms)
                .unwrap_or_else(current_unix_timestamp_millis);
            self.client_history.record_session_restored(
                session_id,
                state,
                size,
                &root_path,
                &default_name,
                &files_path,
                created_at_ms,
                current_unix_timestamp_millis(),
            )?;
            return Ok(());
        }

        if let Some(pending) = self.pending_restore_sessions.get(&session_id).cloned() {
            let root_path = pending.metadata.root_path;
            let files_path = current_record
                .as_ref()
                .and_then(|record| record.files_path.as_ref().map(PathBuf::from))
                .unwrap_or_else(|| root_path.clone());
            let default_name = pending
                .metadata
                .name
                .or_else(|| {
                    current_record
                        .as_ref()
                        .and_then(|record| record.name.clone())
                })
                .unwrap_or_else(|| default_restored_session_name(session_id));
            let created_at_ms = current_record
                .as_ref()
                .map(|record| record.created_at_ms)
                .unwrap_or(pending.record.created_at_ms);
            self.client_history.record_session_restored(
                session_id,
                pending.record.state,
                pending.record.size,
                &root_path,
                &default_name,
                &files_path,
                created_at_ms,
                current_unix_timestamp_millis(),
            )?;
        }

        Ok(())
    }

    fn restore_runtime_sessions(&mut self, sessions: Vec<SessionStateRecord>) {
        let persisted_by_id = self.visible_session_metadata_by_id();

        for session in sessions {
            let wire_session_id = session.session_id;
            // runtime_sessions 的 restore_info 是 supervisor 可重连事实；client history
            // 缺失只影响展示元数据，不能让存活 session 从 Web 列表消失。
            if session.state != SessionState::Running
                || session.restore_info.is_none()
                || !restore_info_is_reconnectable(session.restore_info.as_ref())
            {
                self.mark_persisted_session_closed(wire_session_id);
                continue;
            }

            match self.runtime.reconnect_session(&session) {
                Ok(()) => {
                    let metadata = self
                        .restored_session_metadata(&session, persisted_by_id.get(&wire_session_id));
                    self.register_restored_runtime_session(&session, metadata);
                }
                Err(error) => {
                    tracing::warn!(%error, session_id = %wire_session_id.0, "failed to reconnect persisted session supervisor");
                    let metadata = self
                        .restored_session_metadata(&session, persisted_by_id.get(&wire_session_id));
                    self.pending_restore_sessions.insert(
                        wire_session_id,
                        PendingRestoreSession {
                            record: session,
                            metadata,
                        },
                    );
                }
            }
        }
        if let Err(error) = self.persist_state() {
            tracing::warn!(%error, "failed to persist recovered session supervisor state");
        }
    }

    fn register_restored_runtime_session(
        &mut self,
        session: &SessionStateRecord,
        metadata: RestoredSessionMetadata,
    ) {
        let wire_session_id = session.session_id;
        let internal_session_id = wire_session_id.0.to_string();
        self.pending_restore_sessions.remove(&wire_session_id);
        self.session_index
            .insert(wire_session_id, internal_session_id);
        self.session_output_history_mut(wire_session_id, session.size);
        let (file_tree_signal, _) = watch::channel(0);
        self.session_file_tree_signals
            .insert(wire_session_id, file_tree_signal);
        let (resize_signal, _) = watch::channel(session.size);
        self.session_resize_signals
            .insert(wire_session_id, resize_signal);
        self.session_roots
            .insert(wire_session_id, metadata.root_path);
        if let Some(name) = metadata.name {
            self.session_names.insert(wire_session_id, name);
        }
    }

    fn retry_pending_restore_sessions(&mut self) {
        let session_ids: Vec<_> = self.pending_restore_sessions.keys().copied().collect();
        let mut restored = false;
        for session_id in session_ids {
            match self.reconnect_pending_session(session_id) {
                Ok(true) => restored = true,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, session_id = %session_id.0, "pending session supervisor is still unreachable");
                }
            }
        }
        if restored {
            if let Err(error) = self.persist_state() {
                tracing::warn!(%error, "failed to persist recovered pending session supervisor state");
            }
        }
    }

    fn reconnect_pending_session(&mut self, session_id: SessionId) -> Result<bool, RuntimeError> {
        if self.session_index.contains_key(&session_id) {
            return Ok(true);
        }
        let Some(pending) = self.pending_restore_sessions.get(&session_id).cloned() else {
            return Ok(false);
        };
        self.runtime.reconnect_session(&pending.record)?;
        self.register_restored_runtime_session(&pending.record, pending.metadata);
        Ok(true)
    }

    fn visible_session_metadata_by_id(&self) -> HashMap<SessionId, SessionHistoryRecord> {
        match self.restore_session_metadata_by_id() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "failed to load session metadata while restoring supervisors");
                HashMap::new()
            }
        }
    }

    fn restore_session_metadata_by_id(
        &self,
    ) -> Result<HashMap<SessionId, SessionHistoryRecord>, StateError> {
        let mut records = HashMap::new();
        for record in self.client_history.list_sessions()? {
            records.insert(record.session_id, record);
        }

        // snapshot_state 和 list_sessions 只看可见行；恢复路径还必须补查 closed 行。
        // 之前安装/重启流程可能把仍存活的 session 元数据标成 closed，此时如果不补查，
        // 领养 live supervisor 时就会丢掉用户设置的 session 名称。
        for session_id in self
            .session_index
            .keys()
            .chain(self.pending_restore_sessions.keys())
        {
            if records.contains_key(session_id) {
                continue;
            }
            if let Some(record) = self
                .client_history
                .session_record_including_closed(*session_id)?
            {
                records.insert(*session_id, record);
            }
        }

        Ok(records)
    }

    fn restored_session_metadata(
        &mut self,
        session: &SessionStateRecord,
        persisted: Option<&SessionHistoryRecord>,
    ) -> RestoredSessionMetadata {
        if let Some(record) = persisted {
            return self.restore_session_metadata_from_existing_record(session, record);
        }
        match self
            .client_history
            .session_record_including_closed(session.session_id)
        {
            Ok(Some(record)) => {
                return self.restore_session_metadata_from_existing_record(session, &record);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, session_id = %session.session_id.0, "failed to load closed session metadata while restoring supervisor");
            }
        }

        let root_path = self.default_restored_session_root();
        let default_name = default_restored_session_name(session.session_id);

        match self.client_history.record_session_restored(
            session.session_id,
            session.state,
            session.size,
            &root_path,
            &default_name,
            &root_path,
            session.created_at_ms,
            session.updated_at_ms,
        ) {
            Ok(record) => restored_session_metadata_from_record(&record),
            Err(error) => {
                tracing::warn!(%error, session_id = %session.session_id.0, "failed to repair restored session metadata in sqlite history");
                // SQLite 元数据修复失败不能让已经重连成功的 supervisor 再次不可见。
                RestoredSessionMetadata {
                    name: Some(default_name),
                    root_path,
                }
            }
        }
    }

    fn restore_session_metadata_from_existing_record(
        &mut self,
        session: &SessionStateRecord,
        record: &SessionHistoryRecord,
    ) -> RestoredSessionMetadata {
        let root_path = PathBuf::from(&record.root_path);
        let files_path = record
            .files_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| root_path.clone());
        let default_name = record
            .name
            .clone()
            .unwrap_or_else(|| default_restored_session_name(session.session_id));

        match self.client_history.record_session_restored(
            session.session_id,
            session.state,
            session.size,
            &root_path,
            &default_name,
            &files_path,
            record.created_at_ms,
            session.updated_at_ms,
        ) {
            Ok(repaired) => restored_session_metadata_from_record(&repaired),
            Err(error) => {
                tracing::warn!(%error, session_id = %session.session_id.0, "failed to repair existing session metadata while restoring supervisor");
                // 修复 state 失败不影响本次内存恢复；至少保留已读到的名称和 root。
                restored_session_metadata_from_record(record)
            }
        }
    }

    fn default_restored_session_root(&self) -> PathBuf {
        if let Some(root) = self
            .config
            .default_working_directory
            .as_ref()
            .and_then(|path| path.canonicalize().ok())
        {
            return root;
        }

        if let Ok(root) = std::env::current_dir().and_then(|path| path.canonicalize()) {
            return root;
        }

        // 极端环境下当前目录不可读时，退回系统临时目录，确保文件树根仍指向真实目录。
        std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
    }

    fn default_created_session_name(&self, session_id: SessionId) -> String {
        let mut occupied_names: HashSet<String> = self.session_names.values().cloned().collect();
        if let Ok(records) = self.client_history.list_sessions() {
            occupied_names.extend(records.into_iter().filter_map(|record| record.name));
        }

        for attempt in 0..SESSION_DISPLAY_NAMES.len() {
            let candidate = created_session_name_candidate(session_id, attempt).to_owned();
            if !occupied_names.contains(&candidate) {
                return candidate;
            }
        }

        let base = created_session_name_candidate(session_id, 0);
        let mut suffix = session_name_seed(session_id) % 900 + 100;
        loop {
            let candidate = format!("{base} {suffix}");
            if !occupied_names.contains(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn mark_persisted_session_closed(&mut self, session_id: SessionId) {
        self.close_visible_session_state(session_id);
        let now_ms = current_unix_timestamp_millis();
        if let Err(error) = self
            .client_history
            .record_session_closed(session_id, now_ms)
        {
            tracing::warn!(%error, session_id = %session_id.0, "failed to mark restored session closed in sqlite history");
        }
        if let Err(error) = self.client_history.remove_session_attachments(session_id) {
            tracing::warn!(%error, session_id = %session_id.0, "failed to clear restored session attachments from sqlite history");
        }
    }

    fn client_history_session_record(&self, session_id: SessionId) -> Option<SessionHistoryRecord> {
        self.client_history
            .session_record_including_closed(session_id)
            .ok()?
    }
}

/// Web UI 里的“客户端”是已配对浏览器/设备，不是每次 attach 新建的 WebSocket。
fn stable_client_id_for_device(device_id: DeviceId) -> ClientId {
    ClientId(device_id.0)
}

fn collect_daemon_status() -> DaemonStatusResultPayload {
    let (memory_total_bytes, memory_available_bytes) = read_memory_status();
    let (disk_total_bytes, disk_available_bytes) = read_root_disk_status();
    let (network_rx_bytes, network_tx_bytes) = read_physical_network_bytes();

    DaemonStatusResultPayload {
        host_name: read_host_name(),
        load_avg: read_load_avg(),
        uptime_seconds: read_uptime_seconds(),
        cpu_percent: read_cpu_percent_snapshot(),
        memory_total_bytes,
        memory_available_bytes,
        disk_total_bytes,
        disk_available_bytes,
        network_rx_bytes,
        network_tx_bytes,
        // 兼容 0.1.25 及更早前端的字段；新 UI 不再展示或采集进程数量。
        process_count: 0,
        atop_available: command_available("atop"),
    }
}

fn read_host_name() -> Option<String> {
    // Linux 优先走 /proc，非 Linux 或容器裁剪环境下回退到 HOSTNAME 环境变量。
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .and_then(|value| non_empty_trimmed(value))
        .or_else(|| env::var("HOSTNAME").ok().and_then(non_empty_trimmed))
}

fn read_load_avg() -> [f64; 3] {
    let Some(raw) = fs::read_to_string("/proc/loadavg").ok() else {
        return [0.0, 0.0, 0.0];
    };
    let mut parts = raw.split_whitespace();
    [
        parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
        parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
        parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
    ]
}

fn read_uptime_seconds() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|raw| raw.split_whitespace().next()?.parse::<f64>().ok())
        .map(|seconds| seconds.max(0.0) as u64)
        .unwrap_or(0)
}

fn read_memory_status() -> (u64, u64) {
    let Some(raw) = fs::read_to_string("/proc/meminfo").ok() else {
        return (0, 0);
    };
    let mut total = 0;
    let mut available = 0;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total = parse_meminfo_kib(value);
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available = parse_meminfo_kib(value);
        }
    }
    (total, available)
}

fn parse_meminfo_kib(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024)
}

fn read_cpu_percent_snapshot() -> f32 {
    let Some(sample) = read_cpu_sample() else {
        return 0.0;
    };
    if sample.total == 0 {
        return 0.0;
    }
    let busy = sample.total.saturating_sub(sample.idle);
    // 状态面板不能在 protocol mutex 下 sleep 采样；这里展示启动以来的粗略占用率。
    ((busy as f64 / sample.total as f64) * 100.0).clamp(0.0, 100.0) as f32
}

#[derive(Debug, Clone, Copy)]
struct CpuSample {
    total: u64,
    idle: u64,
}

fn read_cpu_sample() -> Option<CpuSample> {
    let raw = fs::read_to_string("/proc/stat").ok()?;
    let cpu_line = raw.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = cpu_line
        .split_whitespace()
        .skip(1)
        .filter_map(|part| part.parse().ok())
        .collect();
    if values.is_empty() {
        return None;
    }
    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    Some(CpuSample {
        total: values.iter().copied().sum(),
        idle,
    })
}

#[cfg(target_os = "linux")]
fn read_physical_network_bytes() -> (u64, u64) {
    read_physical_network_bytes_from_sys_class_net(Path::new("/sys/class/net"))
}

#[cfg(not(target_os = "linux"))]
fn read_physical_network_bytes() -> (u64, u64) {
    (0, 0)
}

#[cfg(any(target_os = "linux", test))]
fn read_physical_network_bytes_from_sys_class_net(root: &Path) -> (u64, u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return (0, 0);
    };
    let mut total_rx = 0u64;
    let mut total_tx = 0u64;
    for entry in entries.filter_map(Result::ok) {
        let iface_dir = entry.path();
        let iface_name = entry.file_name().to_string_lossy().to_string();
        if !is_physical_network_interface(&iface_name, &iface_dir) {
            continue;
        }
        let stats_dir = iface_dir.join("statistics");
        let Some(rx_bytes) = read_u64_file(&stats_dir.join("rx_bytes")) else {
            continue;
        };
        let Some(tx_bytes) = read_u64_file(&stats_dir.join("tx_bytes")) else {
            continue;
        };
        total_rx = total_rx.saturating_add(rx_bytes);
        total_tx = total_tx.saturating_add(tx_bytes);
    }
    (total_rx, total_tx)
}

#[cfg(any(target_os = "linux", test))]
fn is_physical_network_interface(iface_name: &str, iface_dir: &Path) -> bool {
    if iface_name == "lo" {
        return false;
    }
    if let Ok(canonical) = fs::canonicalize(iface_dir) {
        // Linux virtual interfaces normally resolve below /sys/devices/virtual/net.
        if canonical
            .to_string_lossy()
            .contains("/devices/virtual/net/")
        {
            return false;
        }
    }
    // 物理 PCI/USB/Wi-Fi 网卡通常带 device 链接；veth/docker/tun/bridge 没有。
    iface_dir.join("device").exists()
}

#[cfg(any(target_os = "linux", test))]
fn read_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

#[cfg(unix)]
fn read_root_disk_status() -> (u64, u64) {
    use std::ffi::CString;

    let Ok(path) = CString::new("/") else {
        return (0, 0);
    };
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs 只读取根文件系统元数据；失败时按不可用降级，不影响协议主流程。
    let rc = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if rc != 0 {
        return (0, 0);
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize.max(stats.f_bsize);
    (
        (stats.f_blocks as u64).saturating_mul(block_size as u64),
        (stats.f_bavail as u64).saturating_mul(block_size as u64),
    )
}

#[cfg(not(unix))]
fn read_root_disk_status() -> (u64, u64) {
    (0, 0)
}

fn command_available(command: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(command);
        let Ok(metadata) = fs::metadata(candidate) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

fn non_empty_trimmed(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn restored_session_metadata_from_record(record: &SessionHistoryRecord) -> RestoredSessionMetadata {
    RestoredSessionMetadata {
        name: record.name.clone(),
        root_path: PathBuf::from(&record.root_path),
    }
}

fn session_created_at(session: &SessionSummaryPayload) -> UnixTimestampMillis {
    session.created_at_ms.unwrap_or(UnixTimestampMillis(0))
}

fn default_restored_session_name(session_id: SessionId) -> String {
    let raw = session_id.0.to_string();
    format!("restored-{}", &raw[..8])
}

// 给新建 session 分配一个稳定、可读、不会随列表顺序变化的默认英文名。
const SESSION_DISPLAY_NAMES: &[&str] = &[
    "Ada",
    "Aristotle",
    "Babbage",
    "Bohr",
    "Byron",
    "Cantor",
    "Chandrasekhar",
    "Clarke",
    "Curie",
    "Darwin",
    "Dijkstra",
    "Dirac",
    "Engelbart",
    "Euclid",
    "Euler",
    "Faraday",
    "Fermi",
    "Fourier",
    "Franklin",
    "Galileo",
    "Gauss",
    "Gibbs",
    "Hamilton",
    "Hamming",
    "Heisenberg",
    "Herschel",
    "Hilbert",
    "Hopper",
    "Johnson",
    "Kant",
    "Kay",
    "Kepler",
    "Knuth",
    "Lagrange",
    "Lamport",
    "Laplace",
    "Lovelace",
    "Maxwell",
    "McCarthy",
    "McClintock",
    "Minsky",
    "Mirzakhani",
    "Newton",
    "Noether",
    "Pascal",
    "Perlis",
    "Planck",
    "Plato",
    "Ramanujan",
    "Rawls",
    "Riemann",
    "Russell",
    "Sartre",
    "Shannon",
    "Socrates",
    "Tesla",
    "Thompson",
    "Turing",
    "Vaughan",
    "Volta",
    "Weyl",
    "Wilkes",
];

fn created_session_name_candidate(session_id: SessionId, attempt: usize) -> &'static str {
    let index = session_name_seed(session_id).wrapping_add(attempt.wrapping_mul(17))
        % SESSION_DISPLAY_NAMES.len();
    SESSION_DISPLAY_NAMES[index]
}

fn session_name_seed(session_id: SessionId) -> usize {
    session_id
        .0
        .as_bytes()
        .iter()
        .enumerate()
        .fold(2_166_136_261usize, |acc, (index, byte)| {
            acc.wrapping_mul(16_777_619) ^ usize::from(*byte) ^ index.wrapping_mul(31)
        })
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
    pending_outputs: HashMap<SessionId, VecDeque<Vec<u8>>>,
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
            pending_outputs: HashMap::new(),
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

    /// 读取并加密当前 session 文件树状态，用于 WebSocket watcher 主动推送。
    pub fn read_session_file_tree_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_file_tree_update(protocol, session_id) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response(error)],
        }
    }

    /// 读取并加密当前 session 的 resize 状态，用于多窗口同步 PTY 尺寸元数据。
    pub fn read_session_resize_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_resize_update(protocol, session_id) {
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

    /// 返回当前连接可订阅的 session 活动信号。
    ///
    /// activity 只告诉前端“这个 session 有新输出”，不读取 PTY 内容；这样后台 session
    /// 可以在列表里变色，同时避免为了提示而把大块终端输出推给非当前 xterm。
    pub fn session_activity_signals<B, V>(
        &self,
        protocol: &DaemonProtocol<B, V>,
    ) -> Vec<(SessionId, watch::Receiver<u64>)>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        if self.state != ProtocolConnectionState::Attached || !self.is_authenticated() {
            return Vec::new();
        }

        protocol
            .session_index
            .keys()
            .filter_map(|session_id| {
                protocol
                    .output_signal(*session_id)
                    .ok()
                    .flatten()
                    .map(|signal| (*session_id, signal))
            })
            .collect()
    }

    /// 返回当前连接已 attach session 的文件树信号，供 WebSocket 层注册主动推送 watcher。
    pub fn attached_file_tree_signals<B, V>(
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
                    .file_tree_signal(*session_id)
                    .ok()
                    .flatten()
                    .map(|signal| (*session_id, signal))
            })
            .collect()
    }

    /// 返回当前连接已 attach session 的 resize 信号，供 WebSocket 层向其他窗口同步尺寸。
    pub fn attached_resize_signals<B, V>(
        &self,
        protocol: &DaemonProtocol<B, V>,
    ) -> Vec<(SessionId, watch::Receiver<TerminalSize>)>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.attached_sessions
            .iter()
            .filter_map(|session_id| {
                protocol
                    .resize_signal(*session_id)
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

        let mut chunks = Vec::new();
        if let Some(pending) = self.pending_outputs.get_mut(&session_id) {
            while let Some(chunk) = pending.pop_front() {
                chunks.push(chunk);
            }
        }

        let mut drained_chunks = 0;
        loop {
            self.collect_retained_output_chunks(
                protocol,
                session_id,
                &internal_session_id,
                max_bytes,
                &mut chunks,
            );
            if drained_chunks >= LIVE_OUTPUT_DRAIN_MAX_CHUNKS {
                break;
            }

            // watch::Receiver 会合并多次 PTY 输出信号；如果一次事件只读一个 chunk，
            // 剩余积压可能要等下一次用户输入才被推送。这里在单次唤醒内继续读空
            // 当前已经就绪的非阻塞 PTY 缓存，同时每轮先 collect 再继续 drain，避免
            // raw history 的保留窗口裁掉已连接客户端还没有收到的片段。
            if !protocol.drain_runtime_output_to_history(
                session_id,
                &internal_session_id,
                max_bytes,
            )? {
                break;
            }
            drained_chunks += 1;
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

    /// 发送后台 session 活动通知。
    ///
    /// 这里不调用 `drain_runtime_output_to_history`，避免 activity watcher 消费掉之后 attach
    /// 需要回放的 PTY 输出；真正内容仍由用户打开该 session 时走常规 attach/read 路径。
    pub fn read_session_activity<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_activity(protocol, session_id) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response(error)],
        }
    }

    fn try_read_session_activity<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if !protocol.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }

        self.encrypt_inner_messages(vec![envelope_value(
            MessageType::SessionActivity,
            SessionActivityPayload {
                session_id,
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )?])
    }

    fn collect_retained_output_chunks<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        internal_session_id: &str,
        max_bytes: usize,
        chunks: &mut Vec<Vec<u8>>,
    ) where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        loop {
            let cursor = self
                .output_offsets
                .get(&session_id)
                .copied()
                .unwrap_or_else(|| {
                    let size = protocol
                        .runtime_size_proto(internal_session_id)
                        .unwrap_or_else(|_| {
                            TerminalSize::new(
                                TerminalSize::DEFAULT_ROWS,
                                TerminalSize::DEFAULT_COLS,
                            )
                        });
                    protocol.output_history_base_offset(session_id, size)
                });
            let (bytes, next_cursor) =
                protocol.retained_output_chunk(session_id, cursor, max_bytes);
            self.output_offsets.insert(session_id, next_cursor);
            if bytes.is_empty() {
                break;
            }
            chunks.push(bytes);
        }
    }

    fn try_read_session_file_tree_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if !self.attached_sessions.contains(&session_id) {
            return Err(ProtocolError::InvalidState);
        }

        let update = protocol.session_file_tree_update(session_id)?;
        self.encrypt_inner_messages(vec![update])
    }

    fn try_read_session_resize_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if !self.attached_sessions.contains(&session_id) {
            return Err(ProtocolError::InvalidState);
        }
        let internal_session_id = protocol
            .session_index
            .get(&session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let size = protocol.runtime_size_proto(internal_session_id)?;

        self.encrypt_inner_messages(vec![envelope_value(
            MessageType::SessionResized,
            SessionResizedPayload {
                session_id,
                size,
                resize_owner: protocol.connection_is_resize_owner(session_id, self),
            },
        )?])
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
            MessageType::ClientHello => {
                let payload = decode_payload(envelope.payload)?;
                protocol.record_client_hello(self, payload)
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
            MessageType::SessionReorder => {
                let payload = decode_payload(envelope.payload)?;
                protocol.reorder_sessions(self, payload)
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
            MessageType::SessionFileDownloadPrepare => {
                let payload = decode_payload(envelope.payload)?;
                protocol.prepare_session_file_download(self, payload)
            }
            MessageType::SessionFileDownloadChunk => {
                let payload = decode_payload(envelope.payload)?;
                protocol.read_session_file_download_chunk(self, payload)
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
            MessageType::DaemonClientForget => {
                let payload = decode_payload(envelope.payload)?;
                protocol.forget_daemon_client(self, payload)
            }
            MessageType::DaemonStatus => {
                let payload = decode_payload(envelope.payload)?;
                protocol.daemon_status(self, payload)
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

    fn attach(&mut self, session_id: SessionId, output_base_offset: u64, initial_output: Vec<u8>) {
        if !self.attached_sessions.contains(&session_id) {
            self.attached_sessions.push(session_id);
            self.output_offsets.insert(session_id, output_base_offset);
            if !initial_output.is_empty() {
                self.pending_outputs
                    .entry(session_id)
                    .or_default()
                    .push_back(initial_output);
            }
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

    // Web 终端和 CLI attach 都按 xterm-256color 能力集启动 shell，保证颜色和补全体验一致。
    let mut command = CommandSpec::new(program)
        .args(argv)
        .env("TERM", "xterm-256color");
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
    let root_scoped =
        session_file_request_is_root_scoped(root, &candidate, requested.is_absolute())?;

    let target = candidate.canonicalize().map_err(map_file_path_error)?;
    if root_scoped {
        ensure_path_inside_root(root, &target)?;
    }
    Ok(target)
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
    let root_scoped =
        session_file_request_is_root_scoped(root, &candidate, requested.is_absolute())?;
    let Some(file_name) = candidate.file_name() else {
        return Err(ProtocolError::InvalidEnvelope);
    };
    let Some(parent) = candidate.parent() else {
        return Err(ProtocolError::InvalidEnvelope);
    };
    let parent = parent.canonicalize().map_err(map_file_path_error)?;
    if root_scoped {
        ensure_path_inside_root(root, &parent)?;
    }
    let target = parent.join(file_name);
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            // 写入会跟随 symlink；已存在目标必须先解析真实路径，避免 root 内 symlink 写到 root 外。
            let real_target = target.canonicalize().map_err(map_file_path_error)?;
            if root_scoped {
                ensure_path_inside_root(root, &real_target)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_file_path_error(error)),
    }

    Ok(target)
}

fn session_file_request_is_root_scoped(
    root: &Path,
    candidate: &Path,
    requested_absolute: bool,
) -> Result<bool, ProtocolError> {
    if !requested_absolute {
        return Ok(true);
    }
    let root = root.canonicalize().map_err(map_file_path_error)?;
    // 绝对路径如果本来就在 session root 内，仍按 root 沙箱处理；终端 cwd 跑到 root 外时，
    // daemon 会传入 root 外绝对路径，这类路径由已 attach 的可信设备显式访问。
    Ok(candidate.starts_with(&root))
}

fn ensure_path_inside_root(root: &Path, target: &Path) -> Result<(), ProtocolError> {
    let root = root.canonicalize().map_err(map_file_path_error)?;
    // root 作用于相对路径和 root 内绝对路径，避免 `..` 或 root 内 symlink 意外逃逸。
    if target.starts_with(&root) {
        return Ok(());
    }
    Err(ProtocolError::InvalidEnvelope)
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

fn session_file_download_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn session_file_download_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "download".to_owned())
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

fn sanitize_client_name(raw_name: String) -> Result<String, ProtocolError> {
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

fn restore_info_is_reconnectable(restore_info: Option<&PtyRestoreInfo>) -> bool {
    matches!(
        restore_info,
        Some(PtyRestoreInfo::UnixSocket {
            supervisor_status: PtySupervisorStatus::Running,
            ..
        })
    )
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
        name: record.name,
        peer_ip: record.peer_ip,
        online: record.online,
        connected_at_ms: record.connected_at_ms,
        last_seen_at_ms: record.last_seen_at_ms,
        attached_session_ids,
        cursor_session_id: None,
        cursor_row: None,
        cursor_col: None,
        cursor_focused: None,
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
    // 发送给客户端的 error payload 必须脱敏；真实 PTY/supervisor 错误写入 daemon 日志，
    // 这样其他机器出现 runtime_failed 时可以从 journalctl 查到权限、cwd 或 socket 细节。
    tracing::warn!(%error, "runtime operation failed");
    match error {
        RuntimeError::SessionNotFound => ProtocolError::SessionNotFound,
        RuntimeError::SessionAlreadyExists
        | RuntimeError::SessionClosed
        | RuntimeError::DeviceNotAttached
        | RuntimeError::NotReconnectable
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
    use crate::pty::{
        PtyBackend, PtyError, PtyExitStatus, PtyRestoreInfo, PtyResult, PtySession, PtySize,
        PtySnapshot, PtySupervisorStatus,
    };
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
        outputs_by_session: HashMap<String, VecDeque<Vec<u8>>>,
        cwd_by_session: HashMap<String, PathBuf>,
        writes: Vec<Vec<u8>>,
        reconnects: Vec<String>,
        reconnect_error: Option<String>,
        terminate_error: Option<String>,
        terminate_count: usize,
    }

    impl FakePtyBackend {
        fn push_output(&self, bytes: impl Into<Vec<u8>>) {
            self.state.lock().unwrap().outputs.push_back(bytes.into());
        }

        fn push_output_for_session(&self, session_id: SessionId, bytes: impl Into<Vec<u8>>) {
            self.state
                .lock()
                .unwrap()
                .outputs_by_session
                .entry(session_id.0.to_string())
                .or_default()
                .push_back(bytes.into());
        }

        fn set_cwd_for_session(&self, session_id: SessionId, cwd: impl Into<PathBuf>) {
            self.state
                .lock()
                .unwrap()
                .cwd_by_session
                .insert(session_id.0.to_string(), cwd.into());
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        fn terminate_count(&self) -> usize {
            self.state.lock().unwrap().terminate_count
        }

        fn reconnects(&self) -> Vec<String> {
            self.state.lock().unwrap().reconnects.clone()
        }

        fn fail_reconnects(&self, message: impl Into<String>) {
            self.state.lock().unwrap().reconnect_error = Some(message.into());
        }

        fn allow_reconnects(&self) {
            self.state.lock().unwrap().reconnect_error = None;
        }

        fn fail_terminate(&self, message: impl Into<String>) {
            self.state.lock().unwrap().terminate_error = Some(message.into());
        }
    }

    impl PtyBackend for FakePtyBackend {
        fn spawn(&self, _command: &CommandSpec, _size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
                session_id: None,
                restore_info: None,
            }))
        }

        fn spawn_named(
            &self,
            session_id: &str,
            _command: &CommandSpec,
            _size: PtySize,
        ) -> PtyResult<Box<dyn PtySession>> {
            let wire_session_id = SessionId(
                uuid::Uuid::parse_str(session_id)
                    .map_err(|source| PtyError::Backend(source.to_string()))?,
            );
            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
                session_id: Some(session_id.to_owned()),
                restore_info: Some(socket_restore_info(wire_session_id)),
            }))
        }

        fn reconnect(
            &self,
            session_id: &str,
            restore_info: &PtyRestoreInfo,
        ) -> PtyResult<Box<dyn PtySession>> {
            let mut state = self.state.lock().unwrap();
            state.reconnects.push(session_id.to_owned());
            if let Some(message) = state.reconnect_error.clone() {
                return Err(PtyError::Backend(message));
            }

            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
                session_id: Some(session_id.to_owned()),
                restore_info: Some(restore_info.clone()),
            }))
        }
    }

    struct FakePtySession {
        state: Arc<Mutex<FakePtyState>>,
        session_id: Option<String>,
        restore_info: Option<PtyRestoreInfo>,
    }

    impl PtySession for FakePtySession {
        fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
            let mut state = self.state.lock().unwrap();
            if let Some(session_id) = &self.session_id {
                if let Some(outputs) = state.outputs_by_session.get_mut(session_id) {
                    if let Some(read) = read_fake_output_queue(outputs, buffer) {
                        return Ok(read);
                    }
                }
            }

            // 没有关联到具体 session 的旧测试仍走全局输出队列。
            // 新的多 session 测试应优先使用 push_output_for_session，避免假 PTY 串流。
            Ok(read_fake_output_queue(&mut state.outputs, buffer).unwrap_or(0))
        }

        fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
            self.state.lock().unwrap().writes.push(bytes.to_vec());
            Ok(())
        }

        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::new(24, 80),
                process_id: Some(7),
                retained_output: Vec::new(),
            })
        }

        fn restore_info(&self) -> Option<PtyRestoreInfo> {
            self.restore_info.clone()
        }

        fn terminate(&mut self) -> PtyResult<()> {
            let mut state = self.state.lock().unwrap();
            state.terminate_count += 1;
            if let Some(message) = state.terminate_error.clone() {
                return Err(PtyError::Backend(message));
            }
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

        fn current_working_directory(&self) -> Option<PathBuf> {
            let session_id = self.session_id.as_ref()?;
            self.state
                .lock()
                .unwrap()
                .cwd_by_session
                .get(session_id)
                .cloned()
        }
    }

    fn read_fake_output_queue(outputs: &mut VecDeque<Vec<u8>>, buffer: &mut [u8]) -> Option<usize> {
        let Some(output) = outputs.pop_front() else {
            return None;
        };
        let read = output.len().min(buffer.len());
        buffer[..read].copy_from_slice(&output[..read]);

        if read < output.len() {
            // fake PTY 也保留短读后的剩余输出，便于测试协议层按 buffer 大小读取。
            outputs.push_front(output[read..].to_vec());
        }

        Some(read)
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

    fn socket_restore_info(session_id: SessionId) -> PtyRestoreInfo {
        PtyRestoreInfo::UnixSocket {
            socket_path: std::env::temp_dir().join(format!("termd-test-{}.sock", session_id.0)),
            supervisor_pid: 42,
            supervisor_status: PtySupervisorStatus::Running,
        }
    }

    fn create_test_session(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
    ) -> SessionId {
        let responses = send_encrypted(
            protocol,
            connection,
            device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(device_session, responses);
        let payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        payload.session_id
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
    fn session_create_assigns_stable_human_display_names() {
        let (mut protocol, _) = protocol();
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
        let first_created_payload = first_created.payload.clone();
        let first_payload: SessionCreatedPayload =
            decode_payload(first_created_payload.clone()).unwrap();
        let first_name = first_created_payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .expect("session_created should include a daemon-assigned name")
            .to_owned();

        assert!(!first_name.trim().is_empty());
        assert!(
            !first_name.starts_with("Shell "),
            "daemon-assigned names must not depend on UI list position"
        );

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
        let second_created_payload = second_created.payload.clone();
        let second_payload: SessionCreatedPayload =
            decode_payload(second_created_payload.clone()).unwrap();
        let second_name = second_created_payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .expect("second session_created should include a daemon-assigned name")
            .to_owned();

        assert_ne!(second_payload.session_id, first_payload.session_id);
        assert!(!second_name.trim().is_empty());

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();
        let names_by_id: HashMap<_, _> = list_payload
            .sessions
            .into_iter()
            .map(|session| (session.session_id, session.name))
            .collect();

        assert_eq!(
            names_by_id
                .get(&first_payload.session_id)
                .and_then(Option::as_deref),
            Some(first_name.as_str())
        );
        assert_eq!(
            names_by_id
                .get(&second_payload.session_id)
                .and_then(Option::as_deref),
            Some(second_name.as_str())
        );
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
    fn legacy_fake_daemon_public_key_migration_keeps_server_id_and_generates_static_keypair() {
        let server_id = ServerId::new();
        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: Some(DaemonIdentitySnapshot {
                server_id,
                public_key: PublicKey(format!("termd-daemon-public-{}", server_id.0)),
                private_key: None,
            }),
            trusted_devices: Vec::new(),
            sessions: Vec::new(),
        };
        let backend = FakePtyBackend::default();
        let config = DaemonConfig::default_for_state_path(temp_state_path("legacy-identity.json"));

        let protocol =
            DaemonProtocol::from_state(config, backend, Ed25519SignatureVerifier, state).unwrap();

        assert_eq!(protocol.server_id(), server_id);
        assert!(
            protocol
                .daemon_public_identity()
                .public_key
                .0
                .starts_with("ed25519-v1:")
        );
        assert_ne!(
            protocol.daemon_public_identity().public_key.0,
            format!("termd-daemon-public-{}", server_id.0)
        );
        assert!(
            protocol
                .snapshot_state()
                .daemon_identity
                .unwrap()
                .private_key
                .is_some()
        );
    }

    #[test]
    fn startup_restores_live_session_supervisor_metadata_from_sqlite() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-live-session.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();
        let root_path = std::env::temp_dir();
        let files_path = root_path.join("project");

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(40, 120),
                    None,
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
            history
                .record_session_renamed(session_id, Some("work shell"), UnixTimestampMillis(1_001))
                .unwrap();
            history
                .record_session_files_path(session_id, &files_path, UnixTimestampMillis(1_002))
                .unwrap();
        }

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(40, 120),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_002),
                restore_info: Some(socket_restore_info(session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();
        let protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();

        assert_eq!(backend.reconnects(), vec![session_id.0.to_string()]);
        assert!(protocol.session_index.contains_key(&session_id));
        assert_eq!(
            protocol.session_names.get(&session_id).map(String::as_str),
            Some("work shell")
        );
        assert_eq!(protocol.session_roots.get(&session_id), Some(&root_path));
        assert_eq!(
            protocol
                .client_history
                .session_files_path(session_id)
                .unwrap(),
            Some(files_path.to_string_lossy().to_string())
        );
        assert_eq!(
            protocol
                .snapshot_state()
                .sessions
                .iter()
                .map(|session| session.session_id)
                .collect::<Vec<_>>(),
            vec![session_id]
        );
    }

    #[test]
    fn startup_restores_reconnectable_session_without_client_history_metadata() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-without-history.json");
        let default_root = std::env::temp_dir().canonicalize().unwrap();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
        config.default_working_directory = Some(default_root.clone());
        let session_id = SessionId::new();
        let session_id_text = session_id.0.to_string();
        let default_name = format!("restored-{}", &session_id_text[..8]);

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(32, 100),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_001),
                restore_info: Some(socket_restore_info(session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let mut protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();

        assert_eq!(backend.reconnects(), vec![session_id_text]);
        assert!(protocol.session_index.contains_key(&session_id));
        assert_eq!(protocol.session_roots.get(&session_id), Some(&default_root));
        assert_eq!(
            protocol.session_names.get(&session_id).map(String::as_str),
            Some(default_name.as_str())
        );
        assert_eq!(payload.sessions.len(), 1);
        assert_eq!(payload.sessions[0].session_id, session_id);
        assert_eq!(
            payload.sessions[0].name.as_deref(),
            Some(default_name.as_str())
        );
        assert_eq!(
            payload.sessions[0].files_path.as_deref(),
            Some(default_root.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn startup_restores_live_session_metadata_from_closed_history_row() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-closed-history.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();
        let root_path = std::env::temp_dir();
        let files_path = root_path.join("closed-but-live");

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(30, 100),
                    Some("kept shell"),
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
            history
                .record_session_files_path(session_id, &files_path, UnixTimestampMillis(1_001))
                .unwrap();
            history
                .record_session_closed(session_id, UnixTimestampMillis(1_002))
                .unwrap();
        }

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(30, 100),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_003),
                restore_info: Some(socket_restore_info(session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let protocol = DaemonProtocol::from_state(
            config.clone(),
            backend.clone(),
            Ed25519SignatureVerifier,
            state,
        )
        .unwrap();

        assert_eq!(backend.reconnects(), vec![session_id.0.to_string()]);
        assert_eq!(
            protocol.session_names.get(&session_id).map(String::as_str),
            Some("kept shell")
        );
        assert_eq!(protocol.session_roots.get(&session_id), Some(&root_path));

        let restored_history = protocol
            .client_history
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|record| record.session_id == session_id)
            .unwrap();
        assert_eq!(restored_history.state, SessionState::Running);
        assert_eq!(restored_history.name.as_deref(), Some("kept shell"));
        assert_eq!(
            restored_history.files_path.as_deref(),
            Some(files_path.to_string_lossy().as_ref())
        );

        let reloaded_state = StateStore::load(&config.state_path).unwrap();
        assert_eq!(reloaded_state.sessions.len(), 1);
        assert_eq!(reloaded_state.sessions[0].session_id, session_id);
        assert_eq!(reloaded_state.sessions[0].state, SessionState::Running);
    }

    #[test]
    fn startup_repairs_closed_runtime_rows_without_losing_names_or_order() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("repair-closed-runtime-row.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let first = SessionId::new();
        let second = SessionId::new();
        let root_path = std::env::temp_dir();

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            for (index, (session_id, name)) in [(first, "first shell"), (second, "second shell")]
                .into_iter()
                .enumerate()
            {
                history
                    .record_session_created(
                        session_id,
                        SessionState::Running,
                        TerminalSize::new(24, 80),
                        Some(name),
                        &root_path,
                        UnixTimestampMillis(1_000 + index as u64),
                    )
                    .unwrap();
            }
            history
                .record_session_order(&[second, first], UnixTimestampMillis(2_000))
                .unwrap();
            // 旧安装/恢复路径可能把展示行也误标 closed；live supervisor 补回时必须保留名称和顺序。
            history
                .record_session_closed(first, UnixTimestampMillis(3_000))
                .unwrap();
            history
                .record_session_closed(second, UnixTimestampMillis(3_000))
                .unwrap();
        }

        let mut state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![
                SessionStateRecord {
                    session_id: first,
                    state: SessionState::Closed,
                    size: TerminalSize::new(24, 80),
                    created_at_ms: UnixTimestampMillis(1_000),
                    updated_at_ms: UnixTimestampMillis(3_000),
                    restore_info: None,
                },
                SessionStateRecord {
                    session_id: second,
                    state: SessionState::Closed,
                    size: TerminalSize::new(24, 80),
                    created_at_ms: UnixTimestampMillis(1_001),
                    updated_at_ms: UnixTimestampMillis(3_000),
                    restore_info: None,
                },
            ],
        };
        let supervisors = vec![
            crate::pty::supervisor::SupervisorRestoreCandidate {
                session_id: first.0.to_string(),
                socket_path: std::path::PathBuf::from(format!("/tmp/{}.sock", first.0)),
                supervisor_pid: 11,
                size: PtySize::new(24, 80),
            },
            crate::pty::supervisor::SupervisorRestoreCandidate {
                session_id: second.0.to_string(),
                socket_path: std::path::PathBuf::from(format!("/tmp/{}.sock", second.0)),
                supervisor_pid: 12,
                size: PtySize::new(24, 80),
            },
        ];
        crate::net::server::adopt_or_repair_runtime_sessions_from_supervisors(
            &mut state,
            supervisors,
            UnixTimestampMillis(4_000),
        );
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let mut protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();

        assert_eq!(
            payload
                .sessions
                .iter()
                .map(|session| (session.session_id, session.name.as_deref()))
                .collect::<Vec<_>>(),
            vec![(second, Some("second shell")), (first, Some("first shell"))]
        );
        assert_eq!(backend.reconnects().len(), 2);
        assert!(
            StateStore::load(&state_path)
                .unwrap()
                .sessions
                .iter()
                .all(|session| session.state == SessionState::Running)
        );
    }

    #[test]
    fn snapshot_keeps_live_runtime_session_when_history_row_was_deleted() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("snapshot-without-history.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();
        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_001),
                restore_info: Some(socket_restore_info(session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();
        let protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();

        let snapshot = protocol.snapshot_state();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].session_id, session_id);
        assert_eq!(snapshot.sessions[0].state, SessionState::Running);
    }

    #[test]
    fn session_reorder_is_persisted_and_drives_session_list_order() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (mut connection, _) = protocol.start_connection();
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let first = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let second = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let third = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let wanted_order = vec![third, first, second];

        let reorder_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionReorder,
                SessionReorderPayload {
                    session_ids: wanted_order.clone(),
                },
            )
            .unwrap(),
        );
        let reordered = decrypt_first(&mut device_session, reorder_responses);
        let reordered_payload: SessionReorderedPayload = decode_payload(reordered.payload).unwrap();
        assert_eq!(reordered_payload.session_ids, wanted_order);

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();
        let listed_ids = list_payload
            .sessions
            .into_iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        assert_eq!(listed_ids, wanted_order);

        let persisted_ids = protocol
            .client_history
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|record| record.session_id)
            .collect::<Vec<_>>();
        assert_eq!(persisted_ids, wanted_order);
    }

    #[test]
    fn session_reorder_repairs_live_sessions_whose_history_rows_were_closed() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (mut connection, _) = protocol.start_connection();
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let first = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let second = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let third = create_test_session(&mut protocol, &mut connection, &mut device_session);

        // 旧安装/重启路径曾把仍由 supervisor 持有的 session 展示行标成 closed。
        // 这些 session 在内存和 runtime_sessions 中仍是 live，重排必须修复展示行后再写顺序。
        for session_id in [first, second, third] {
            protocol
                .client_history
                .record_session_closed(session_id, UnixTimestampMillis(2_000))
                .unwrap();
        }

        let wanted_order = vec![third, first, second];
        let reorder_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionReorder,
                SessionReorderPayload {
                    session_ids: wanted_order.clone(),
                },
            )
            .unwrap(),
        );
        let reordered = decrypt_first(&mut device_session, reorder_responses);
        let reordered_payload: SessionReorderedPayload = decode_payload(reordered.payload).unwrap();
        assert_eq!(reordered_payload.session_ids, wanted_order);

        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
        );
        let list = decrypt_first(&mut device_session, list_responses);
        let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();
        let listed_ids = list_payload
            .sessions
            .into_iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        assert_eq!(listed_ids, wanted_order);

        let persisted_records = protocol.client_history.list_sessions().unwrap();
        assert_eq!(
            persisted_records
                .iter()
                .map(|record| record.session_id)
                .collect::<Vec<_>>(),
            wanted_order
        );
        assert!(
            persisted_records
                .iter()
                .all(|record| record.state == SessionState::Running)
        );
    }

    #[test]
    fn session_reorder_rejects_unknown_or_duplicate_ids() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (mut connection, _) = protocol.start_connection();
        let (_, mut device_session) = open_e2ee(&mut protocol, &mut connection, device_id);
        pair_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);

        let duplicate_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionReorder,
                SessionReorderPayload {
                    session_ids: vec![session_id, session_id],
                },
            )
            .unwrap(),
        );
        let duplicate_error = decrypt_first(&mut device_session, duplicate_responses);
        assert_eq!(duplicate_error.kind, MessageType::Error);

        let unknown_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionReorder,
                SessionReorderPayload {
                    session_ids: vec![SessionId::new()],
                },
            )
            .unwrap(),
        );
        let unknown_error = decrypt_first(&mut device_session, unknown_responses);
        assert_eq!(unknown_error.kind, MessageType::Error);
    }

    #[test]
    fn startup_keeps_stale_running_restore_records_pending() {
        let backend = FakePtyBackend::default();
        backend.fail_reconnects("stale supervisor socket");
        let state_path = temp_state_path("restore-stale-session.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let stale_session_id = SessionId::new();
        let closing_session_id = SessionId::new();
        let root_path = std::env::temp_dir();
        let stale_session_name = "maybe still alive";

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    stale_session_id,
                    SessionState::Running,
                    TerminalSize::new(24, 80),
                    Some(stale_session_name),
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
            history
                .record_session_created(
                    closing_session_id,
                    SessionState::Running,
                    TerminalSize::new(24, 80),
                    None,
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
        }

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![
                SessionStateRecord {
                    session_id: stale_session_id,
                    state: SessionState::Running,
                    size: TerminalSize::new(24, 80),
                    created_at_ms: UnixTimestampMillis(1_000),
                    updated_at_ms: UnixTimestampMillis(1_001),
                    restore_info: Some(socket_restore_info(stale_session_id)),
                },
                SessionStateRecord {
                    session_id: closing_session_id,
                    state: SessionState::Running,
                    size: TerminalSize::new(24, 80),
                    created_at_ms: UnixTimestampMillis(1_000),
                    updated_at_ms: UnixTimestampMillis(1_001),
                    restore_info: Some(PtyRestoreInfo::UnixSocket {
                        socket_path: std::env::temp_dir().join("termd-test-closing.sock"),
                        supervisor_pid: 43,
                        supervisor_status: PtySupervisorStatus::Closing,
                    }),
                },
            ],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let mut protocol = DaemonProtocol::from_state(
            config.clone(),
            backend.clone(),
            Ed25519SignatureVerifier,
            state,
        )
        .unwrap();

        assert!(protocol.session_index.is_empty());
        assert_eq!(
            protocol
                .client_history
                .list_sessions()
                .unwrap()
                .into_iter()
                .map(|session| session.session_id)
                .collect::<Vec<_>>(),
            vec![stale_session_id]
        );
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();
        assert_eq!(payload.sessions.len(), 1);
        assert_eq!(payload.sessions[0].session_id, stale_session_id);
        assert_eq!(
            payload.sessions[0].name.as_deref(),
            Some(stale_session_name)
        );
        assert!(!protocol.session_index.contains_key(&stale_session_id));

        backend.allow_reconnects();
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();
        assert_eq!(payload.sessions.len(), 1);
        assert_eq!(payload.sessions[0].session_id, stale_session_id);
        assert!(
            protocol.session_index.contains_key(&stale_session_id),
            "后续 socket 恢复后，list_sessions 应该自动把 pending session 接回"
        );
        assert!(
            !protocol
                .pending_restore_sessions
                .contains_key(&stale_session_id)
        );

        let reloaded_state = StateStore::load(&config.state_path).unwrap();
        let sessions_by_id: HashMap<_, _> = reloaded_state
            .sessions
            .into_iter()
            .map(|session| (session.session_id, session))
            .collect();
        let stale_session = sessions_by_id
            .get(&stale_session_id)
            .expect("unreachable running supervisor must remain retryable");
        assert_eq!(stale_session.state, SessionState::Running);
        assert!(stale_session.restore_info.is_some());
        let closing_session = sessions_by_id
            .get(&closing_session_id)
            .expect("non-running restore record should remain as closed fact");
        assert_eq!(closing_session.state, SessionState::Closed);
        assert!(closing_session.restore_info.is_none());
    }

    #[test]
    fn pending_restore_session_can_be_attached_after_socket_recovers() {
        let backend = FakePtyBackend::default();
        backend.fail_reconnects("stale supervisor socket");
        let state_path = temp_state_path("pending-attach-session.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();
        let root_path = std::env::temp_dir();

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    session_id,
                    SessionState::Running,
                    TerminalSize::new(24, 80),
                    Some("resume me"),
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
        }

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_001),
                restore_info: Some(socket_restore_info(session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let mut protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();
        assert!(protocol.session_index.is_empty());
        assert!(protocol.pending_restore_sessions.contains_key(&session_id));
        let reconnect_attempts_before = backend.reconnects().len();

        backend.allow_reconnects();
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());

        let response = protocol
            .attach_session(&mut connection, SessionAttachPayload { session_id })
            .unwrap();
        let attached: SessionAttachedPayload = decode_payload(response[0].payload.clone()).unwrap();

        assert_eq!(response[0].kind, MessageType::SessionAttached);
        assert_eq!(attached.session_id, session_id);
        assert!(protocol.session_index.contains_key(&session_id));
        assert!(!protocol.pending_restore_sessions.contains_key(&session_id));
        let reconnect_attempts_after = backend.reconnects();
        assert_eq!(
            reconnect_attempts_after.len(),
            reconnect_attempts_before + 1
        );
        assert_eq!(
            reconnect_attempts_after.last().map(String::as_str),
            Some(session_id.0.to_string().as_str())
        );
    }

    #[test]
    fn daemon_status_requires_authentication_and_returns_snapshot() {
        let (mut protocol, _) = protocol();
        let mut connection = ProtocolConnection::new(None);

        assert!(matches!(
            protocol.daemon_status(&connection, DaemonStatusPayload {}),
            Err(ProtocolError::Unauthenticated)
        ));

        connection.authenticated_device_id = Some(DeviceId::new());
        let response = protocol
            .daemon_status(&connection, DaemonStatusPayload {})
            .unwrap();
        let payload: DaemonStatusResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();

        assert_eq!(response[0].kind, MessageType::DaemonStatusResult);
        assert_eq!(payload.load_avg.len(), 3);
        assert!((0.0..=100.0).contains(&payload.cpu_percent));
        assert_eq!(payload.process_count, 0);
        let _network_bytes = (payload.network_rx_bytes, payload.network_tx_bytes);
    }

    #[test]
    fn daemon_status_network_bytes_sum_physical_interfaces_only() {
        let root = temp_state_path("sys-class-net");
        fs::create_dir_all(root.join("eth0").join("statistics")).unwrap();
        fs::write(root.join("eth0").join("device"), b"physical").unwrap();
        fs::write(
            root.join("eth0").join("statistics").join("rx_bytes"),
            b"1024\n",
        )
        .unwrap();
        fs::write(
            root.join("eth0").join("statistics").join("tx_bytes"),
            b"2048\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("wlan0").join("statistics")).unwrap();
        fs::write(root.join("wlan0").join("device"), b"physical").unwrap();
        fs::write(
            root.join("wlan0").join("statistics").join("rx_bytes"),
            b"4096\n",
        )
        .unwrap();
        fs::write(
            root.join("wlan0").join("statistics").join("tx_bytes"),
            b"8192\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("lo").join("statistics")).unwrap();
        fs::write(root.join("lo").join("device"), b"loopback").unwrap();
        fs::write(
            root.join("lo").join("statistics").join("rx_bytes"),
            b"100000\n",
        )
        .unwrap();
        fs::write(
            root.join("lo").join("statistics").join("tx_bytes"),
            b"100000\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("docker0").join("statistics")).unwrap();
        fs::write(
            root.join("docker0").join("statistics").join("rx_bytes"),
            b"500000\n",
        )
        .unwrap();
        fs::write(
            root.join("docker0").join("statistics").join("tx_bytes"),
            b"500000\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("broken0").join("statistics")).unwrap();
        fs::write(root.join("broken0").join("device"), b"physical").unwrap();
        fs::write(
            root.join("broken0").join("statistics").join("rx_bytes"),
            b"700000\n",
        )
        .unwrap();

        assert_eq!(
            read_physical_network_bytes_from_sys_class_net(&root),
            (5 * 1024, 10 * 1024)
        );
    }

    #[test]
    fn startup_does_not_restore_created_sessions_even_with_restore_info() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-created-session.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let created_session_id = SessionId::new();
        let root_path = std::env::temp_dir();

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    created_session_id,
                    SessionState::Created,
                    TerminalSize::new(24, 80),
                    None,
                    &root_path,
                    UnixTimestampMillis(1_000),
                )
                .unwrap();
        }

        let state = DaemonState {
            version: crate::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id: created_session_id,
                state: SessionState::Created,
                size: TerminalSize::new(24, 80),
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_001),
                restore_info: Some(socket_restore_info(created_session_id)),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let protocol = DaemonProtocol::from_state(
            config.clone(),
            backend.clone(),
            Ed25519SignatureVerifier,
            state,
        )
        .unwrap();

        assert!(protocol.session_index.is_empty());
        assert!(protocol.client_history.list_sessions().unwrap().is_empty());
        assert!(protocol.session_names.is_empty());
        assert!(protocol.session_roots.is_empty());
        assert!(protocol.snapshot_state().sessions.is_empty());

        let reloaded_state = StateStore::load(&config.state_path).unwrap();
        assert_eq!(reloaded_state.sessions.len(), 1);
        assert_eq!(reloaded_state.sessions[0].state, SessionState::Closed);
        assert!(reloaded_state.sessions[0].restore_info.is_none());
        assert!(backend.reconnects().is_empty());
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
    fn daemon_client_list_includes_attached_operator_cursor_and_focus() {
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
                    focused: true,
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
            payload.clients[0].attached_session_ids,
            vec![created_payload.session_id]
        );
        assert_eq!(
            payload.clients[0].cursor_session_id,
            Some(created_payload.session_id)
        );
        assert_eq!(payload.clients[0].cursor_row, Some(12));
        assert_eq!(payload.clients[0].cursor_col, Some(8));
        assert_eq!(payload.clients[0].cursor_focused, Some(true));
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
    fn forgetting_offline_daemon_client_is_idempotent() {
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
        let state_path = temp_state_path("forget-offline-client.json");
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
        let forget = envelope_value(
            MessageType::DaemonClientForget,
            DaemonClientForgetPayload {
                device_id: historical_device_id,
            },
        )
        .unwrap();

        let first_response = send_encrypted(
            &mut protocol,
            &mut inspector,
            &mut inspector_crypto,
            forget.clone(),
        );
        let first_response = decrypt_first(&mut inspector_crypto, first_response);
        let first_payload: DaemonClientForgotPayload =
            decode_payload(first_response.payload).unwrap();
        assert_eq!(first_response.kind, MessageType::DaemonClientForgot);
        assert_eq!(first_payload.device_id, historical_device_id);

        let second_response =
            send_encrypted(&mut protocol, &mut inspector, &mut inspector_crypto, forget);
        let second_response = decrypt_first(&mut inspector_crypto, second_response);
        let second_payload: DaemonClientForgotPayload =
            decode_payload(second_response.payload).unwrap();
        assert_eq!(second_response.kind, MessageType::DaemonClientForgot);
        assert_eq!(second_payload.device_id, historical_device_id);

        let list_response = send_encrypted(
            &mut protocol,
            &mut inspector,
            &mut inspector_crypto,
            envelope_value(MessageType::DaemonClients, DaemonClientsPayload {}).unwrap(),
        );
        let list_response = decrypt_first(&mut inspector_crypto, list_response);
        let list_payload: DaemonClientsResultPayload =
            decode_payload(list_response.payload).unwrap();
        assert!(
            list_payload
                .clients
                .iter()
                .all(|client| client.device_id != historical_device_id)
        );
    }

    #[test]
    fn reattached_connection_receives_plain_text_history_without_clearing_scrollback() {
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
        let replayed_snapshot = String::from_utf8(
            general_purpose::STANDARD
                .decode(replayed_data.data_base64)
                .unwrap(),
        )
        .unwrap();
        assert!(replayed_snapshot.contains("original screen"));
        assert!(!replayed_snapshot.contains("\x1b[2J\x1b[H"));
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
    fn attached_session_resize_returns_session_resized_ack() {
        let (mut protocol, _) = protocol();
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
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut device_session, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        assert!(created_payload.resize_owner);
        let resized_size = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 640,
        };

        // resize 是有状态操作；客户端必须等这个明确 ack 后才能认为 daemon 已接受尺寸。
        let resize_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: created_payload.session_id,
                    size: resized_size,
                },
            )
            .unwrap(),
        );
        let resized = decrypt_first(&mut device_session, resize_responses);
        let resized_payload: SessionResizedPayload = decode_payload(resized.payload).unwrap();

        assert_eq!(resized.kind, MessageType::SessionResized);
        assert_eq!(resized_payload.session_id, created_payload.session_id);
        assert_eq!(resized_payload.size, resized_size);
        assert!(resized_payload.resize_owner);
    }

    #[test]
    fn non_owner_attached_connection_cannot_resize_session() {
        let (mut protocol, _) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
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
        let created = decrypt_first(&mut first_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        assert!(created_payload.resize_owner);

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
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert_eq!(attached.kind, MessageType::SessionAttached);
        assert!(!attached_payload.resize_owner);

        let resize_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: created_payload.session_id,
                    size: TerminalSize {
                        rows: 40,
                        cols: 120,
                        pixel_width: 960,
                        pixel_height: 640,
                    },
                },
            )
            .unwrap(),
        );
        let resize_error = decrypt_first(&mut second_crypto, resize_responses);
        let resize_error_payload: ErrorPayload = decode_payload(resize_error.payload).unwrap();

        // 非 owner 可以继续作为 operator 输入，但不能覆盖 shared PTY 的尺寸。
        assert_eq!(resize_error.kind, MessageType::Error);
        assert_eq!(resize_error_payload.code, "invalid_state");
    }

    #[test]
    fn resize_owner_moves_to_attached_connection_after_owner_disconnect() {
        let (mut protocol, _) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
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
        let created = decrypt_first(&mut first_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

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
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert!(!attached_payload.resize_owner);

        let mut resize_signal = second_connection
            .attached_resize_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to resize owner changes");
        resize_signal.borrow_and_update();

        first_connection.close(&mut protocol);
        assert!(resize_signal.has_changed().unwrap());
        let owner_update =
            second_connection.read_session_resize_update(&mut protocol, created_payload.session_id);
        let owner_update = decrypt_first(&mut second_crypto, owner_update);
        let owner_update_payload: SessionResizedPayload =
            decode_payload(owner_update.payload).unwrap();
        assert_eq!(owner_update.kind, MessageType::SessionResized);
        assert!(owner_update_payload.resize_owner);

        let resized_size = TerminalSize {
            rows: 32,
            cols: 100,
            pixel_width: 840,
            pixel_height: 600,
        };
        let resize_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: created_payload.session_id,
                    size: resized_size,
                },
            )
            .unwrap(),
        );
        let resized = decrypt_first(&mut second_crypto, resize_responses);
        let resized_payload: SessionResizedPayload = decode_payload(resized.payload).unwrap();
        assert_eq!(resized.kind, MessageType::SessionResized);
        assert_eq!(resized_payload.size, resized_size);
        assert!(resized_payload.resize_owner);
    }

    #[test]
    fn attached_resize_signal_pushes_session_resized_to_other_connection() {
        let (mut protocol, _) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
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
        let created = decrypt_first(&mut first_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

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

        let mut resize_signal = second_connection
            .attached_resize_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to resize changes");
        resize_signal.borrow_and_update();

        let resized_size = TerminalSize {
            rows: 32,
            cols: 120,
            pixel_width: 1000,
            pixel_height: 700,
        };
        let first_ack = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionResize,
                SessionResizePayload {
                    session_id: created_payload.session_id,
                    size: resized_size,
                },
            )
            .unwrap(),
        );
        let first_ack = decrypt_first(&mut first_crypto, first_ack);
        assert_eq!(first_ack.kind, MessageType::SessionResized);
        assert!(resize_signal.has_changed().unwrap());

        let push =
            second_connection.read_session_resize_update(&mut protocol, created_payload.session_id);
        let push = decrypt_first(&mut second_crypto, push);
        let push_payload: SessionResizedPayload = decode_payload(push.payload).unwrap();
        assert_eq!(push.kind, MessageType::SessionResized);
        assert_eq!(push_payload.session_id, created_payload.session_id);
        assert_eq!(push_payload.size, resized_size);
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
        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let listed = decrypt_first(&mut device_session, list_responses);
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
    fn session_files_without_path_uses_daemon_persisted_file_tree_position() {
        let base = temp_state_path("shared-files-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("shared-files-state.json"));
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

        let (mut second_connection, _) = protocol.start_connection();
        let mut second_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut second_connection,
            device_id,
            &signing_key,
        );
        let unattached_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let unattached = decrypt_first(&mut second_crypto, unattached_responses);
        assert_eq!(unattached.kind, MessageType::Error);

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

        let second_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let second = decrypt_first(&mut second_crypto, second_responses);
        let second_payload: SessionFilesResultPayload = decode_payload(second.payload).unwrap();

        assert_eq!(second.kind, MessageType::SessionFilesResult);
        assert_eq!(second_payload.path, root.to_string_lossy());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_files_without_path_prefers_terminal_cwd_over_persisted_file_tree_position() {
        let base = temp_state_path("shared-files-cwd-base");
        let root = base.join("project");
        let manual = root.join("manual");
        let terminal_cwd = root.join("terminal-cwd");
        fs::create_dir_all(&manual).unwrap();
        fs::create_dir_all(&terminal_cwd).unwrap();
        fs::write(manual.join("manual.txt"), b"manual\n").unwrap();
        fs::write(terminal_cwd.join("cwd.txt"), b"cwd\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("shared-files-cwd-state.json"));
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

        let manual_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: Some(manual.to_string_lossy().to_string()),
                },
            )
            .unwrap(),
        );
        let manual_listed = decrypt_first(&mut device_session, manual_responses);
        assert_eq!(manual_listed.kind, MessageType::SessionFilesResult);

        backend.set_cwd_for_session(created_payload.session_id, terminal_cwd.clone());
        let cwd_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let cwd_listed = decrypt_first(&mut device_session, cwd_responses);
        let cwd_payload: SessionFilesResultPayload = decode_payload(cwd_listed.payload).unwrap();

        assert_eq!(cwd_listed.kind, MessageType::SessionFilesResult);
        assert_eq!(cwd_payload.path, terminal_cwd.to_string_lossy());
        assert!(
            cwd_payload
                .entries
                .iter()
                .any(|entry| entry.name == "cwd.txt")
        );
        assert!(
            !cwd_payload
                .entries
                .iter()
                .any(|entry| entry.name == "manual.txt")
        );

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_files_without_path_follows_terminal_cwd_outside_initial_root() {
        let base = temp_state_path("shared-files-cwd-outside-base");
        let root = base.join("project");
        let terminal_cwd = base.join("outside-cwd");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&terminal_cwd).unwrap();
        fs::write(terminal_cwd.join("outside.txt"), b"cwd outside root\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "shared-files-cwd-outside-state.json",
        ));
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

        backend.set_cwd_for_session(created_payload.session_id, terminal_cwd.clone());
        let cwd_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let cwd_listed = decrypt_first(&mut device_session, cwd_responses);
        let cwd_payload: SessionFilesResultPayload = decode_payload(cwd_listed.payload).unwrap();

        assert_eq!(cwd_listed.kind, MessageType::SessionFilesResult);
        assert_eq!(cwd_payload.path, terminal_cwd.to_string_lossy());
        assert!(
            cwd_payload
                .entries
                .iter()
                .any(|entry| entry.name == "outside.txt")
        );

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn attached_connection_receives_file_tree_update_when_another_client_changes_directory() {
        let base = temp_state_path("shared-files-push-base");
        let root = base.join("project");
        let work = base.join("work");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("beta.log"), b"sync\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("shared-files-push-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

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

        let list_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: Some(work.to_string_lossy().to_string()),
                },
            )
            .unwrap(),
        );
        let listed = decrypt_first(&mut first_crypto, list_responses);
        assert_eq!(listed.kind, MessageType::SessionFilesResult);

        let pushed = second_connection
            .read_session_file_tree_update(&mut protocol, created_payload.session_id);
        let pushed = decrypt_first(&mut second_crypto, pushed);
        let pushed_payload: SessionFilesResultPayload = decode_payload(pushed.payload).unwrap();

        assert_eq!(pushed.kind, MessageType::SessionFilesResult);
        assert_eq!(pushed_payload.path, work.to_string_lossy());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn terminal_output_pushes_file_tree_update_when_terminal_cwd_changes() {
        let base = temp_state_path("terminal-cwd-push-base");
        let root = base.join("project");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("from-cwd.txt"), b"cwd\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("terminal-cwd-push-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_crypto) = open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

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
        let mut file_tree_signal = second_connection
            .attached_file_tree_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to file tree changes");
        file_tree_signal.borrow_and_update();

        backend.set_cwd_for_session(created_payload.session_id, work.clone());
        backend.push_output_for_session(created_payload.session_id, b"$ ");
        let output_responses =
            first_connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert!(!output_responses.is_empty());
        assert!(file_tree_signal.has_changed().unwrap());

        let pushed = second_connection
            .read_session_file_tree_update(&mut protocol, created_payload.session_id);
        let pushed = decrypt_first(&mut second_crypto, pushed);
        let pushed_payload: SessionFilesResultPayload = decode_payload(pushed.payload).unwrap();

        assert_eq!(pushed.kind, MessageType::SessionFilesResult);
        assert_eq!(pushed_payload.path, work.to_string_lossy());
        assert!(
            pushed_payload
                .entries
                .iter()
                .any(|entry| entry.name == "from-cwd.txt")
        );

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_download_prepare_returns_one_time_ready_token() {
        let base = temp_state_path("download-base");
        let root = base.join("project");
        let target = root.join("artifact.log");
        fs::create_dir_all(&root).unwrap();
        fs::write(&target, b"download me\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("download-state.json"));
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

        // token 仅用于兼容旧前端的准备信号；新下载路径走 E2EE chunk，避免 HTTP 明文。
        let prepare_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileDownloadPrepare,
                SessionFileDownloadPreparePayload {
                    session_id: created_payload.session_id,
                    path: target.to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let ready = decrypt_first(&mut device_session, prepare_responses);
        let ready_payload: SessionFileDownloadReadyPayload = decode_payload(ready.payload).unwrap();

        assert_eq!(ready.kind, MessageType::SessionFileDownloadReady);
        assert_eq!(ready_payload.session_id, created_payload.session_id);
        assert_eq!(ready_payload.path, target.to_string_lossy());
        assert_eq!(ready_payload.size_bytes, 12);
        assert!(!ready_payload.token.trim().is_empty());

        let grant = protocol
            .consume_session_file_download(&ready_payload.token, current_unix_timestamp_millis())
            .unwrap();
        assert_eq!(grant.path, target);
        assert_eq!(grant.download_name, "artifact.log");
        assert_eq!(grant.size_bytes, 12);
        assert!(grant.expires_at_ms > current_unix_timestamp_millis());
        assert!(
            protocol
                .consume_session_file_download(
                    &ready_payload.token,
                    current_unix_timestamp_millis(),
                )
                .is_err(),
            "download token 被消费后不能再次使用"
        );

        let chunk_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileDownloadChunk,
                SessionFileDownloadChunkPayload {
                    session_id: created_payload.session_id,
                    path: target.to_string_lossy().to_string(),
                    offset_bytes: 0,
                    max_bytes: 5,
                },
            )
            .unwrap(),
        );
        let chunk = decrypt_first(&mut device_session, chunk_responses);
        let chunk_payload: SessionFileDownloadChunkResultPayload =
            decode_payload(chunk.payload).unwrap();

        assert_eq!(chunk.kind, MessageType::SessionFileDownloadChunkResult);
        assert_eq!(
            general_purpose::STANDARD
                .decode(chunk_payload.data_base64)
                .unwrap(),
            b"downl"
        );
        assert_eq!(chunk_payload.next_offset_bytes, 5);
        assert!(!chunk_payload.eof);
        assert!(backend.writes().is_empty());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_download_prepare_requires_attached_connection() {
        let base = temp_state_path("download-attach-base");
        let root = base.join("project");
        let target = root.join("artifact.log");
        fs::create_dir_all(&root).unwrap();
        fs::write(&target, b"download me\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("download-attach-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
        let (mut owner_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut owner_crypto) = open_e2ee(&mut protocol, &mut owner_connection, device_id);
        pair_device(
            &mut protocol,
            &mut owner_connection,
            &mut owner_crypto,
            device_id,
            public_key,
        );
        let create_responses = send_encrypted(
            &mut protocol,
            &mut owner_connection,
            &mut owner_crypto,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut owner_crypto, create_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        let (mut unattached_connection, _) = protocol.start_connection();
        let mut unattached_crypto = authenticate_paired_connection(
            &mut protocol,
            &mut unattached_connection,
            device_id,
            &signing_key,
        );
        let prepare_responses = send_encrypted(
            &mut protocol,
            &mut unattached_connection,
            &mut unattached_crypto,
            envelope_value(
                MessageType::SessionFileDownloadPrepare,
                SessionFileDownloadPreparePayload {
                    session_id: created_payload.session_id,
                    path: target.to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let prepare = decrypt_first(&mut unattached_crypto, prepare_responses);

        // legacy prepare 仍会暴露文件元数据和一次性 token，因此必须和 chunk/read/write 一样要求 attach。
        assert_eq!(prepare.kind, MessageType::Error);
        assert!(backend.writes().is_empty());

        fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_write_rejects_symlink_target_escape() {
        let base = temp_state_path("write-symlink-base");
        let root = base.join("project");
        let outside = base.join("outside");
        let outside_target = outside.join("escape.txt");
        let link_path = root.join("escape-link.txt");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(&outside_target, b"outside original\n").unwrap();
        std::os::unix::fs::symlink(&outside_target, &link_path).unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("write-symlink-state.json"));
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

        let write_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileWrite,
                SessionFileWritePayload {
                    session_id: created_payload.session_id,
                    path: link_path.to_string_lossy().to_string(),
                    data_base64: general_purpose::STANDARD.encode(b"escaped write\n"),
                },
            )
            .unwrap(),
        );
        let written = decrypt_first(&mut device_session, write_responses);

        assert_eq!(written.kind, MessageType::Error);
        assert_eq!(fs::read(&outside_target).unwrap(), b"outside original\n");
        assert!(backend.writes().is_empty());

        fs::remove_dir_all(base).ok();
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
        let list_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: Some("../outside".to_owned()),
                },
            )
            .unwrap(),
        );
        let listed = decrypt_first(&mut device_session, list_responses);
        assert_eq!(listed.kind, MessageType::Error);

        let read_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileRead,
                SessionFileReadPayload {
                    session_id: created_payload.session_id,
                    path: outside.join("readme.txt").to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let read = decrypt_first(&mut device_session, read_responses);
        let read_payload: SessionFileReadResultPayload = decode_payload(read.payload).unwrap();
        assert_eq!(read.kind, MessageType::SessionFileReadResult);
        assert_eq!(
            read_payload.path,
            outside.join("readme.txt").to_string_lossy()
        );
        assert_eq!(
            read_payload.data_base64,
            general_purpose::STANDARD.encode(b"outside file\n")
        );

        let upload_path = root.join("upload.txt");
        let write_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
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
        let written = decrypt_first(&mut device_session, write_responses);
        let written_payload: SessionFileWrittenPayload = decode_payload(written.payload).unwrap();
        assert_eq!(written.kind, MessageType::SessionFileWritten);
        assert_eq!(written_payload.size_bytes, 9);
        assert_eq!(fs::read(&upload_path).unwrap(), b"uploaded\n");

        let delete_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileDelete,
                SessionFileDeletePayload {
                    session_id: created_payload.session_id,
                    path: upload_path.to_string_lossy().to_string(),
                },
            )
            .unwrap(),
        );
        let deleted = decrypt_first(&mut device_session, delete_responses);
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
    fn attached_connection_drains_coalesced_live_output_without_waiting_for_another_signal() {
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

        // PTY 后台线程可能在 WebSocket watcher 醒来前已经累积多段输出；
        // watch 信号会合并这些变化，所以一次推送必须主动读空当前积压。
        backend.push_output(b"chunk-1\n".to_vec());
        backend.push_output(b"chunk-2\n".to_vec());
        backend.push_output(b"chunk-3\n".to_vec());

        let responses =
            connection.read_session_output(&mut protocol, created_payload.session_id, 8);
        let mut output = Vec::new();
        for response in responses {
            let inner = decrypt_first(&mut device_session, vec![response]);
            assert_eq!(inner.kind, MessageType::SessionData);
            let payload: SessionDataPayload = decode_payload(inner.payload).unwrap();
            output.extend(
                general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap(),
            );
        }

        assert_eq!(output, b"chunk-1\nchunk-2\nchunk-3\n");
    }

    #[test]
    fn session_output_history_snapshot_tracks_visible_line_refreshes() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(3, 20));

        history.append(b"alpha\nbeta\ngamma\x1b[2;1Hbravo\x1b[K");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();
        assert!(snapshot.contains("alpha\r\nbravo\r\ngamma"));
        assert!(!snapshot.contains("beta"));
    }

    #[test]
    fn session_output_history_keeps_pre_clear_text_and_visible_background_rows() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(3, 8));

        // 普通屏清屏会开启新的空白 viewport，但旧可见内容应保留为 scrollback；
        // 否则 Codex/CLI 的普通屏重绘会让刚输出的状态块在重新 attach 时少行。
        history.append(b"alpha\nbeta\n\x1b[48;5;22m\x1b[2J");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(snapshot.contains("alpha"));
        assert!(snapshot.contains("beta"));
        assert!(
            snapshot.contains("\x1b[48;5;22m        \x1b[0m"),
            "snapshot should preserve styled blank rows: {snapshot:?}"
        );
    }

    #[test]
    fn session_output_history_keeps_crlf_lines_in_text_scrollback() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(3, 20));

        // 真实 PTY 常见输出是 CRLF。单独 CR 会回到行首，后续 K 清掉旧行尾。
        history.append(b"one\r\ntwo\r\nstate: pending\rstate: done\x1b[K\r\n");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(snapshot.contains("one\r\ntwo\r\nstate: done"));
        assert!(!snapshot.contains("state: pending"));
        assert!(!snapshot.contains("\x1b[2J\x1b[H"));
    }

    #[test]
    fn session_output_history_snapshot_keeps_tail_after_wide_text_wraps() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(5, 20));

        // 中文宽字符如果按 1 列算，会比真实终端少换行，attach 清屏重绘后尾部几行会消失。
        history.append(
            "前缀：这是一段很长的中文输出，会在真实终端里自动换行\r\n\
             验证过了：\r\n\r\n\
             go test ./internal/app/shelves/pkg/deploy\r\n\r\n\
             另外，这里还有最后一行\r\n"
                .as_bytes(),
        );

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();
        assert!(snapshot.contains("验证过了："));
        assert!(snapshot.contains("go test"));
        assert!(snapshot.contains("shelves"));
        // 20 列终端里这句中文会按双宽字符自然换行，不能要求整句在快照中连续出现。
        assert!(snapshot.contains("另外，这里还有最后一"));
        assert!(snapshot.contains("行"));
        assert!(!snapshot.contains("\x1b[2J\x1b[H"));
    }

    #[test]
    fn session_output_history_keeps_status_block_before_screen_redraw() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(6, 80));

        history.append(
            "• 当前状态：\r\n\
             \r\n\
             - 工作区干净：git status --short --branch 显示 dev...origin/dev，无未提交改动。\r\n\
             - 当前 HEAD：4b70e91a 【谢一林】【fix: 修复部署方案wildcard判断异常问题】\r\n\
             \r\n\
             验证已通过：\r\n\
             \r\n\
             go test ./internal/app/shelves/pkg/deploy\r\n"
                .as_bytes(),
        );
        // 全屏 UI 随后滚动并清屏重绘时，已经滚入 scrollback 的状态块不能丢。
        history.append(b"\n\n\n\n\n\n\x1b[2J\x1b[1;1Hvisible after redraw");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(snapshot.contains("当前状态"));
        assert!(snapshot.contains("工作区干净"));
        assert!(snapshot.contains("4b70e91a"));
        assert!(snapshot.contains("go test"));
        assert!(snapshot.contains("visible after redraw"));
    }

    #[test]
    fn session_output_history_keeps_visible_status_block_when_plain_screen_clears() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(8, 100));

        // 这个形态贴近 `软件货架` session 的真实尾部：状态块还在普通屏 viewport 内，
        // 随后 Codex/CLI 用 ESC[2J 做整屏重绘。回放缓存必须把清屏前的可见行保留下来。
        history.append(
            "验证过了：\r\n\
             \r\n\
             • 当前状态：\r\n\
             \r\n\
             - 工作区干净：git status --short --branch 显示 dev...origin/dev，无未提交改动。\r\n\
             - 当前 HEAD：4b70e91a 【谢一林】【fix: 修复部署方案wildcard判断异常问题】\r\n\
             \r\n\
             go test ./internal/app/shelves/pkg/deploy\r\n"
                .as_bytes(),
        );
        history.append(b"\x1b[2J\x1b[H\x1b[48;5;236mSummarize recent commits");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(snapshot.contains("当前状态"));
        assert!(snapshot.contains("工作区干净"));
        assert!(snapshot.contains("4b70e91a"));
        assert!(snapshot.contains("go test"));
        assert!(snapshot.contains("Summarize recent commits"));
    }

    #[test]
    fn new_attach_receives_current_screen_snapshot_instead_of_raw_scrollback() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut first_device_session) =
            open_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            public_key,
        );
        let created_responses = send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            envelope_value(
                MessageType::SessionCreate,
                SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(3, 20),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        backend.push_output(b"alpha\nbeta\ngamma\x1b[2;1Hbravo\x1b[K".to_vec());

        let (mut second_connection, _) = protocol.start_connection();
        let mut second_device_session = authenticate_paired_connection(
            &mut protocol,
            &mut second_connection,
            device_id,
            &signing_key,
        );
        let attached = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let attached_inner = decrypt_first(&mut second_device_session, attached);
        assert_eq!(attached_inner.kind, MessageType::SessionAttached);

        let outputs = second_connection.read_attached_outputs(&mut protocol, 4096);
        let output_inner = decrypt_first(&mut second_device_session, outputs);
        let payload: SessionDataPayload = decode_payload(output_inner.payload).unwrap();
        let snapshot = String::from_utf8(
            general_purpose::STANDARD
                .decode(payload.data_base64)
                .unwrap(),
        )
        .unwrap();

        assert!(snapshot.contains("alpha\r\nbravo\r\ngamma"));
        assert!(!snapshot.contains("beta"));
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

        backend.push_output_for_session(first_payload.session_id, b"first output\n".to_vec());
        backend.push_output_for_session(second_payload.session_id, b"second output\n".to_vec());
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
    fn explicit_close_persists_closed_runtime_fact_and_skips_future_recovery() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("explicit-close-no-recovery.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let mut protocol =
            DaemonProtocol::new(config.clone(), backend.clone(), Ed25519SignatureVerifier).unwrap();
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

        let running_state = StateStore::load(&state_path).unwrap();
        assert_eq!(running_state.sessions.len(), 1);
        assert_eq!(running_state.sessions[0].state, SessionState::Running);
        assert!(running_state.sessions[0].restore_info.is_some());

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
        assert_eq!(closed.kind, MessageType::SessionClosed);

        let closed_state = StateStore::load(&state_path).unwrap();
        assert_eq!(closed_state.sessions.len(), 1);
        assert_eq!(
            closed_state.sessions[0].session_id,
            created_payload.session_id
        );
        assert_eq!(closed_state.sessions[0].state, SessionState::Closed);
        assert!(closed_state.sessions[0].restore_info.is_none());

        let restarted = DaemonProtocol::from_state(
            config,
            backend.clone(),
            Ed25519SignatureVerifier,
            closed_state,
        )
        .unwrap();

        assert!(restarted.session_index.is_empty());
        assert!(backend.reconnects().is_empty());
    }

    #[test]
    fn explicit_close_recovers_state_even_when_pty_terminate_fails() {
        let backend = FakePtyBackend::default();
        backend.fail_terminate("stale supervisor socket");
        let state_path = temp_state_path("explicit-close-terminate-failure.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let mut protocol =
            DaemonProtocol::new(config.clone(), backend.clone(), Ed25519SignatureVerifier).unwrap();
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
        assert_eq!(closed.kind, MessageType::SessionClosed);
        assert_eq!(backend.terminate_count(), 1);
        assert!(protocol.session_index.is_empty());

        let closed_state = StateStore::load(&state_path).unwrap();
        assert_eq!(closed_state.sessions.len(), 1);
        assert_eq!(closed_state.sessions[0].state, SessionState::Closed);
        assert!(closed_state.sessions[0].restore_info.is_none());
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
    fn command_spec_uses_configured_default_working_directory_and_term_env() {
        let mut config = DaemonConfig::default_for_state_path(temp_state_path("cwd.json"));
        config.default_command = vec!["/bin/bash".to_owned()];
        config.default_working_directory = Some(std::path::PathBuf::from("/home/termd-user"));

        let default_command = command_spec_from_payload(&[], &config).unwrap();
        let requested_command =
            command_spec_from_payload(&["/usr/bin/env".to_owned()], &config).unwrap();

        assert_eq!(default_command.program(), "/bin/bash");
        assert_eq!(
            default_command.env_map().get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            default_command.cwd_path(),
            Some(std::path::Path::new("/home/termd-user"))
        );
        assert_eq!(
            requested_command.env_map().get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            requested_command.cwd_path(),
            Some(std::path::Path::new("/home/termd-user"))
        );
    }
}
