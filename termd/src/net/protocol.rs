//! termd daemon 的 WebSocket 协议状态机核心。
//!
//! 本模块不依赖真实 socket，便于单元测试直接驱动认证和 session 操作。
//! Axum 只负责把网络帧转成这里的统一 envelope。

mod recovery;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::fs::{FileExt as WindowsFileExt, MetadataExt as WindowsMetadataExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::process::{ChildStderr, ChildStdout};
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::{
    Engine as _,
    engine::general_purpose::{self, URL_SAFE_NO_PAD},
};
use rand_core::{OsRng, RngCore};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd_proto::{
    AttachRole, AuthPayload, ClientId, ControlGrantPayload, ControlRequestPayload,
    DaemonClientForgetPayload, DaemonClientForgotPayload, DaemonClientSummaryPayload,
    DaemonClientsResultPayload, DaemonStatusResultPayload, DeviceId, Envelope,
    METHOD_CONTROL_REQUEST, METHOD_DAEMON_CLIENT_FORGET, METHOD_SESSION_CLOSE,
    METHOD_SESSION_FILE_DELETE, METHOD_SESSION_FILE_READ, METHOD_SESSION_FILE_WRITE,
    METHOD_SESSION_FILES, METHOD_SESSION_GIT, METHOD_SESSION_GIT_ACTION, METHOD_SESSION_GIT_DIFF,
    METHOD_SESSION_RENAME, METHOD_SESSION_REORDER, METHOD_SESSION_SEARCH, MessageType, ServerId,
    SessionAttachPayload, SessionAttachedPayload, SessionClosePayload, SessionClosedPayload,
    SessionCreatePayload, SessionCreatedPayload, SessionFileDeletePayload,
    SessionFileDeletedPayload, SessionFileDownloadPreparePayload, SessionFileDownloadReadyPayload,
    SessionFileDownloadStreamReadyPayload, SessionFileEntryPayload, SessionFileHttpDownloadPayload,
    SessionFileHttpUploadReadyPayload, SessionFileHttpUploadStreamPayload, SessionFileKind,
    SessionFileReadPayload, SessionFileReadResultPayload, SessionFileUploadPayload,
    SessionFileUploadProgressPayload, SessionFileWritePayload, SessionFileWrittenPayload,
    SessionFilesPayload, SessionFilesResultPayload, SessionGitActionKind, SessionGitActionPayload,
    SessionGitActionResultPayload, SessionGitDiffPayload, SessionGitDiffResultPayload,
    SessionGitFileChangePayload, SessionGitPayload, SessionGitResultPayload,
    SessionGitWorktreePayload, SessionId, SessionListPayload, SessionListResultPayload,
    SessionRenamePayload, SessionRenamedPayload, SessionReorderPayload, SessionReorderedPayload,
    SessionSearchPayload, SessionState, SessionSummaryPayload, TerminalSize, UnixTimestampMillis,
};
use termd_proto::{SessionSearchMatchPayload, SessionSearchResultPayload};
use thiserror::Error;
use tokio::sync::watch;

use crate::auth::{
    AccessTokenProofInput, AuthChallengeManager, ChallengeResponseService, CredentialKind,
    CredentialService, DaemonIdentity, DaemonPublicIdentity, DeviceIdentity,
    InMemoryTrustedDeviceStore, PairingService, PairingTokenManager, ReplayProtector,
    SignatureVerifier, TrustedDevice, TrustedDeviceStore, current_unix_timestamp_millis,
    validate_device_public_key_wire, verify_credential,
};
use crate::config::DaemonConfig;
use crate::pty::supervisor::{
    SupervisorTerminalClientFrame, decode_supervisor_terminal_client_frame,
};
use crate::pty::{
    CommandSpec, PtyAttachmentBootstrap, PtyBackend, PtyRestoreInfo, PtySize, PtySupervisorStatus,
    PtyTerminalFrame,
};
use crate::runtime::{RuntimeError, SessionRuntime};
use crate::session::{
    AttachRole as RuntimeAttachRole, SessionState as RuntimeSessionState,
    TerminalSize as RuntimeTerminalSize,
};
use crate::session_ownership::SessionOwnership;
use crate::state::{
    DaemonIdentitySnapshot, DaemonState, HttpUploadRecoveryRecord, SessionStateRecord, StateError,
    StateStore, TrustedDeviceState,
    client_history::{ClientHistoryRecord, ClientHistoryStore, SessionHistoryRecord},
};

use super::screen::TerminalScreen;
const AUTH_CHALLENGE_TTL_MS: u64 = 60_000;
const LIVE_OUTPUT_MIN_BYTES: usize = 16 * 1024;
const LIVE_OUTPUT_BYTES_PER_CELL: usize = 8;
// 中文注释：supervisor 会按 PTY read 边界生成 terminal frame，很多命令会变成
// “一行一个 frame”。live drain 不能只取几个小 frame，否则 relay/Web 会看到逐行蹦。
// 真正的上限仍由下面的 MB 级 payload/transport budget 控制。
const LIVE_OUTPUT_DRAIN_MAX_CHUNKS: usize = 512;
const TERMINAL_STREAM_BATCH_MAX_BYTES: usize = 512 * 1024;
#[cfg(test)]
#[allow(dead_code)]
const TERMINAL_STREAM_BATCH_TRANSPORT_OVERHEAD_BYTES: usize = 128;
#[cfg(test)]
#[allow(dead_code)]
const TERMINAL_STREAM_FRAME_TRANSPORT_OVERHEAD_BYTES: usize = 256;
const SESSION_TERMINAL_CWD_PROBE_MIN_INTERVAL_MS: u64 = 1_000;
const SESSION_FILE_DOWNLOAD_TOKEN_TTL_MS: u64 = 60_000;
const SESSION_FILE_DOWNLOAD_GRANT_LIMIT: usize = 128;
const SESSION_FILE_HTTP_UPLOAD_ACTIVE_IDLE_TTL_MS: u64 = 60 * 60 * 1000;
const SESSION_FILE_HTTP_UPLOAD_TOMBSTONE_TTL_MS: u64 = 10 * 60 * 1000;
#[cfg(windows)]
const SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN: u64 = u64::MAX;
// 中文注释：RPC file_read/file_write 只服务浏览器内置文本编辑器；大文件传输必须走
// HTTP upload/download stream，避免 JSON/base64 RPC 重新变成大文件通道。
const SESSION_FILE_RPC_MAX_BYTES: usize = 1024 * 1024;
const SESSION_FILE_READ_MAX_BYTES: u64 = SESSION_FILE_RPC_MAX_BYTES as u64;
const SESSION_FILE_WRITE_MAX_BYTES: usize = SESSION_FILE_RPC_MAX_BYTES;
const SESSION_FILE_WRITE_MAX_BASE64_BYTES: usize = SESSION_FILE_WRITE_MAX_BYTES.div_ceil(3) * 4;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const GIT_COMMAND_DRAIN_MAX_READS: usize = 16;
const GIT_COMMAND_STDOUT_MAX_BYTES: usize = 1024 * 1024;
const GIT_COMMAND_STDERR_MAX_BYTES: usize = 64 * 1024;
const GIT_OUTPUT_TRUNCATED_MESSAGE: &str = "git output exceeded internal limit";
const GIT_COMMAND_FAILED_MESSAGE: &str = "git command failed";
const GIT_GRAPH_TRUNCATED_MESSAGE: &str = "* [termd: git graph output truncated]";

#[cfg(test)]
fn http_upload_test_crash_checkpoint(point: &str) {
    let Some(expected) = std::env::var_os("TERMD_TEST_HTTP_UPLOAD_CHECKPOINT") else {
        return;
    };
    if expected != point {
        return;
    }
    let checkpoint_dir = PathBuf::from(
        std::env::var_os("TERMD_TEST_HTTP_UPLOAD_CHECKPOINT_DIR")
            .expect("HTTP upload crash checkpoint directory must be configured"),
    );
    fs::create_dir_all(&checkpoint_dir).unwrap();
    let marker = checkpoint_dir.join(format!("{point}.reached"));
    let release = checkpoint_dir.join(format!("{point}.continue"));
    fs::write(&marker, b"reached").unwrap();
    while !release.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(test))]
fn http_upload_test_crash_checkpoint(_point: &str) {}

/// 协议层统一使用的 JSON envelope。
pub type JsonEnvelope = Envelope<Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolWireMessage {
    Json(JsonEnvelope),
    Binary(Vec<u8>),
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileHttpUploadStatus {
    Active,
    Complete,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileHttpUploadCleanupOutcome {
    Removed,
    AlreadyGone,
    TargetReplaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileHttpUploadCleanupIdentityMode {
    InMemoryOpenHandle,
    PersistedRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileHttpUploadCleanupIdentityMatch {
    Same,
    Replaced,
}

#[derive(Debug)]
struct SessionFileHttpUploadState {
    session_id: SessionId,
    target: PathBuf,
    temp_path: PathBuf,
    file: fs::File,
    upload_id: String,
    size_bytes: u64,
    file_identity: SessionFileHttpUploadFileIdentity,
    status: SessionFileHttpUploadStatus,
    written_ranges: BTreeMap<u64, u64>,
    inflight_ranges: BTreeMap<u64, u64>,
    modified_at_ms: Option<UnixTimestampMillis>,
    published: bool,
    updated_at_ms: u64,
}

#[derive(Debug, Clone)]
struct ActiveSessionFileHttpUploadTarget {
    target: PathBuf,
    file_identity: SessionFileHttpUploadFileIdentity,
}

#[derive(Debug)]
pub(crate) struct SessionFileHttpUploadWritePlan {
    pub(crate) target: PathBuf,
    storage_path: PathBuf,
    file: fs::File,
    pub(crate) size_bytes: u64,
    pub(crate) offset_bytes: u64,
    file_identity: SessionFileHttpUploadFileIdentity,
    written_ranges: BTreeMap<u64, u64>,
    pub(crate) reserved_range: Option<(u64, u64)>,
}

#[derive(Debug)]
pub(crate) struct SessionFileHttpUploadFileWriteResult {
    written_ranges: Vec<(u64, u64)>,
    reserved_range: Option<(u64, u64)>,
    pub(crate) modified_at_ms: Option<UnixTimestampMillis>,
}

#[derive(Debug)]
pub(crate) enum SessionFileHttpUploadBegin {
    Write(SessionFileHttpUploadWritePlan),
    Complete(SessionFileUploadProgressPayload),
}

#[derive(Debug)]
pub(crate) enum SessionFileHttpUploadCommit {
    Progress(SessionFileUploadProgressPayload),
    Complete(SessionFileUploadProgressPayload),
}

impl SessionFileHttpUploadState {
    fn progress(&self, eof: bool) -> SessionFileUploadProgressPayload {
        let offset_bytes = if eof { self.size_bytes } else { 0 };
        self.progress_with_offset(offset_bytes, eof)
    }

    fn progress_with_offset(
        &self,
        offset_bytes: u64,
        eof: bool,
    ) -> SessionFileUploadProgressPayload {
        SessionFileUploadProgressPayload {
            session_id: self.session_id,
            path: absolute_path_string(&self.target),
            offset_bytes,
            size_bytes: self.size_bytes,
            eof,
            modified_at_ms: if eof { self.modified_at_ms } else { None },
        }
    }

    fn received_bytes(&self) -> Result<u64, ProtocolError> {
        self.written_ranges
            .values()
            .try_fold(0_u64, |sum, len| sum.checked_add(*len))
            .ok_or(ProtocolError::InvalidEnvelope)
    }

    fn has_complete_coverage(&self) -> Result<bool, ProtocolError> {
        if self.size_bytes == 0 {
            return Ok(true);
        }
        let mut expected_offset = 0_u64;
        for (&offset, &len) in &self.written_ranges {
            if offset != expected_offset || len == 0 {
                return Ok(false);
            }
            expected_offset = expected_offset
                .checked_add(len)
                .ok_or(ProtocolError::InvalidEnvelope)?;
        }
        Ok(expected_offset == self.size_bytes)
    }

    fn record_written_range(&mut self, start: u64, end: u64) -> Result<(), ProtocolError> {
        if start == end {
            return Ok(());
        }
        if start > end || end > self.size_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let len = end
            .checked_sub(start)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        if let Some((&previous_start, &previous_len)) =
            self.written_ranges.range(..=start).next_back()
        {
            let previous_end = previous_start
                .checked_add(previous_len)
                .ok_or(ProtocolError::InvalidEnvelope)?;
            if previous_end > start {
                // 中文注释：已确认写入的区间不能被另一段重复覆盖；完成后的重试会在
                // status=Complete 路径直接返回，避免旧请求覆盖最终文件。
                if previous_start == start && previous_end == end {
                    return Ok(());
                }
                return Err(ProtocolError::InvalidEnvelope);
            }
        }
        if let Some((&next_start, _)) = self.written_ranges.range(start..).next()
            && next_start < end
        {
            return Err(ProtocolError::InvalidEnvelope);
        }
        self.written_ranges.insert(start, len);
        Ok(())
    }

    fn reserve_write_range(
        &mut self,
        start: u64,
        len: u64,
    ) -> Result<Option<(u64, u64)>, ProtocolError> {
        if len == 0 {
            if start == self.size_bytes {
                return Ok(None);
            }
            return Err(ProtocolError::InvalidEnvelope);
        }
        let end = start
            .checked_add(len)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        if end > self.size_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }
        if range_is_fully_covered(&self.written_ranges, start, end) {
            return Ok(None);
        }
        if range_overlaps(&self.written_ranges, start, end)
            || range_overlaps(&self.inflight_ranges, start, end)
        {
            return Err(ProtocolError::InvalidState);
        }
        self.inflight_ranges.insert(start, len);
        Ok(Some((start, end)))
    }

    fn release_inflight_range(&mut self, range: Option<(u64, u64)>) -> Result<(), ProtocolError> {
        let Some((start, end)) = range else {
            return Ok(());
        };
        let expected_len = end
            .checked_sub(start)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        match self.inflight_ranges.remove(&start) {
            Some(len) if len == expected_len => Ok(()),
            _ => Err(ProtocolError::InvalidState),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionFileHttpUploadFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(not(any(unix, windows)))]
    modified_at_ms: Option<UnixTimestampMillis>,
    #[cfg(not(any(unix, windows)))]
    created_at_ms: Option<UnixTimestampMillis>,
    len: u64,
}

impl SessionFileHttpUploadFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(windows)]
            volume_serial_number: metadata.volume_serial_number(),
            #[cfg(windows)]
            file_index: metadata.file_index(),
            #[cfg(not(any(unix, windows)))]
            modified_at_ms: metadata_modified_at_ms(metadata),
            #[cfg(not(any(unix, windows)))]
            created_at_ms: metadata_created_at_ms(metadata),
            len: metadata.len(),
        }
    }

    fn dev(self) -> u64 {
        #[cfg(unix)]
        {
            self.dev
        }
        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                self.volume_serial_number
                    .map(u64::from)
                    .unwrap_or(SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN)
            }
            #[cfg(not(any(unix, windows)))]
            {
                self.modified_at_ms.map(|time| time.0).unwrap_or(0)
            }
        }
    }

    fn ino(self) -> u64 {
        #[cfg(unix)]
        {
            self.ino
        }
        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                self.file_index
                    .unwrap_or(SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN)
            }
            #[cfg(not(any(unix, windows)))]
            {
                self.created_at_ms.map(|time| time.0).unwrap_or(0)
            }
        }
    }

    fn has_stable_filesystem_object_identity(self) -> bool {
        #[cfg(unix)]
        {
            true
        }
        #[cfg(windows)]
        {
            self.volume_serial_number.is_some() && self.file_index.is_some()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    fn is_same_filesystem_object(self, other: Self) -> bool {
        #[cfg(unix)]
        {
            self.dev == other.dev && self.ino == other.ino
        }
        #[cfg(windows)]
        {
            return self.volume_serial_number.is_some()
                && self.volume_serial_number == other.volume_serial_number
                && self.file_index.is_some()
                && self.file_index == other.file_index;
        }
        #[cfg(not(any(unix, windows)))]
        {
            // 中文注释：非 Unix/Windows 平台没有稳定 inode；退回到原完整 identity。
            return self == other;
        }
    }
}

fn session_file_http_upload_cleanup_identity_match(
    actual: SessionFileHttpUploadFileIdentity,
    expected: SessionFileHttpUploadFileIdentity,
    mode: SessionFileHttpUploadCleanupIdentityMode,
) -> Result<SessionFileHttpUploadCleanupIdentityMatch, ProtocolError> {
    #[cfg(any(unix, windows))]
    {
        #[cfg(windows)]
        {
            if !actual.has_stable_filesystem_object_identity()
                || !expected.has_stable_filesystem_object_identity()
            {
                return Err(ProtocolError::InvalidState);
            }
        }
        if actual == expected {
            return Ok(SessionFileHttpUploadCleanupIdentityMatch::Same);
        }
        if actual.is_same_filesystem_object(expected) {
            return match mode {
                // 中文注释：运行中 cleanup 持有原 upload 文件句柄；同一个对象即使长度
                // 被外部改变，仍然是 daemon 创建的 active 目标，可以安全移除。
                SessionFileHttpUploadCleanupIdentityMode::InMemoryOpenHandle => {
                    Ok(SessionFileHttpUploadCleanupIdentityMatch::Same)
                }
                // 中文注释：启动 recovery 没有原文件句柄。dev/ino 相同但完整 identity
                // 不同可能是原文件被改，也可能是 inode 被复用；安全失败并保留记录。
                SessionFileHttpUploadCleanupIdentityMode::PersistedRecovery => {
                    Err(ProtocolError::InvalidState)
                }
            };
        }
        match mode {
            SessionFileHttpUploadCleanupIdentityMode::InMemoryOpenHandle => {
                Ok(SessionFileHttpUploadCleanupIdentityMatch::Replaced)
            }
            // 中文注释：启动 recovery 没有原文件句柄；只要完整 identity 不一致，
            // 就不能证明原 active 对象已经不可达，必须保留 recovery record。
            SessionFileHttpUploadCleanupIdentityMode::PersistedRecovery => {
                Err(ProtocolError::InvalidState)
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // 中文注释：非 Unix/Windows 平台没有稳定 file id，也不能可靠判断 hardlink。
        // HTTP upload cleanup 统一安全失败，避免静默删除未知对象或丢失 guard。
        let _ = (actual, expected, mode);
        Err(ProtocolError::InvalidState)
    }
}

/// HTTP 文件下载的一次性授权。
///
/// token 只存在 daemon 内存中，必须先通过已认证的 JSON HTTP 请求申请；download HTTP 层
/// 只消费 token 并流式读取文件，不接收任意路径参数。
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

    fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());
        self.screen.apply(bytes);
        self.trim_to_live_output_limit();
    }

    fn search(
        &self,
        query: &str,
        case_sensitive: bool,
        max_results: u16,
    ) -> (Vec<SessionSearchMatchPayload>, bool, u32) {
        let needle = if case_sensitive {
            query.to_owned()
        } else {
            query.to_lowercase()
        };
        let mut matches = Vec::new();
        let mut truncated = false;
        let lines = self.screen.snapshot_plain_lines();
        let line_count = lines.len().min(u32::MAX as usize) as u32;

        for (line_index, line_text) in lines.into_iter().enumerate() {
            let haystack = if case_sensitive {
                line_text.clone()
            } else {
                line_text.to_lowercase()
            };
            for column_index in match_indices(&haystack, &needle) {
                if matches.len() >= usize::from(max_results) {
                    truncated = true;
                    return (matches, truncated, line_count);
                }
                matches.push(SessionSearchMatchPayload {
                    line_index: line_index.min(u32::MAX as usize) as u32,
                    column_index: column_index.min(u16::MAX as usize) as u16,
                    line_text: line_text.clone(),
                });
            }
        }

        (matches, truncated, line_count)
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
            Self::Unauthenticated => "device must authenticate before session operations",
            Self::AuthFailed => "device authentication failed",
            Self::PairingFailed => "pairing failed",
            Self::SessionNotFound => "session was not found",
            Self::RuntimeFailed => "runtime operation failed",
            Self::StateFailed => "daemon state persistence failed",
        }
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
    pairing_service: PairingService,
    auth_service: ChallengeResponseService,
    consumed_pair_ticket_ids: HashSet<String>,
    trusted_store: InMemoryTrustedDeviceStore,
    runtime: SessionRuntime<B>,
    ownership: SessionOwnership<B>,
    verifier: V,
    session_index: HashMap<SessionId, String>,
    session_names: HashMap<SessionId, String>,
    session_roots: HashMap<SessionId, PathBuf>,
    session_terminal_cwds: HashMap<SessionId, PathBuf>,
    session_terminal_cwd_probe_notified_at_ms: HashMap<SessionId, u64>,
    session_file_downloads: HashMap<String, SessionFileDownloadGrant>,
    session_file_http_uploads: HashMap<String, SessionFileHttpUploadState>,
    daemon_clients: HashMap<DeviceId, DaemonClientRecord>,
    client_history: ClientHistoryStore,
    #[cfg(test)]
    session_output_history: HashMap<SessionId, SessionOutputHistory>,
    daemon_clients_signal: watch::Sender<u64>,
    v070_metadata_signal: watch::Sender<u64>,
    session_cwd_signals: HashMap<SessionId, watch::Sender<u64>>,
    session_resize_signals: HashMap<SessionId, watch::Sender<TerminalSize>>,
}

#[derive(Debug, Clone)]
pub enum V070TerminalOpen {
    Create(SessionCreatePayload),
    Attach(SessionAttachPayload),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct V070Cursor {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct V070TerminalSnapshot {
    pub session_id: SessionId,
    pub size: TerminalSize,
    pub cursor: V070Cursor,
}

#[derive(Debug, Clone)]
pub struct V070TerminalOpened {
    pub created: Option<SessionCreatedPayload>,
    pub attached: Option<SessionAttachedPayload>,
    pub snapshot: V070TerminalSnapshot,
}

impl<B: PtyBackend, V> Drop for DaemonProtocol<B, V> {
    fn drop(&mut self) {
        self.ownership.shutdown();
    }
}

/// session 作用域 handler 共用的已认证、已 attach 上下文。
struct AttachedSessionContext {
    device_id: DeviceId,
    internal_session_id: String,
}

/// 文件/Git handler 额外需要 session root，和基础 attach 上下文一起解析。
struct AttachedSessionRootContext {
    root: PathBuf,
}

/// 中文注释：watched attachment 替换是两阶段操作。
/// 先启动新 watcher，但只有 attach 响应和 scope grant 都构造成功后才提交替换；
/// 否则必须释放新 watcher 并恢复旧 watcher id，避免失败 attach 破坏当前输出订阅。
struct PendingWatchedAttachmentStart {
    wire_session_id: SessionId,
    attachment_id: String,
    previous_attachment_id: Option<String>,
}

impl<B, V> DaemonProtocol<B, V>
where
    B: PtyBackend,
    V: SignatureVerifier,
{
    /// 创建可测试的协议服务，调用方显式注入 PTY backend 和签名 verifier。
    pub fn new(config: DaemonConfig, backend: B, verifier: V) -> Result<Self, StateError>
    where
        B: 'static,
    {
        let daemon_identity = DaemonIdentity::generate();
        let protocol = Self::from_identity_and_store(
            config,
            backend,
            verifier,
            daemon_identity,
            InMemoryTrustedDeviceStore::new(),
        )?;
        Ok(protocol)
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
    ) -> Result<Self, StateError>
    where
        B: 'static,
    {
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
        let persisted_sessions = protocol
            .ownership
            .recover(&mut protocol.runtime, persisted_sessions)
            .map_err(|error| StateError::InvalidOwnershipState {
                source: error.to_string(),
            })?;
        protocol.restore_runtime_sessions(persisted_sessions);
        Ok(protocol)
    }

    fn from_identity_and_store(
        config: DaemonConfig,
        backend: B,
        verifier: V,
        daemon_identity: DaemonIdentity,
        trusted_store: InMemoryTrustedDeviceStore,
    ) -> Result<Self, StateError>
    where
        B: 'static,
    {
        let client_history = ClientHistoryStore::open(&config.state_path)?;
        let auth_service = ChallengeResponseService::new(
            daemon_identity.public_identity(),
            AuthChallengeManager::new(),
            ReplayProtector::default(),
        );
        let (daemon_clients_signal, _) = watch::channel(0);
        let (v070_metadata_signal, _) = watch::channel(0);
        let runtime = SessionRuntime::new(backend);
        let ownership = SessionOwnership::open(&config.state_path, runtime.backend_handle())
            .map_err(|error| StateError::InvalidOwnershipState {
                source: error.to_string(),
            })?;
        Ok(Self {
            config,
            daemon_identity,
            pairing_service: PairingService::new(PairingTokenManager::new()),
            auth_service,
            consumed_pair_ticket_ids: HashSet::new(),
            trusted_store,
            runtime,
            ownership,
            verifier,
            session_index: HashMap::new(),
            session_names: HashMap::new(),
            session_roots: HashMap::new(),
            session_terminal_cwds: HashMap::new(),
            session_terminal_cwd_probe_notified_at_ms: HashMap::new(),
            session_file_downloads: HashMap::new(),
            session_file_http_uploads: HashMap::new(),
            daemon_clients: HashMap::new(),
            client_history,
            #[cfg(test)]
            session_output_history: HashMap::new(),
            daemon_clients_signal,
            v070_metadata_signal,
            session_cwd_signals: HashMap::new(),
            session_resize_signals: HashMap::new(),
        })
    }

    /// 生成可写入本地 SQLite 的最小状态快照。
    ///
    /// 不保存 pairing token、auth challenge、access token、PTY 输出或终端输入。
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

    /// 清理不可恢复的 closed session；仍有 live supervisor 的 session id 不允许删除。
    pub(crate) fn prune_closed_sessions_except(
        &mut self,
        protected_session_ids: &HashSet<SessionId>,
    ) -> Result<usize, StateError> {
        let history_deleted = self
            .client_history
            .prune_closed_sessions_except(protected_session_ids)?;
        let runtime_deleted = StateStore::prune_closed_sessions_except(
            &self.config.state_path,
            protected_session_ids,
        )?;
        Ok(history_deleted + runtime_deleted)
    }

    pub fn server_id(&self) -> ServerId {
        self.daemon_identity.server_id()
    }

    pub fn daemon_public_identity(&self) -> &DaemonPublicIdentity {
        self.auth_service.daemon_public_identity()
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

    pub fn issue_pair_ticket_credential(
        &self,
        now_ms: UnixTimestampMillis,
    ) -> Result<(String, UnixTimestampMillis), ProtocolError> {
        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(self.config.pairing_token_ttl_ms)
                .ok_or(ProtocolError::AuthFailed)?,
        );
        let ticket = CredentialService::new(self.daemon_identity.clone())
            .issue_pair_ticket(now_ms, expires_at_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok((ticket, expires_at_ms))
    }

    pub fn pair_device_certificate(
        &mut self,
        pair_ticket: &str,
        device_id: DeviceId,
        device_public_key: termd_proto::PublicKey,
        now_ms: UnixTimestampMillis,
    ) -> Result<String, ProtocolError> {
        let claims = verify_credential(
            pair_ticket,
            self.daemon_identity.public_key(),
            self.server_id(),
            now_ms,
            CredentialKind::PairTicket,
        )
        .map_err(|_| ProtocolError::PairingFailed)?;
        if self
            .consumed_pair_ticket_ids
            .contains(&claims.credential_id)
        {
            return Err(ProtocolError::PairingFailed);
        }
        validate_device_public_key_wire(&device_public_key)
            .map_err(|_| ProtocolError::PairingFailed)?;
        self.trusted_store.trust_device(
            DeviceIdentity::new(device_id, device_public_key.clone()),
            now_ms,
            None,
        );
        self.persist_state()?;
        self.consumed_pair_ticket_ids.insert(claims.credential_id);
        CredentialService::new(self.daemon_identity.clone())
            .issue_device_certificate(device_id, device_public_key, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)
    }

    pub fn issue_device_certificate_migration_challenge(
        &mut self,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
    ) -> Result<termd_proto::AuthChallengePayload, ProtocolError> {
        self.trusted_store
            .require_trusted(&device_id)
            .map_err(|_| ProtocolError::AuthFailed)?;
        let challenge = self
            .auth_service
            .issue_challenge(device_id, now_ms, AUTH_CHALLENGE_TTL_MS)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(termd_proto::AuthChallengePayload {
            device_id,
            challenge: challenge.challenge().clone(),
            expires_at_ms: challenge.expires_at_ms(),
        })
    }

    pub fn migrate_device_certificate(
        &mut self,
        payload: AuthPayload,
        now_ms: UnixTimestampMillis,
    ) -> Result<String, ProtocolError> {
        self.auth_service
            .challenge_manager_mut()
            .consume(&payload.device_id, &payload.challenge, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.auth_service
            .replay_protector_mut()
            .check(
                &payload.device_id,
                &payload.nonce,
                payload.timestamp_ms,
                now_ms,
            )
            .map_err(|_| ProtocolError::AuthFailed)?;
        let trusted = self
            .trusted_store
            .require_trusted(&payload.device_id)
            .map_err(|_| ProtocolError::AuthFailed)?
            .clone();
        self.verifier
            .verify(
                trusted.public_key(),
                &AccessTokenProofInput {
                    server_id: self.server_id(),
                    payload: &payload,
                }
                .to_bytes(),
                &payload.signature,
            )
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.auth_service.replay_protector_mut().record_checked(
            &payload.device_id,
            &payload.nonce,
            now_ms,
        );
        self.trusted_store
            .mark_seen(&payload.device_id, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        CredentialService::new(self.daemon_identity.clone())
            .issue_device_certificate(payload.device_id, trusted.public_key().clone(), now_ms)
            .map_err(|_| ProtocolError::AuthFailed)
    }

    fn device_certificate_claims(
        &self,
        certificate: &str,
        now_ms: UnixTimestampMillis,
    ) -> Result<crate::auth::CredentialClaims, ProtocolError> {
        let claims = verify_credential(
            certificate,
            self.daemon_identity.public_key(),
            self.server_id(),
            now_ms,
            CredentialKind::DeviceCertificate,
        )
        .map_err(|_| ProtocolError::AuthFailed)?;
        let device_id = claims.device_id.ok_or(ProtocolError::AuthFailed)?;
        let device_public_key = claims
            .device_public_key
            .as_ref()
            .ok_or(ProtocolError::AuthFailed)?;
        self.trusted_store
            .require_trusted_identity(&DeviceIdentity::new(device_id, device_public_key.clone()))
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(claims)
    }

    pub fn issue_access_token_challenge(
        &mut self,
        certificate: &str,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
    ) -> Result<termd_proto::AuthChallengePayload, ProtocolError> {
        let claims = self.device_certificate_claims(certificate, now_ms)?;
        if claims.device_id != Some(device_id) {
            return Err(ProtocolError::AuthFailed);
        }
        let challenge = self
            .auth_service
            .issue_challenge(device_id, now_ms, AUTH_CHALLENGE_TTL_MS)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(termd_proto::AuthChallengePayload {
            device_id,
            challenge: challenge.challenge().clone(),
            expires_at_ms: challenge.expires_at_ms(),
        })
    }

    pub fn exchange_access_token(
        &mut self,
        certificate: &str,
        payload: AuthPayload,
        now_ms: UnixTimestampMillis,
    ) -> Result<(String, UnixTimestampMillis), ProtocolError> {
        let claims = self.device_certificate_claims(certificate, now_ms)?;
        if claims.device_id != Some(payload.device_id) {
            return Err(ProtocolError::AuthFailed);
        }
        self.auth_service
            .challenge_manager_mut()
            .consume(&payload.device_id, &payload.challenge, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.auth_service
            .replay_protector_mut()
            .check(
                &payload.device_id,
                &payload.nonce,
                payload.timestamp_ms,
                now_ms,
            )
            .map_err(|_| ProtocolError::AuthFailed)?;
        let trusted = self
            .trusted_store
            .require_trusted(&payload.device_id)
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.verifier
            .verify(
                trusted.public_key(),
                &AccessTokenProofInput {
                    server_id: self.server_id(),
                    payload: &payload,
                }
                .to_bytes(),
                &payload.signature,
            )
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.auth_service.replay_protector_mut().record_checked(
            &payload.device_id,
            &payload.nonce,
            now_ms,
        );
        self.trusted_store
            .mark_seen(&payload.device_id, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        let expires_at_ms = UnixTimestampMillis(
            now_ms
                .0
                .checked_add(300_000)
                .ok_or(ProtocolError::AuthFailed)?,
        );
        let token = CredentialService::new(self.daemon_identity.clone())
            .issue_access_token(payload.device_id, now_ms, expires_at_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok((token, expires_at_ms))
    }

    pub fn verify_access_token_credential(
        &self,
        token: &str,
        now_ms: UnixTimestampMillis,
    ) -> Result<DeviceId, ProtocolError> {
        let claims = verify_credential(
            token,
            self.daemon_identity.public_key(),
            self.server_id(),
            now_ms,
            CredentialKind::AccessToken,
        )
        .map_err(|_| ProtocolError::AuthFailed)?;
        let device_id = claims.device_id.ok_or(ProtocolError::AuthFailed)?;
        self.trusted_store
            .require_trusted(&device_id)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(device_id)
    }

    pub fn open_v070_terminal(
        &mut self,
        connection: &mut ProtocolConnection,
        open: V070TerminalOpen,
    ) -> Result<V070TerminalOpened, ProtocolError> {
        let (created, attached) = match open {
            V070TerminalOpen::Create(payload) => {
                let responses = self.create_terminal_stream_session(connection, payload)?;
                let created: SessionCreatedPayload = responses
                    .into_iter()
                    .find(|response| response.kind == MessageType::SessionCreated)
                    .ok_or(ProtocolError::InvalidEnvelope)
                    .and_then(|response| decode_payload(response.payload))?;
                (Some(created), None)
            }
            V070TerminalOpen::Attach(payload) => {
                let responses = self.attach_terminal_session(connection, payload)?;
                let attached: SessionAttachedPayload = responses
                    .into_iter()
                    .find(|response| response.kind == MessageType::SessionAttached)
                    .ok_or(ProtocolError::InvalidEnvelope)
                    .and_then(|response| decode_payload(response.payload))?;
                (None, Some(attached))
            }
        };
        let (session_id, size) = created
            .as_ref()
            .map(|payload| (payload.session_id, payload.size))
            .or_else(|| {
                attached
                    .as_ref()
                    .map(|payload| (payload.session_id, payload.size))
            })
            .ok_or(ProtocolError::InvalidEnvelope)?;
        let cursor = self.v070_terminal_cursor(session_id, size);
        Ok(V070TerminalOpened {
            created,
            attached,
            snapshot: V070TerminalSnapshot {
                session_id,
                size,
                cursor,
            },
        })
    }

    fn v070_terminal_cursor(&mut self, session_id: SessionId, size: TerminalSize) -> V070Cursor {
        let Some(internal_session_id) = self.session_index.get(&session_id).cloned() else {
            return V070Cursor { row: 1, col: 1 };
        };
        let Ok(frames) = self.runtime.terminal_snapshot(&internal_session_id, None) else {
            return V070Cursor { row: 1, col: 1 };
        };
        let mut screen = TerminalScreen::new(size.rows, size.cols);
        for frame in frames {
            match frame {
                PtyTerminalFrame::Snapshot { size, data, .. } => {
                    screen = TerminalScreen::new(size.rows, size.cols);
                    screen.apply(&data);
                }
                PtyTerminalFrame::Output { data, .. } => screen.apply(&data),
                PtyTerminalFrame::Resize { size, .. } => screen.resize(size.rows, size.cols),
                PtyTerminalFrame::Exit { .. } => {}
            }
        }
        let (row, col) = screen.cursor_position();
        V070Cursor { row, col }
    }

    pub fn v070_metadata_payload(&mut self, device_id: DeviceId) -> Result<Value, ProtocolError> {
        let connection = ProtocolConnection::authenticated_http(device_id);
        let sessions = self
            .list_sessions(&connection, SessionListPayload::default())?
            .into_iter()
            .next()
            .ok_or(ProtocolError::InvalidEnvelope)
            .and_then(|message| decode_payload::<SessionListResultPayload>(message.payload))?;
        let clients = self.daemon_clients_snapshot_payload();
        let cwd = self
            .session_terminal_cwds
            .iter()
            .map(|(session_id, path)| {
                (session_id.0.to_string(), path.to_string_lossy().to_string())
            })
            .collect::<BTreeMap<_, _>>();
        Ok(serde_json::json!({
            "sessions": sessions.sessions,
            "clients": clients.clients,
            "daemon": collect_daemon_status(),
            "cwd": cwd,
            "rtt_ms": null,
        }))
    }

    pub fn v070_metadata_signal(&self) -> watch::Receiver<u64> {
        self.v070_metadata_signal.subscribe()
    }

    fn notify_v070_metadata_changed(&self) {
        let next = self.v070_metadata_signal.borrow().saturating_add(1);
        self.v070_metadata_signal.send_replace(next);
    }

    /// HTTP control plane 每次请求都会重建一条临时 connection scope。
    ///
    /// 中文注释：这里只恢复 session 作用域，不创建 watched terminal attachment，
    /// 也不把任何 terminal output 订阅绑到 HTTP 短连接上。
    pub fn restore_http_control_scope(
        &mut self,
        connection: &mut ProtocolConnection,
        session_id: SessionId,
    ) -> Result<(), ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .attach(&internal_session_id, device_key(device_id))
            .map_err(map_runtime_error)?;
        connection.attach(session_id);
        connection.state = ProtocolConnectionState::Attached;
        Ok(())
    }

    fn create_terminal_stream_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionCreatePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.create_session_inner(connection, payload, true)
    }

    fn create_session_inner(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionCreatePayload,
        enqueue_terminal_snapshot: bool,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let command = command_spec_from_payload(&payload.command, &self.config)?;
        let session_root = session_root_from_command(&command)?;
        let runtime_size = proto_size_to_runtime(payload.size);
        let wire_session_id = SessionId::new();
        let internal_session_id = wire_session_id.0.to_string();
        let pty_size = PtySize::with_pixels(
            runtime_size.rows,
            runtime_size.cols,
            runtime_size.pixel_width,
            runtime_size.pixel_height,
        );
        let session_name = self.default_created_session_name(wire_session_id);
        let now_ms = current_unix_timestamp_millis();
        let attachment_id = connection.allocate_watched_attachment_id(wire_session_id);
        let response_name = session_name.clone();
        let client_history = &mut self.client_history;
        let prepared = self.ownership.create(
            &mut self.runtime,
            &internal_session_id,
            command,
            pty_size,
            |runtime| {
                let role = runtime.attach(&internal_session_id, device_key(device_id))?;
                runtime.start_watched_attachment(
                    &internal_session_id,
                    &attachment_id,
                    runtime_size,
                    PtyAttachmentBootstrap::default(),
                )?;
                let response = envelope_value(
                    MessageType::SessionCreated,
                    SessionCreatedPayload {
                        session_id: wire_session_id,
                        name: Some(response_name),
                        role: runtime_role_to_proto(role),
                        state: SessionState::Running,
                        size: payload.size,
                        resize_owner: true,
                    },
                )
                .map_err(|_| crate::session_ownership::OwnershipError::Preparation)?;
                client_history.record_session_created(
                    wire_session_id,
                    SessionState::Running,
                    payload.size,
                    Some(session_name.as_str()),
                    &session_root,
                    now_ms,
                )?;
                client_history.record_session_runtime_state(
                    wire_session_id,
                    SessionState::Running,
                    payload.size,
                    now_ms,
                )?;
                Ok((response, attachment_id))
            },
        );
        let (response_envelope, attachment_id) = match prepared {
            Ok(prepared) => prepared,
            Err(_) => {
                let _ = self
                    .client_history
                    .record_session_closed(wire_session_id, current_unix_timestamp_millis());
                let _ = self.client_history.prune_closed_session(wire_session_id);
                return Err(ProtocolError::StateFailed);
            }
        };

        self.session_index
            .insert(wire_session_id, internal_session_id.clone());
        self.session_names.insert(wire_session_id, session_name);
        self.session_roots.insert(wire_session_id, session_root);
        let (cwd_signal, _) = watch::channel(0);
        self.session_cwd_signals.insert(wire_session_id, cwd_signal);
        let (resize_signal, _) = watch::channel(payload.size);
        self.session_resize_signals
            .insert(wire_session_id, resize_signal);
        let _ = enqueue_terminal_snapshot;
        connection.attach(wire_session_id);
        connection.remember_watched_attachment(wire_session_id, attachment_id);
        self.record_daemon_client_attach(wire_session_id, connection, device_id);
        connection.state = ProtocolConnectionState::Attached;
        self.notify_v070_metadata_changed();
        crate::session_ownership::test_crash_checkpoint("before_create_response");
        Ok(vec![response_envelope])
    }

    pub(crate) fn attach_terminal_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionAttachPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.reconcile_persisted_closed_sessions()?;
        let device_id = connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&payload.session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        let state_before_attach = self.runtime_state_proto(&internal_session_id)?;
        let runtime_device_key = device_key(device_id);
        let was_runtime_attached = self
            .runtime
            .role(&internal_session_id, &runtime_device_key)
            .map_err(map_runtime_error)?
            .is_some();
        let role = self
            .runtime
            .attach(&internal_session_id, runtime_device_key.clone())
            .map_err(map_runtime_error)?;
        let wire_role = runtime_role_to_proto(role);
        let attach_result =
            (|| -> Result<(TerminalSize, u64, Vec<u8>, SessionState), ProtocolError> {
                let response_size = self.runtime_size_proto(&internal_session_id)?;
                let (output_offset, initial_output) = (0, Vec::new());
                let response_state = self.runtime_state_proto(&internal_session_id)?;
                self.client_history.record_session_runtime_state(
                    payload.session_id,
                    response_state,
                    response_size,
                    current_unix_timestamp_millis(),
                )?;
                if state_before_attach != response_state {
                    self.persist_state()?;
                }
                Ok((response_size, output_offset, initial_output, response_state))
            })();
        let (response_size, output_offset, initial_output, response_state) = match attach_result {
            Ok(result) => result,
            Err(error) => {
                if !was_runtime_attached {
                    let _ = self
                        .runtime
                        .detach(&internal_session_id, &runtime_device_key);
                }
                return Err(error);
            }
        };
        let pending_watched_attachment = match self.start_watched_attachment(
            connection,
            payload.session_id,
            &internal_session_id,
            response_size,
            PtyAttachmentBootstrap {
                last_terminal_seq: payload.last_terminal_seq,
            },
        ) {
            Ok(attachment_id) => attachment_id,
            Err(error) => {
                if !was_runtime_attached {
                    let _ = self
                        .runtime
                        .detach(&internal_session_id, &runtime_device_key);
                }
                return Err(error);
            }
        };
        let response = SessionAttachedPayload {
            session_id: payload.session_id,
            role: wire_role,
            state: response_state,
            size: response_size,
            resize_owner: true,
        };
        let responses = match (|| -> Result<Vec<JsonEnvelope>, ProtocolError> {
            let response_envelope = envelope_value(MessageType::SessionAttached, response)?;
            Ok(vec![response_envelope])
        })() {
            Ok(responses) => responses,
            Err(error) => {
                // 中文注释：scope grant / response 构造失败虽然罕见，但此时客户端会收到错误；
                // 连接级 watched attachment 替换和本次新增的 runtime operator 都必须一起撤销。
                self.rollback_watched_attachment_start(connection, pending_watched_attachment);
                if !was_runtime_attached {
                    let _ = self
                        .runtime
                        .detach(&internal_session_id, &runtime_device_key);
                }
                return Err(error);
            }
        };

        let _ = (output_offset, initial_output);
        connection.attach(payload.session_id);
        self.commit_watched_attachment_start(connection, pending_watched_attachment);
        self.record_daemon_client_attach(payload.session_id, connection, device_id);
        connection.state = ProtocolConnectionState::Attached;

        Ok(responses)
    }

    fn require_attached_session(
        &self,
        connection: &ProtocolConnection,
        session_id: SessionId,
    ) -> Result<AttachedSessionContext, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        // 中文注释：多数 session RPC 的旧兼容顺序是先确认当前连接已 attach，
        // 再暴露 session 是否存在，避免 authenticated-unattached 连接探测 session id。
        connection.ensure_attached_to(session_id)?;
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;

        Ok(AttachedSessionContext {
            device_id,
            internal_session_id,
        })
    }

    fn require_existing_attached_session(
        &self,
        connection: &ProtocolConnection,
        session_id: SessionId,
    ) -> Result<AttachedSessionContext, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        // 中文注释：输入、resize 和旧 control 路径历史上先解析 session，再检查 attach；
        // 这里保留该错误优先级，同时仍禁止未 attach 的当前连接操作 session。
        connection.ensure_attached_to(session_id)?;

        Ok(AttachedSessionContext {
            device_id,
            internal_session_id,
        })
    }

    fn require_attached_session_root(
        &self,
        connection: &ProtocolConnection,
        session_id: SessionId,
    ) -> Result<AttachedSessionRootContext, ProtocolError> {
        self.require_attached_session(connection, session_id)?;
        let root = self
            .session_roots
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;

        Ok(AttachedSessionRootContext { root })
    }

    fn rename_session(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionRenamePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.require_attached_session(connection, payload.session_id)?;
        let name = sanitize_session_name(payload.name)?;
        self.session_names.insert(payload.session_id, name.clone());
        self.client_history.record_session_renamed(
            payload.session_id,
            Some(&name),
            current_unix_timestamp_millis(),
        )?;
        self.notify_v070_metadata_changed();

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
        self.notify_v070_metadata_changed();

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
        let _attached = self.require_attached_session(connection, payload.session_id)?;
        self.ownership
            .close(&mut self.runtime, &payload.session_id.0.to_string())
            .map_err(|_| ProtocolError::StateFailed)?;
        self.close_visible_session_state(payload.session_id);
        let now_ms = current_unix_timestamp_millis();
        let _ = self
            .client_history
            .record_session_closed(payload.session_id, now_ms);
        let _ = self
            .client_history
            .remove_session_attachments(payload.session_id);
        let _ = self.client_history.prune_closed_session(payload.session_id);
        self.notify_v070_metadata_changed();

        Ok(vec![envelope_value(
            MessageType::SessionClosed,
            SessionClosedPayload {
                session_id: payload.session_id,
            },
        )?])
    }

    fn reconcile_persisted_closed_sessions(&mut self) -> Result<(), ProtocolError> {
        let active = StateStore::load(&self.config.state_path)?
            .sessions
            .into_iter()
            .filter(|record| record.state == SessionState::Running)
            .map(|record| record.session_id)
            .collect::<HashSet<_>>();
        let closed = self
            .session_index
            .keys()
            .filter(|session_id| !active.contains(session_id))
            .copied()
            .collect::<Vec<_>>();
        for session_id in closed {
            self.close_visible_session_state(session_id);
            let _ = self
                .client_history
                .record_session_closed(session_id, current_unix_timestamp_millis());
            let _ = self.client_history.remove_session_attachments(session_id);
        }
        Ok(())
    }

    fn close_visible_session_state(&mut self, session_id: SessionId) {
        self.session_index.remove(&session_id);
        self.session_names.remove(&session_id);
        self.session_roots.remove(&session_id);
        self.session_terminal_cwds.remove(&session_id);
        self.session_terminal_cwd_probe_notified_at_ms
            .remove(&session_id);
        #[cfg(test)]
        self.session_output_history.remove(&session_id);
        self.session_cwd_signals.remove(&session_id);
        self.session_resize_signals.remove(&session_id);
        for record in self.daemon_clients.values_mut() {
            for sessions in record.active_connections.values_mut() {
                sessions.remove(&session_id);
            }
        }
    }

    fn search_session_output(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionSearchPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let attached = self.require_attached_session(connection, payload.session_id)?;
        let query = payload.query.trim();
        if query.is_empty() || query.chars().any(char::is_control) {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let snapshot = self
            .runtime
            .snapshot(&attached.internal_session_id)
            .map_err(map_runtime_error)?;
        let size = self.runtime_size_proto(&attached.internal_session_id)?;
        let mut history = SessionOutputHistory::new(size);
        history.append(&snapshot.retained_output);
        let max_results = payload.max_results.unwrap_or(80).clamp(1, 500);
        let (matches, truncated, line_count) =
            history.search(query, payload.case_sensitive, max_results);

        Ok(vec![envelope_value(
            MessageType::SessionSearchResult,
            SessionSearchResultPayload {
                session_id: payload.session_id,
                query: query.to_owned(),
                line_count,
                matches,
                truncated,
            },
        )?])
    }

    fn list_session_files(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFilesPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.require_attached_session(connection, payload.session_id)?;

        let has_explicit_path = payload
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .is_some();
        let refreshed_cwd = self.refresh_session_terminal_cwd(payload.session_id)?;
        let requested_path = if has_explicit_path {
            payload.path.clone()
        } else {
            self.default_session_files_path_after_refresh(
                payload.session_id,
                refreshed_cwd.clone(),
            )?
        };
        let result = self.session_files_result_after_refresh(
            payload.session_id,
            requested_path,
            !has_explicit_path,
            refreshed_cwd,
        )?;
        Ok(vec![envelope_value(
            MessageType::SessionFilesResult,
            result,
        )?])
    }

    fn list_session_git(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionGitPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.require_attached_session(connection, payload.session_id)?;

        let result = self.session_git_result(payload.session_id)?;
        Ok(vec![envelope_value(MessageType::SessionGitResult, result)?])
    }

    fn apply_session_git_action(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionGitActionPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.require_attached_session(connection, payload.session_id)?;

        validate_git_relative_file_path(&payload.file_path)?;
        let worktree =
            self.session_git_worktree_path(payload.session_id, &payload.worktree_path)?;
        self.ensure_no_active_session_file_http_upload_target_in_git_scope(
            payload.session_id,
            &worktree,
            Some(&payload.file_path),
        )?;
        apply_git_file_action(&worktree, &payload.file_path, payload.action)?;

        Ok(vec![envelope_value(
            MessageType::SessionGitActionResult,
            SessionGitActionResultPayload {
                session_id: payload.session_id,
                worktree_path: absolute_path_string(&worktree),
                file_path: payload.file_path,
                action: payload.action,
            },
        )?])
    }

    fn read_session_git_diff(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionGitDiffPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.require_attached_session(connection, payload.session_id)?;
        let file_path = payload
            .file_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        if let Some(path) = file_path {
            validate_git_relative_file_path(path)?;
        }
        let worktree =
            self.session_git_worktree_path(payload.session_id, &payload.worktree_path)?;
        if let Some(path) = file_path {
            self.ensure_no_active_session_file_http_upload_target_in_git_scope(
                payload.session_id,
                &worktree,
                Some(path),
            )?;
        } else {
            self.ensure_no_active_session_file_http_upload_target_in_git_scope(
                payload.session_id,
                &worktree,
                None,
            )?;
        }
        let diff = read_git_diff(&worktree, file_path, payload.staged)?;

        Ok(vec![envelope_value(
            MessageType::SessionGitDiffResult,
            SessionGitDiffResultPayload {
                session_id: payload.session_id,
                worktree_path: absolute_path_string(&worktree),
                file_path: file_path.map(ToOwned::to_owned),
                staged: payload.staged,
                diff,
            },
        )?])
    }

    fn read_session_file(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileReadPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_existing_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        let metadata = fs::metadata(&target).map_err(map_file_path_error)?;
        if metadata.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let max_bytes = payload
            .max_bytes
            .unwrap_or(SESSION_FILE_READ_MAX_BYTES)
            .min(SESSION_FILE_READ_MAX_BYTES);
        if metadata.len() > max_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let file = fs::File::open(&target).map_err(map_file_path_error)?;
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(map_file_path_error)?;
        if bytes.len() as u64 > max_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }

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
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        if payload.data_base64.len() > SESSION_FILE_WRITE_MAX_BASE64_BYTES
            || base64_payload_decoded_len(&payload.data_base64) > SESSION_FILE_WRITE_MAX_BYTES
        {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let bytes = general_purpose::STANDARD
            .decode(payload.data_base64)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;
        if bytes.len() > SESSION_FILE_WRITE_MAX_BYTES {
            return Err(ProtocolError::InvalidEnvelope);
        }

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
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
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

    pub fn prepare_session_file_http_upload(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileUploadPayload,
        _device_id: DeviceId,
    ) -> Result<SessionFileHttpUploadReadyPayload, ProtocolError> {
        self.prune_session_file_http_uploads();
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        if target.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let upload_id = session_file_http_upload_id();
        if fs::symlink_metadata(&target).is_ok() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let temp_path = session_file_http_upload_temp_path(&target, &upload_id)?;
        let mut recovery = HttpUploadRecoveryRecord {
            upload_id: upload_id.clone(),
            target_path: temp_path.clone(),
            size_bytes: payload.size_bytes,
            dev: 0,
            ino: 0,
            updated_at_ms: current_unix_timestamp_millis(),
        };
        StateStore::record_http_upload(&self.config.state_path, &recovery)
            .map_err(|_| ProtocolError::StateFailed)?;
        http_upload_test_crash_checkpoint("after_intent");
        let (file, file_identity) =
            match create_session_file_http_upload_target(&temp_path, payload.size_bytes) {
                Ok(created) => created,
                Err(error) => {
                    let _ = StateStore::remove_http_upload(&self.config.state_path, &upload_id);
                    return Err(error);
                }
            };
        http_upload_test_crash_checkpoint("after_temp_create");
        recovery.dev = file_identity.dev();
        recovery.ino = file_identity.ino();
        recovery.updated_at_ms = current_unix_timestamp_millis();
        if let Err(error) = StateStore::record_http_upload(&self.config.state_path, &recovery) {
            tracing::debug!(%error, "failed to persist HTTP upload recovery record detail");
            tracing::warn!("failed to persist HTTP upload recovery record");
            let cleanup = remove_session_file_http_upload_target(&temp_path, file_identity);
            let keep_guard = match cleanup {
                Ok(SessionFileHttpUploadCleanupOutcome::Removed) => false,
                Ok(
                    SessionFileHttpUploadCleanupOutcome::AlreadyGone
                    | SessionFileHttpUploadCleanupOutcome::TargetReplaced,
                ) => session_file_http_upload_open_file_has_remaining_links(&file, file_identity),
                Err(cleanup_error) => {
                    tracing::debug!(
                        %cleanup_error,
                        target = %temp_path.display(),
                        "failed to cleanup HTTP upload target after recovery persist failure detail"
                    );
                    true
                }
            };
            if keep_guard {
                let now_ms = current_unix_timestamp_millis().0;
                // 中文注释：recovery record 持久化失败后，如果目标清理也不能证明
                // 未完成对象已不可达，仍要保留内存 guard，避免当前 daemon 生命周期内暴露。
                self.session_file_http_uploads.insert(
                    upload_id.clone(),
                    SessionFileHttpUploadState {
                        session_id: payload.session_id,
                        target: target.clone(),
                        temp_path: temp_path.clone(),
                        file,
                        upload_id: upload_id.clone(),
                        size_bytes: payload.size_bytes,
                        file_identity,
                        status: SessionFileHttpUploadStatus::Active,
                        written_ranges: BTreeMap::new(),
                        inflight_ranges: BTreeMap::new(),
                        modified_at_ms: None,
                        published: false,
                        updated_at_ms: now_ms,
                    },
                );
            }
            return Err(ProtocolError::StateFailed);
        }
        http_upload_test_crash_checkpoint("after_temp_identity");
        let now_ms = current_unix_timestamp_millis().0;
        // 中文注释：最终路径在完整上传并 fsync 前始终不存在；所有分片只写同目录私有临时对象。
        self.session_file_http_uploads.insert(
            upload_id.clone(),
            SessionFileHttpUploadState {
                session_id: payload.session_id,
                target: target.clone(),
                temp_path,
                file,
                upload_id: upload_id.clone(),
                size_bytes: payload.size_bytes,
                file_identity,
                status: SessionFileHttpUploadStatus::Active,
                written_ranges: BTreeMap::new(),
                inflight_ranges: BTreeMap::new(),
                modified_at_ms: None,
                published: false,
                updated_at_ms: now_ms,
            },
        );

        Ok(SessionFileHttpUploadReadyPayload {
            session_id: payload.session_id,
            path: absolute_path_string(&target),
            upload_id,
            size_bytes: payload.size_bytes,
            offset_bytes: 0,
        })
    }

    pub fn v070_session_file_http_upload_payload(
        &mut self,
        connection: &ProtocolConnection,
        upload_id: &str,
        offset_bytes: u64,
    ) -> Result<SessionFileHttpUploadStreamPayload, ProtocolError> {
        connection.authenticated_device_id()?;
        let state = self
            .session_file_http_uploads
            .get(upload_id)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        Ok(SessionFileHttpUploadStreamPayload {
            session_id: state.session_id,
            path: absolute_path_string(&state.target),
            upload_id: state.upload_id.clone(),
            size_bytes: state.size_bytes,
            offset_bytes,
        })
    }

    pub fn v070_session_file_http_upload_progress(
        &mut self,
        connection: &ProtocolConnection,
        upload_id: &str,
    ) -> Result<SessionFileUploadProgressPayload, ProtocolError> {
        let state = self
            .session_file_http_uploads
            .get(upload_id)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        self.require_attached_session_root(connection, state.session_id)?;
        let progress = state.progress(state.status == SessionFileHttpUploadStatus::Complete);
        if !progress.eof {
            return Err(ProtocolError::InvalidState);
        }
        Ok(progress)
    }

    pub fn v070_abort_session_file_http_upload(
        &mut self,
        connection: &ProtocolConnection,
        upload_id: &str,
    ) -> Result<(), ProtocolError> {
        let payload = self.v070_session_file_http_upload_payload(connection, upload_id, 0)?;
        self.abort_session_file_http_upload(connection, &payload)
    }

    fn prune_session_file_http_uploads(&mut self) {
        let now_ms = current_unix_timestamp_millis().0;
        let stale_upload_ids: Vec<String> = self
            .session_file_http_uploads
            .iter()
            .filter_map(|(upload_id, state)| match state.status {
                SessionFileHttpUploadStatus::Active
                    if now_ms.saturating_sub(state.updated_at_ms)
                        > SESSION_FILE_HTTP_UPLOAD_ACTIVE_IDLE_TTL_MS =>
                {
                    Some(upload_id.clone())
                }
                SessionFileHttpUploadStatus::Complete | SessionFileHttpUploadStatus::Aborted
                    if now_ms.saturating_sub(state.updated_at_ms)
                        > SESSION_FILE_HTTP_UPLOAD_TOMBSTONE_TTL_MS =>
                {
                    Some(upload_id.clone())
                }
                SessionFileHttpUploadStatus::Active
                | SessionFileHttpUploadStatus::Complete
                | SessionFileHttpUploadStatus::Aborted => None,
            })
            .collect();
        for upload_id in stale_upload_ids {
            let Some(mut state) = self.session_file_http_uploads.remove(&upload_id) else {
                continue;
            };
            if state.status == SessionFileHttpUploadStatus::Active {
                // 中文注释：Active idle 超时代表浏览器在 init 后中断；这里删除本 upload_id
                // 预分配的新目标文件，避免半截文件永久留在文件列表。
                if state.published {
                    state.updated_at_ms = now_ms;
                    self.session_file_http_uploads.insert(upload_id, state);
                    continue;
                }
                match remove_session_file_http_upload_target(&state.temp_path, state.file_identity)
                {
                    Ok(
                        SessionFileHttpUploadCleanupOutcome::AlreadyGone
                        | SessionFileHttpUploadCleanupOutcome::TargetReplaced,
                    ) if session_file_http_upload_open_file_has_remaining_links(
                        &state.file,
                        state.file_identity,
                    ) =>
                    {
                        tracing::warn!(
                            upload_id = %upload_id,
                            "stale HTTP upload target is gone or replaced but original file still has links"
                        );
                        // 中文注释：target path 已缺失或被替换，但原 active 文件对象仍有
                        // hardlink alias；不能删除 guard，否则 alias 会暴露未完成内容。
                        state.updated_at_ms = now_ms;
                        self.session_file_http_uploads.insert(upload_id, state);
                    }
                    Ok(
                        SessionFileHttpUploadCleanupOutcome::Removed
                        | SessionFileHttpUploadCleanupOutcome::AlreadyGone
                        | SessionFileHttpUploadCleanupOutcome::TargetReplaced,
                    ) => {
                        let _ = StateStore::remove_http_upload(&self.config.state_path, &upload_id);
                    }
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            upload_id = %upload_id,
                        target = %state.target.display(),
                            "failed to prune stale HTTP upload target detail"
                        );
                        tracing::warn!(
                            %error,
                            upload_id = %upload_id,
                            "failed to prune stale HTTP upload target"
                        );
                        // 中文注释：清理失败时不能丢内存 active state；guard 依赖它隐藏
                        // 未完成目标。刷新更新时间，避免每次文件列表/Git 请求都重复清理失败。
                        state.updated_at_ms = now_ms;
                        self.session_file_http_uploads.insert(upload_id, state);
                    }
                }
            }
        }
    }

    pub(crate) fn begin_session_file_http_upload_write(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileHttpUploadStreamPayload,
        _device_id: DeviceId,
        write_len: u64,
    ) -> Result<SessionFileHttpUploadBegin, ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        if !session_file_http_upload_id_is_safe(&payload.upload_id) {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        if target.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) else {
            return Err(ProtocolError::InvalidEnvelope);
        };
        if state.session_id != payload.session_id
            || state.target != target
            || state.upload_id != payload.upload_id
            || state.size_bytes != payload.size_bytes
        {
            return Err(ProtocolError::InvalidEnvelope);
        }
        state.updated_at_ms = current_unix_timestamp_millis().0;
        match state.status {
            SessionFileHttpUploadStatus::Active => {
                let file = state.file.try_clone().map_err(map_file_path_error)?;
                let reserved_range = state.reserve_write_range(payload.offset_bytes, write_len)?;
                Ok(SessionFileHttpUploadBegin::Write(
                    SessionFileHttpUploadWritePlan {
                        target,
                        storage_path: if state.published {
                            state.target.clone()
                        } else {
                            state.temp_path.clone()
                        },
                        file,
                        size_bytes: payload.size_bytes,
                        offset_bytes: payload.offset_bytes,
                        file_identity: state.file_identity,
                        written_ranges: state.written_ranges.clone(),
                        reserved_range,
                    },
                ))
            }
            SessionFileHttpUploadStatus::Complete => {
                Ok(SessionFileHttpUploadBegin::Complete(state.progress(true)))
            }
            SessionFileHttpUploadStatus::Aborted => Err(ProtocolError::InvalidState),
        }
    }

    pub(crate) fn commit_session_file_http_upload_write(
        &mut self,
        payload: &SessionFileHttpUploadStreamPayload,
        file_result: &SessionFileHttpUploadFileWriteResult,
    ) -> Result<SessionFileHttpUploadCommit, ProtocolError> {
        let complete_progress = {
            let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) else {
                return Err(ProtocolError::InvalidEnvelope);
            };
            if state.session_id != payload.session_id || state.size_bytes != payload.size_bytes {
                return Err(ProtocolError::InvalidEnvelope);
            }
            state.updated_at_ms = current_unix_timestamp_millis().0;
            match state.status {
                SessionFileHttpUploadStatus::Active => {
                    state.release_inflight_range(file_result.reserved_range)?;
                    for &(start, end) in &file_result.written_ranges {
                        state.record_written_range(start, end)?;
                    }
                    let received_bytes = state.received_bytes()?;
                    if state.has_complete_coverage()? {
                        state.modified_at_ms = file_result.modified_at_ms;
                        Some(state.progress(true))
                    } else {
                        return Ok(SessionFileHttpUploadCommit::Progress(
                            state.progress_with_offset(received_bytes, false),
                        ));
                    }
                }
                SessionFileHttpUploadStatus::Complete => {
                    let _ = state.release_inflight_range(file_result.reserved_range);
                    return Ok(SessionFileHttpUploadCommit::Complete(state.progress(true)));
                }
                SessionFileHttpUploadStatus::Aborted => {
                    let _ = state.release_inflight_range(file_result.reserved_range);
                    return Err(ProtocolError::InvalidState);
                }
            }
        };

        if let Some(progress) = complete_progress {
            let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) else {
                return Err(ProtocolError::InvalidEnvelope);
            };
            state.file.sync_all().map_err(map_file_path_error)?;
            http_upload_test_crash_checkpoint("after_file_sync");
            if !state.published {
                ensure_session_file_http_upload_target_identity(
                    &state.temp_path,
                    state.file_identity,
                )?;
                publish_session_file_http_upload_noreplace(&state.temp_path, &state.target)?;
                state.published = true;
                http_upload_test_crash_checkpoint("after_rename");
            }
            sync_session_file_http_upload_parent(&state.target)?;
            http_upload_test_crash_checkpoint("after_dir_sync");
            StateStore::remove_http_upload(&self.config.state_path, &payload.upload_id)
                .map_err(|_| ProtocolError::StateFailed)?;
            state.status = SessionFileHttpUploadStatus::Complete;
            http_upload_test_crash_checkpoint("after_finalize");
            return Ok(SessionFileHttpUploadCommit::Complete(progress));
        }
        Err(ProtocolError::InvalidState)
    }

    pub(crate) fn cancel_session_file_http_upload_write(
        &mut self,
        payload: &SessionFileHttpUploadStreamPayload,
        reserved_range: Option<(u64, u64)>,
    ) {
        if let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) {
            let _ = state.release_inflight_range(reserved_range);
            state.updated_at_ms = current_unix_timestamp_millis().0;
        }
    }

    pub fn write_session_file_http_upload(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileHttpUploadStreamPayload,
        device_id: DeviceId,
        chunks: impl IntoIterator<Item = Vec<u8>>,
    ) -> Result<SessionFileUploadProgressPayload, ProtocolError> {
        let chunks: Vec<Vec<u8>> = chunks.into_iter().collect();
        let write_len = session_file_http_upload_chunks_len(&chunks)?;
        let plan = match self.begin_session_file_http_upload_write(
            connection,
            payload.clone(),
            device_id,
            write_len,
        )? {
            SessionFileHttpUploadBegin::Write(plan) => plan,
            SessionFileHttpUploadBegin::Complete(progress) => return Ok(progress),
        };
        let reserved_range = plan.reserved_range;
        let file_result = match write_session_file_http_upload_files(plan, chunks) {
            Ok(result) => result,
            Err(error) => {
                self.cancel_session_file_http_upload_write(&payload, reserved_range);
                return Err(error);
            }
        };
        match self.commit_session_file_http_upload_write(&payload, &file_result)? {
            SessionFileHttpUploadCommit::Progress(progress)
            | SessionFileHttpUploadCommit::Complete(progress) => Ok(progress),
        }
    }

    pub fn abort_session_file_http_upload(
        &mut self,
        connection: &ProtocolConnection,
        payload: &SessionFileHttpUploadStreamPayload,
    ) -> Result<(), ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        // 中文注释：abort 只删除本 upload_id 的隐藏临时对象；一旦已经原子发布就不能回滚。
        if !session_file_http_upload_id_is_safe(&payload.upload_id) {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) else {
            return Ok(());
        };
        if state.session_id != payload.session_id
            || state.target != target
            || state.size_bytes != payload.size_bytes
        {
            return Err(ProtocolError::InvalidEnvelope);
        }
        if state.status == SessionFileHttpUploadStatus::Complete {
            return Ok(());
        }
        if state.published {
            return Err(ProtocolError::InvalidState);
        }
        state.inflight_ranges.clear();
        let cleanup_outcome =
            remove_session_file_http_upload_target(&state.temp_path, state.file_identity)?;
        if matches!(
            cleanup_outcome,
            SessionFileHttpUploadCleanupOutcome::AlreadyGone
                | SessionFileHttpUploadCleanupOutcome::TargetReplaced
        ) && session_file_http_upload_open_file_has_remaining_links(
            &state.file,
            state.file_identity,
        ) {
            // 中文注释：用户删除或替换了 target path，但原 upload 对象还可通过
            // hardlink alias 访问；abort 必须失败并保留 active guard。
            state.updated_at_ms = current_unix_timestamp_millis().0;
            return Err(ProtocolError::InvalidState);
        }
        StateStore::remove_http_upload(&self.config.state_path, &payload.upload_id)
            .map_err(|_| ProtocolError::StateFailed)?;
        state.status = SessionFileHttpUploadStatus::Aborted;
        state.updated_at_ms = current_unix_timestamp_millis().0;
        if cleanup_outcome == SessionFileHttpUploadCleanupOutcome::TargetReplaced {
            Err(ProtocolError::InvalidState)
        } else {
            Ok(())
        }
    }

    pub fn prepare_session_file_http_download(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileHttpDownloadPayload,
    ) -> Result<(SessionFileDownloadStreamReadyPayload, fs::File, u64), ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_existing_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        let file = fs::File::open(&target).map_err(map_file_path_error)?;
        let metadata = file.metadata().map_err(map_file_path_error)?;
        if metadata.is_dir() || payload.offset_bytes > metadata.len() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let ready = SessionFileDownloadStreamReadyPayload {
            session_id: payload.session_id,
            path: absolute_path_string(&target),
            name: session_file_download_name(&target),
            size_bytes: metadata.len(),
            modified_at_ms: metadata_modified_at_ms(&metadata),
        };
        Ok((ready, file, payload.offset_bytes))
    }

    pub fn prepare_v070_session_file_download(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDownloadPreparePayload,
    ) -> Result<SessionFileDownloadReadyPayload, ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_existing_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        let metadata = fs::metadata(&target).map_err(map_file_path_error)?;
        if metadata.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let now_ms = current_unix_timestamp_millis();
        self.expire_session_file_downloads(now_ms);
        if self.session_file_downloads.len() >= SESSION_FILE_DOWNLOAD_GRANT_LIMIT {
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
                path: target,
                download_name: session_file_download_name(Path::new(&path)),
                size_bytes: metadata.len(),
                modified_at_ms,
                expires_at_ms,
            },
        );
        Ok(SessionFileDownloadReadyPayload {
            session_id: payload.session_id,
            path,
            token,
            size_bytes: metadata.len(),
            modified_at_ms,
            expires_at_ms,
        })
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
        let attached = self.require_existing_attached_session(connection, payload.session_id)?;

        self.runtime
            .steal_control(
                &attached.internal_session_id,
                &device_key(attached.device_id),
            )
            .map_err(map_runtime_error)?;

        let response = ControlGrantPayload {
            session_id: payload.session_id,
            device_id: attached.device_id,
        };

        Ok(vec![envelope_value(MessageType::ControlGrant, response)?])
    }

    fn list_sessions(
        &mut self,
        connection: &ProtocolConnection,
        _payload: SessionListPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        connection.authenticated_device_id()?;
        self.reconcile_persisted_closed_sessions()?;
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

    fn daemon_clients_snapshot_payload(&mut self) -> DaemonClientsResultPayload {
        let mut clients_by_device = self.daemon_clients_by_device();
        self.merge_active_daemon_clients(&mut clients_by_device);
        self.daemon_client_payloads_from_history(clients_by_device)
    }

    fn notify_daemon_clients_changed(&self) {
        let current = *self.daemon_clients_signal.borrow();
        let _ = self.daemon_clients_signal.send(current.saturating_add(1));
        self.notify_v070_metadata_changed();
    }

    fn daemon_clients_by_device(&self) -> HashMap<DeviceId, ClientHistoryRecord> {
        match self.client_history.list_clients() {
            Ok(records) => records
                .into_iter()
                .map(|record| (record.device_id, record))
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "failed to list daemon clients from sqlite history");
                HashMap::new()
            }
        }
    }

    fn merge_active_daemon_clients(
        &self,
        clients_by_device: &mut HashMap<DeviceId, ClientHistoryRecord>,
    ) {
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
    }

    fn daemon_client_payloads_from_history(
        &self,
        clients_by_device: HashMap<DeviceId, ClientHistoryRecord>,
    ) -> DaemonClientsResultPayload {
        let mut clients: Vec<_> = clients_by_device
            .into_values()
            .map(daemon_client_to_payload_from_history)
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

        DaemonClientsResultPayload { clients }
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

    fn record_daemon_client_connection(
        &mut self,
        connection: &ProtocolConnection,
        device_id: DeviceId,
        name: Option<&str>,
    ) {
        if !connection.track_daemon_client_history {
            return;
        }
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
            self.notify_daemon_clients_changed();
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
        self.notify_daemon_clients_changed();
    }

    fn record_daemon_client_attach(
        &mut self,
        session_id: SessionId,
        connection: &ProtocolConnection,
        device_id: DeviceId,
    ) {
        if !connection.track_daemon_client_history {
            return;
        }
        let now_ms = current_unix_timestamp_millis();
        if !self.daemon_clients.get(&device_id).is_some_and(|record| {
            record
                .active_connections
                .contains_key(&connection.client_id)
        }) {
            self.record_daemon_client_connection(connection, device_id, None);
        }
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
            self.notify_daemon_clients_changed();
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
        self.notify_daemon_clients_changed();
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
        self.notify_daemon_clients_changed();
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

    fn default_session_files_path(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<String>, ProtocolError> {
        let refreshed_cwd = self.refresh_session_terminal_cwd(session_id)?;
        self.default_session_files_path_after_refresh(session_id, refreshed_cwd)
    }

    fn default_session_files_path_after_refresh(
        &mut self,
        session_id: SessionId,
        refreshed_cwd: Option<String>,
    ) -> Result<Option<String>, ProtocolError> {
        if let Some(cwd) = refreshed_cwd {
            return Ok(Some(cwd));
        }
        if let Some(cwd) = self.session_terminal_cwds.get(&session_id) {
            return Ok(Some(absolute_path_string(cwd)));
        }

        self.client_history
            .session_files_path(session_id)
            .map_err(ProtocolError::from)
    }

    fn refresh_session_terminal_cwd(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<String>, ProtocolError> {
        let Some(cwd) = self.read_session_terminal_cwd(session_id)? else {
            // 中文注释：supervisor 上报的 terminal cwd 可能在目录被删除或权限变化后暂时不可读。
            // 这时不能继续使用上一轮成功同步的 terminal cwd cache；否则文件面板
            // 会在用户手动浏览后又被旧 cwd 拉回去。清掉内存 cache 后让调用方
            // 回退到 client history 中的最新文件面板位置。
            if self.session_terminal_cwds.remove(&session_id).is_some() {
                self.notify_v070_metadata_changed();
            }
            return Ok(None);
        };
        if self
            .session_terminal_cwds
            .get(&session_id)
            .is_some_and(|cached| cached == &cwd)
        {
            return Ok(None);
        }

        let cwd_string = absolute_path_string(&cwd);
        self.session_terminal_cwds.insert(session_id, cwd.clone());
        self.client_history.record_session_files_path(
            session_id,
            &cwd,
            current_unix_timestamp_millis(),
        )?;
        self.notify_v070_metadata_changed();
        Ok(Some(cwd_string))
    }

    fn refresh_v070_metadata_after_terminal_output(&mut self, session_id: SessionId) {
        if self.v070_metadata_signal.receiver_count() == 0 {
            return;
        }
        let now_ms = current_unix_timestamp_millis().0;
        if self
            .session_terminal_cwd_probe_notified_at_ms
            .get(&session_id)
            .is_some_and(|last_ms| {
                now_ms.saturating_sub(*last_ms) < SESSION_TERMINAL_CWD_PROBE_MIN_INTERVAL_MS
            })
        {
            return;
        }
        self.session_terminal_cwd_probe_notified_at_ms
            .insert(session_id, now_ms);
        let _ = self.refresh_session_terminal_cwd(session_id);
        self.notify_v070_metadata_changed();
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

    fn session_files_result_after_refresh(
        &mut self,
        session_id: SessionId,
        requested_path: Option<String>,
        fallback_to_root: bool,
        refreshed_cwd: Option<String>,
    ) -> Result<SessionFilesResultPayload, ProtocolError> {
        // 中文注释：文件列表是 active upload 目标可见性的入口；进入列表前先清理
        // 已超时的 upload 状态，避免断开的旧上传把同目录隐藏临时对象永久遗留。
        self.prune_session_file_http_uploads();
        let requested_path = if requested_path.is_some() {
            requested_path
        } else {
            self.default_session_files_path_after_refresh(session_id, refreshed_cwd)?
        };
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
        let mut entries = read_session_file_entries(&root, &target)?;
        let active_upload_targets = self.active_session_file_http_upload_targets(session_id);
        if !active_upload_targets.is_empty() {
            // 中文注释：HTTP upload 的 init 会创建同目录隐藏临时对象；文件 API 既不能
            // 枚举该对象，也不能在 commit 前暴露最终目标名。
            entries.retain(|entry| {
                !path_matches_active_session_file_http_upload_target(
                    Path::new(&entry.path),
                    &active_upload_targets,
                )
            });
        }
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

    fn active_session_file_http_upload_targets(
        &self,
        _session_id: SessionId,
    ) -> Vec<ActiveSessionFileHttpUploadTarget> {
        self.session_file_http_uploads
            .values()
            .filter(|state| {
                // 中文注释：不同 session 可以指向同一个目录。active upload 是文件系统对象级
                // 保护，不能只按所属 session 过滤，否则另一个 session 能读写同一未完成目标。
                state.status == SessionFileHttpUploadStatus::Active
            })
            .flat_map(|state| {
                [state.target.clone(), state.temp_path.clone()].map(|target| {
                    ActiveSessionFileHttpUploadTarget {
                        target,
                        file_identity: state.file_identity,
                    }
                })
            })
            .collect()
    }

    fn ensure_not_active_session_file_http_upload_target(
        &mut self,
        session_id: SessionId,
        target: &Path,
    ) -> Result<(), ProtocolError> {
        // 中文注释：Git 面板也能操作文件。HTTP upload commit 前的隐藏临时对象和
        // 最终目标名不能被 git add/clean/restore/diff 绕过文件 API guard 操作。
        self.prune_session_file_http_uploads();
        let active_targets = self.active_session_file_http_upload_targets(session_id);
        if path_matches_active_session_file_http_upload_target(target, &active_targets) {
            return Err(ProtocolError::InvalidState);
        }
        Ok(())
    }

    fn ensure_no_active_session_file_http_upload_target_in_git_scope(
        &mut self,
        session_id: SessionId,
        worktree: &Path,
        file_path: Option<&str>,
    ) -> Result<(), ProtocolError> {
        // 中文注释：Git 的目录级操作会递归读取/删除目录内路径；不仅要挡住
        // active 目标自身，也要挡住其父目录，以及指向同一 inode 的 hardlink/symlink alias。
        self.prune_session_file_http_uploads();
        let active_targets = self.active_session_file_http_upload_targets(session_id);
        if active_targets.is_empty() {
            return Ok(());
        }
        let scope = file_path
            .map(|path| worktree.join(path))
            .unwrap_or_else(|| worktree.to_path_buf());
        if active_targets
            .iter()
            .any(|active| active.target == scope || active.target.starts_with(&scope))
        {
            return Err(ProtocolError::InvalidState);
        }
        if path_matches_active_session_file_http_upload_target(&scope, &active_targets)
            || git_scope_contains_active_session_file_http_upload_alias(
                worktree,
                file_path,
                &active_targets,
            )?
        {
            return Err(ProtocolError::InvalidState);
        }
        Ok(())
    }

    fn session_git_result(
        &mut self,
        session_id: SessionId,
    ) -> Result<SessionGitResultPayload, ProtocolError> {
        self.prune_session_file_http_uploads();
        let root = self
            .session_roots
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        let requested_path = self.default_session_files_path(session_id)?;
        let (cwd, normalized_cwd) = match resolve_session_file_target(&root, requested_path) {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(%error, session_id = %session_id.0, "session git cwd is not readable; falling back to root");
                resolve_session_file_target(&root, None)?
            }
        };
        let active_upload_targets = self.active_session_file_http_upload_targets(session_id);

        Ok(read_session_git_snapshot(
            session_id,
            &cwd,
            normalized_cwd,
            &active_upload_targets,
        ))
    }

    fn session_git_worktree_path(
        &mut self,
        session_id: SessionId,
        requested_worktree: &str,
    ) -> Result<PathBuf, ProtocolError> {
        let root = self
            .session_roots
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        let requested_path = self.default_session_files_path(session_id)?;
        let (cwd, _) = match resolve_session_file_target(&root, requested_path) {
            Ok(resolved) => resolved,
            Err(_) => resolve_session_file_target(&root, None)?,
        };
        let repo_root = current_git_repository_root(&cwd)
            .map_err(|_| ProtocolError::RuntimeFailed)?
            .ok_or(ProtocolError::InvalidEnvelope)?;
        let requested = Path::new(requested_worktree)
            .canonicalize()
            .map_err(map_file_path_error)?;

        read_git_worktrees(&repo_root)
            .map_err(|_| ProtocolError::RuntimeFailed)?
            .into_iter()
            .map(|worktree| worktree.path)
            .find(|worktree| same_path(worktree, &requested))
            .ok_or(ProtocolError::InvalidEnvelope)
    }

    fn notify_session_resized(&self, session_id: SessionId, size: TerminalSize) {
        let Some(signal) = self.session_resize_signals.get(&session_id) else {
            return;
        };
        // resize 是 session 元数据，不含终端明文；推送给已 attach 连接可避免多窗口尺寸认知分叉。
        let _ = signal.send(size);
    }

    fn start_watched_attachment(
        &mut self,
        connection: &mut ProtocolConnection,
        wire_session_id: SessionId,
        internal_session_id: &str,
        size: TerminalSize,
        bootstrap: PtyAttachmentBootstrap,
    ) -> Result<PendingWatchedAttachmentStart, ProtocolError> {
        let previous_attachment_id = connection.take_watched_attachment_id(wire_session_id);
        let attachment_id = connection.allocate_watched_attachment_id(wire_session_id);
        if let Err(error) = self
            .runtime
            .start_watched_attachment(
                internal_session_id,
                &attachment_id,
                proto_size_to_runtime(size),
                bootstrap,
            )
            .map_err(map_runtime_error)
        {
            if let Some(previous_attachment_id) = previous_attachment_id {
                connection.remember_watched_attachment(wire_session_id, previous_attachment_id);
            }
            return Err(error);
        }
        Ok(PendingWatchedAttachmentStart {
            wire_session_id,
            attachment_id,
            previous_attachment_id,
        })
    }

    fn commit_watched_attachment_start(
        &mut self,
        connection: &mut ProtocolConnection,
        pending: PendingWatchedAttachmentStart,
    ) {
        if let Some(previous_attachment_id) = pending.previous_attachment_id {
            self.release_watched_attachment(pending.wire_session_id, previous_attachment_id);
        }
        connection.remember_watched_attachment(pending.wire_session_id, pending.attachment_id);
    }

    fn rollback_watched_attachment_start(
        &mut self,
        connection: &mut ProtocolConnection,
        pending: PendingWatchedAttachmentStart,
    ) {
        self.release_watched_attachment(pending.wire_session_id, pending.attachment_id);
        if let Some(previous_attachment_id) = pending.previous_attachment_id {
            connection.remember_watched_attachment(pending.wire_session_id, previous_attachment_id);
        }
    }

    fn release_watched_attachment(&mut self, wire_session_id: SessionId, attachment_id: String) {
        let Some(internal_session_id) = self.session_index.get(&wire_session_id).cloned() else {
            return;
        };
        let _ = self
            .runtime
            .drop_watched_attachment(&internal_session_id, &attachment_id);
    }

    fn release_watched_attachments(&mut self, watched_attachments: Vec<(SessionId, String)>) {
        for (session_id, attachment_id) in watched_attachments {
            self.release_watched_attachment(session_id, attachment_id);
        }
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
        self.release_watched_attachments(connection.take_all_watched_attachments());
        if connection.track_daemon_client_history {
            self.mark_daemon_client_connection_offline(device_id, connection.client_id, now_ms);
        }

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

    fn read_watched_attachment_frame(
        &mut self,
        session_id: SessionId,
        attachment_id: &str,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .read_watched_attachment_frame(&internal_session_id, attachment_id)
            .map_err(map_runtime_error)
    }

    fn write_watched_attachment_frame(
        &mut self,
        session_id: SessionId,
        attachment_id: &str,
        bytes: &[u8],
    ) -> Result<(), ProtocolError> {
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .write_watched_attachment_frame(&internal_session_id, attachment_id, bytes)
            .map_err(map_runtime_error)
    }

    fn record_v070_terminal_resize(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> Result<(), ProtocolError> {
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .record_watched_attachment_resize(&internal_session_id, proto_size_to_runtime(size))
            .map_err(map_runtime_error)?;
        self.notify_session_resized(session_id, size);
        self.client_history.record_session_resized(
            session_id,
            size,
            current_unix_timestamp_millis(),
        )?;
        self.persist_state()?;
        self.notify_v070_metadata_changed();
        Ok(())
    }

    fn visible_session_ids(&self) -> HashSet<SessionId> {
        self.session_index.keys().copied().collect()
    }

    fn repair_visible_session_metadata(&mut self) {
        let session_ids = self.visible_session_ids().into_iter().collect::<Vec<_>>();
        for session_id in session_ids {
            if let Err(error) = self.repair_visible_session_metadata_for(session_id) {
                tracing::warn!(%error, session_id = %session_id.0, "failed to repair visible session metadata");
            }
        }
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
        .and_then(non_empty_trimmed)
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
        stats.f_blocks.saturating_mul(block_size),
        stats.f_bavail.saturating_mul(block_size),
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

fn session_created_at(session: &SessionSummaryPayload) -> UnixTimestampMillis {
    session.created_at_ms.unwrap_or(UnixTimestampMillis(0))
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

pub struct ProtocolConnection {
    client_id: ClientId,
    peer_ip: Option<String>,
    state: ProtocolConnectionState,
    track_daemon_client_history: bool,
    authenticated_device_id: Option<DeviceId>,
    attached_sessions: Vec<SessionId>,
    watched_attachment_ids: HashMap<SessionId, String>,
    next_watched_attachment_number: u64,
}

impl ProtocolConnection {
    fn new(device_id: DeviceId, track_daemon_client_history: bool) -> Self {
        Self {
            client_id: ClientId::new(),
            peer_ip: None,
            state: ProtocolConnectionState::Authenticated,
            track_daemon_client_history,
            authenticated_device_id: Some(device_id),
            attached_sessions: Vec::new(),
            watched_attachment_ids: HashMap::new(),
            next_watched_attachment_number: 1,
        }
    }

    pub fn authenticated_http(device_id: DeviceId) -> Self {
        Self::new(device_id, false)
    }

    pub fn authenticated_v070_terminal(device_id: DeviceId) -> Self {
        Self::new(device_id, true)
    }

    pub fn write_v070_terminal_frame<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        bytes: &[u8],
    ) -> Result<(), ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let frame = decode_supervisor_terminal_client_frame(bytes)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;
        self.ensure_attached_to(session_id)?;
        let attachment_id = self
            .watched_attachment_ids
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::InvalidState)?;
        protocol.write_watched_attachment_frame(session_id, &attachment_id, bytes)?;
        if let SupervisorTerminalClientFrame::Resize { size } = frame {
            protocol.record_v070_terminal_resize(
                session_id,
                TerminalSize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                },
            )?;
        }
        Ok(())
    }

    pub fn drain_v070_terminal_frames<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<Vec<u8>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.ensure_attached_to(session_id)?;
        let attachment_id = self
            .watched_attachment_ids
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::InvalidState)?;
        let mut frames = Vec::new();
        let mut transported_bytes = 0usize;
        for _ in 0..LIVE_OUTPUT_DRAIN_MAX_CHUNKS {
            let Some(frame) = protocol.read_watched_attachment_frame(session_id, &attachment_id)?
            else {
                break;
            };
            transported_bytes = transported_bytes.saturating_add(frame.len());
            frames.push(frame);
            if transported_bytes >= TERMINAL_STREAM_BATCH_MAX_BYTES {
                break;
            }
        }
        if !frames.is_empty() {
            protocol.refresh_v070_metadata_after_terminal_output(session_id);
        }
        Ok(frames)
    }

    pub fn authenticated_device_id(&self) -> Result<DeviceId, ProtocolError> {
        self.authenticated_device_id
            .ok_or(ProtocolError::Unauthenticated)
    }

    pub fn close<B, V>(&mut self, protocol: &mut DaemonProtocol<B, V>)
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        protocol.detach_connection(self);
    }

    pub(crate) fn dispatch_v070_http_control<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        method: &str,
        payload: Value,
    ) -> Result<Value, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let mut responses = match method {
            METHOD_CONTROL_REQUEST => protocol.request_control(self, decode_payload(payload)?),
            METHOD_SESSION_RENAME => protocol.rename_session(self, decode_payload(payload)?),
            METHOD_SESSION_REORDER => protocol.reorder_sessions(self, decode_payload(payload)?),
            METHOD_SESSION_CLOSE => protocol.close_session(self, decode_payload(payload)?),
            METHOD_SESSION_SEARCH => protocol.search_session_output(self, decode_payload(payload)?),
            METHOD_SESSION_FILES => protocol.list_session_files(self, decode_payload(payload)?),
            METHOD_SESSION_GIT => protocol.list_session_git(self, decode_payload(payload)?),
            METHOD_SESSION_GIT_ACTION => {
                protocol.apply_session_git_action(self, decode_payload(payload)?)
            }
            METHOD_SESSION_GIT_DIFF => {
                protocol.read_session_git_diff(self, decode_payload(payload)?)
            }
            METHOD_SESSION_FILE_READ => protocol.read_session_file(self, decode_payload(payload)?),
            METHOD_SESSION_FILE_WRITE => {
                protocol.write_session_file(self, decode_payload(payload)?)
            }
            METHOD_SESSION_FILE_DELETE => {
                protocol.delete_session_file(self, decode_payload(payload)?)
            }
            METHOD_DAEMON_CLIENT_FORGET => {
                protocol.forget_daemon_client(self, decode_payload(payload)?)
            }
            _ => return Err(ProtocolError::InvalidEnvelope),
        }?;
        responses
            .drain(..)
            .next()
            .map(|response| response.payload)
            .ok_or(ProtocolError::InvalidState)
    }

    fn allocate_watched_attachment_id(&mut self, session_id: SessionId) -> String {
        let number = self.next_watched_attachment_number;
        self.next_watched_attachment_number = self.next_watched_attachment_number.saturating_add(1);
        format!("{}:{}:{number}", self.client_id.0, session_id.0)
    }

    fn remember_watched_attachment(&mut self, session_id: SessionId, attachment_id: String) {
        self.watched_attachment_ids
            .insert(session_id, attachment_id);
    }

    fn take_watched_attachment_id(&mut self, session_id: SessionId) -> Option<String> {
        self.watched_attachment_ids.remove(&session_id)
    }

    fn take_all_watched_attachments(&mut self) -> Vec<(SessionId, String)> {
        self.watched_attachment_ids.drain().collect()
    }

    fn attach(&mut self, session_id: SessionId) {
        if !self.attached_sessions.contains(&session_id) {
            self.attached_sessions.push(session_id);
        }
    }

    fn ensure_attached_to(&self, session_id: SessionId) -> Result<(), ProtocolError> {
        self.attached_sessions
            .contains(&session_id)
            .then_some(())
            .ok_or(ProtocolError::InvalidState)
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

fn base64_payload_decoded_len(data_base64: &str) -> usize {
    data_base64.trim_end_matches('=').len().saturating_mul(3) / 4
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

    // 中文注释：这里保留 `TERM=xterm-256color` 只是给 shell/TUI 暴露稳定能力集，
    // 不代表 Web 端仍在使用 xterm renderer。当前浏览器 renderer 已固定为 Ghostty。
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

fn path_matches_active_session_file_http_upload_target(
    path: &Path,
    active_targets: &[ActiveSessionFileHttpUploadTarget],
) -> bool {
    if active_targets.iter().any(|active| active.target == path) {
        return true;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let identity = SessionFileHttpUploadFileIdentity::from_metadata(&metadata);
    active_targets.iter().any(|active| {
        if !active.file_identity.has_stable_filesystem_object_identity()
            || !identity.has_stable_filesystem_object_identity()
        {
            // 中文注释：无法可靠识别 alias 时按命中处理，避免 active upload
            // 在缺失 file id 的文件系统上被读写删或 Git 操作绕过。
            return true;
        }
        active.file_identity.is_same_filesystem_object(identity)
    })
}

fn git_scope_contains_active_session_file_http_upload_alias(
    worktree: &Path,
    file_path: Option<&str>,
    active_targets: &[ActiveSessionFileHttpUploadTarget],
) -> Result<bool, ProtocolError> {
    // 中文注释：不要自己递归扫描 worktree。Git action/diff 只会影响 Git 认为有变化
    // 的 pathspec，直接用 `git status -z -- <scope>` 找候选路径，再按 inode 识别 alias。
    let output = if let Some(path) = file_path {
        run_git_command(
            worktree,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "-z",
                "--",
                path,
            ],
        )
    } else {
        run_git_command(
            worktree,
            &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
        )
    };
    let output = output.map_err(|_| ProtocolError::RuntimeFailed)?;
    if output.truncated() || !output.success {
        return Err(ProtocolError::RuntimeFailed);
    }
    Ok(parse_git_status_entries(&output.stdout)
        .into_iter()
        .any(|change| {
            path_matches_active_session_file_http_upload_target(
                &worktree.join(change.path),
                active_targets,
            )
        }))
}

fn validate_git_relative_file_path(path: &str) -> Result<(), ProtocolError> {
    let raw = path.trim();
    if raw.starts_with(':') {
        // 中文注释：Git 的 pathspec magic（如 `:(glob)*`）即使放在 `--` 后仍会生效；
        // 当前 UI 只支持普通相对路径，因此直接拒绝 magic 入口，避免绕过 active upload guard。
        return Err(ProtocolError::InvalidEnvelope);
    }
    if raw
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']'))
    {
        // 中文注释：`--` 不会禁用 Git wildcard pathspec；这里禁用 glob 字符，
        // 保证 guard 检查到的字面路径和 Git 实际操作的路径一致。
        return Err(ProtocolError::InvalidEnvelope);
    }
    let path = Path::new(raw);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ProtocolError::InvalidEnvelope);
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ProtocolError::InvalidEnvelope);
    }
    Ok(())
}

fn match_indices(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut indices = Vec::new();
    let mut offset = 0;
    while let Some(index) = haystack[offset..].find(needle) {
        let absolute_index = offset + index;
        indices.push(haystack[..absolute_index].chars().count());
        offset = absolute_index + needle.len();
    }
    indices
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

#[derive(Debug, Clone)]
struct GitCommandResult {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy)]
struct GitCommandLimits {
    timeout: Duration,
    stdout_max_bytes: usize,
    stderr_max_bytes: usize,
}

#[derive(Debug)]
struct BoundedGitOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedGitOutput {
    fn with_capacity(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            truncated: false,
        }
    }

    fn extend_from_slice(&mut self, bytes: &[u8], max_bytes: usize) {
        let retained = bytes.len().min(max_bytes.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..retained]);
        if retained < bytes.len() {
            self.truncated = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitSnapshotError {
    Command,
    Truncated,
}

impl GitCommandResult {
    fn truncated(&self) -> bool {
        self.stdout_truncated || self.stderr_truncated
    }
}

#[derive(Debug, Clone)]
struct GitWorktreeInfo {
    path: PathBuf,
    branch: Option<String>,
    head: Option<String>,
}

fn read_session_git_snapshot(
    session_id: SessionId,
    cwd: &Path,
    cwd_text: String,
    active_upload_targets: &[ActiveSessionFileHttpUploadTarget],
) -> SessionGitResultPayload {
    let repo_output = match run_git_command(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(output) => output,
        Err(error) => {
            return session_git_error(session_id, cwd_text, format!("git unavailable: {error}"));
        }
    };
    if repo_output.truncated() {
        return session_git_error(session_id, cwd_text, GIT_OUTPUT_TRUNCATED_MESSAGE);
    }
    if !repo_output.success {
        return session_git_error(
            session_id,
            cwd_text,
            git_error_message(&repo_output, "not a git repository"),
        );
    }
    let Some(repo_root_text) = first_non_empty_line(&repo_output.stdout) else {
        return session_git_error(session_id, cwd_text, "git repository root is unavailable");
    };
    let repo_root = PathBuf::from(repo_root_text);
    let canonical_repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let repository_root_text = absolute_path_string(&canonical_repo_root);

    let current_root = canonical_repo_root.clone();
    let worktree_infos = match read_git_worktrees(&canonical_repo_root) {
        Ok(worktree_infos) => worktree_infos,
        Err(error) => {
            return session_git_error(session_id, cwd_text, session_git_snapshot_error(error));
        }
    };
    let mut worktrees = Vec::new();
    for worktree in worktree_infos {
        let (staged, unstaged) =
            match read_git_worktree_changes(&worktree.path, active_upload_targets) {
                Ok(changes) => changes,
                Err(error) => {
                    return session_git_error(
                        session_id,
                        cwd_text,
                        session_git_snapshot_error(error),
                    );
                }
            };
        let is_current = same_path(&worktree.path, &current_root);
        worktrees.push(SessionGitWorktreePayload {
            path: absolute_path_string(&worktree.path),
            branch: worktree.branch,
            head: worktree.head,
            is_current,
            staged,
            unstaged,
        });
    }
    let graph = match read_git_graph(&canonical_repo_root) {
        Ok(graph) => graph,
        Err(error) => {
            return session_git_error(session_id, cwd_text, session_git_snapshot_error(error));
        }
    };

    SessionGitResultPayload {
        session_id,
        cwd: cwd_text,
        repository_root: Some(repository_root_text),
        worktrees,
        graph,
        error: None,
    }
}

fn session_git_snapshot_error(error: GitSnapshotError) -> &'static str {
    match error {
        GitSnapshotError::Command => GIT_COMMAND_FAILED_MESSAGE,
        GitSnapshotError::Truncated => GIT_OUTPUT_TRUNCATED_MESSAGE,
    }
}

fn session_git_error(
    session_id: SessionId,
    cwd: String,
    error: impl Into<String>,
) -> SessionGitResultPayload {
    SessionGitResultPayload {
        session_id,
        cwd,
        repository_root: None,
        worktrees: Vec::new(),
        graph: Vec::new(),
        error: Some(error.into()),
    }
}

fn run_git_command(cwd: &Path, args: &[&str]) -> Result<GitCommandResult, std::io::Error> {
    // Git 信息只通过本机 git CLI 读取，避免 daemon 为侧栏展示引入额外持久状态。
    run_git_command_with_limits(
        Path::new("git"),
        cwd,
        args,
        GitCommandLimits {
            timeout: GIT_COMMAND_TIMEOUT,
            stdout_max_bytes: GIT_COMMAND_STDOUT_MAX_BYTES,
            stderr_max_bytes: GIT_COMMAND_STDERR_MAX_BYTES,
        },
    )
}

fn run_git_command_with_limits(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    limits: GitCommandLimits,
) -> Result<GitCommandResult, std::io::Error> {
    let mut command = Command::new(program);
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());

    #[cfg(unix)]
    let (status, stdout, stderr) = {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("git stdout must be piped");
        let stderr = child.stderr.take().expect("git stderr must be piped");
        wait_for_git_child_and_output(&mut child, stdout, stderr, limits)?
    };

    #[cfg(not(unix))]
    let (status, stdout, stderr) = {
        let mut cleanup = GitCommandTempOutputCleanup::default();
        let mut stdout_file = open_git_command_temp_output_file("stdout")?;
        cleanup.track(stdout_file.path.clone());
        let mut stderr_file = open_git_command_temp_output_file("stderr")?;
        cleanup.track(stderr_file.path.clone());
        command
            .stdout(Stdio::from(stdout_file.file.try_clone()?))
            .stderr(Stdio::from(stderr_file.file.try_clone()?));
        let mut child = command.spawn()?;
        let status = wait_for_git_child_status(&mut child, limits.timeout)?;
        let stdout =
            read_bounded_git_output_from_file(&mut stdout_file.file, limits.stdout_max_bytes)?;
        let stderr =
            read_bounded_git_output_from_file(&mut stderr_file.file, limits.stderr_max_bytes)?;
        (status, stdout, stderr)
    };

    Ok(GitCommandResult {
        success: status.success(),
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

#[cfg(unix)]
fn wait_for_git_child_and_output(
    child: &mut Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    limits: GitCommandLimits,
) -> Result<(ExitStatus, BoundedGitOutput, BoundedGitOutput), std::io::Error> {
    if let Err(error) =
        set_fd_nonblocking(stdout.as_raw_fd()).and_then(|_| set_fd_nonblocking(stderr.as_raw_fd()))
    {
        let _ = terminate_git_child(child);
        return Err(error);
    }

    let started_at = Instant::now();
    let mut stdout_output = BoundedGitOutput::with_capacity(limits.stdout_max_bytes);
    let mut stderr_output = BoundedGitOutput::with_capacity(limits.stderr_max_bytes);
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    loop {
        if !stdout_closed {
            stdout_closed = match read_available_git_output(
                &mut stdout,
                &mut stdout_output,
                limits.stdout_max_bytes,
            ) {
                Ok(closed) => closed,
                Err(error) => {
                    let _ = terminate_git_child(child);
                    return Err(error);
                }
            };
        }
        if !stderr_closed {
            stderr_closed = match read_available_git_output(
                &mut stderr,
                &mut stderr_output,
                limits.stderr_max_bytes,
            ) {
                Ok(closed) => closed,
                Err(error) => {
                    let _ = terminate_git_child(child);
                    return Err(error);
                }
            };
        }
        if stdout_closed && stderr_closed {
            match child.try_wait() {
                Ok(Some(status)) => return Ok((status, stdout_output, stderr_output)),
                Ok(None) => {}
                Err(error) => {
                    let _ = terminate_git_child(child);
                    return Err(error);
                }
            }
        }
        if started_at.elapsed() >= limits.timeout {
            // 中文注释：总 deadline 覆盖 child 和 pipe drain；后代脱离进程组继续持管道时也不能阻塞 API。
            terminate_git_child(child)?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "git command timed out after {} ms",
                    limits.timeout.as_millis()
                ),
            ));
        }
        let remaining = limits.timeout.saturating_sub(started_at.elapsed());
        std::thread::sleep(GIT_COMMAND_POLL_INTERVAL.min(remaining));
    }
}

#[cfg(unix)]
fn set_fd_nonblocking(fd: std::os::fd::RawFd) -> Result<(), std::io::Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn read_available_git_output(
    reader: &mut impl Read,
    output: &mut BoundedGitOutput,
    max_bytes: usize,
) -> Result<bool, std::io::Error> {
    let mut buffer = [0_u8; 8 * 1024];
    for _ in 0..GIT_COMMAND_DRAIN_MAX_READS {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        output.extend_from_slice(&buffer[..read], max_bytes);
    }
    Ok(false)
}

#[cfg(not(unix))]
struct GitCommandTempOutputFile {
    path: PathBuf,
    file: fs::File,
}

#[cfg(not(unix))]
#[derive(Default)]
struct GitCommandTempOutputCleanup {
    paths: Vec<PathBuf>,
}

#[cfg(not(unix))]
impl GitCommandTempOutputCleanup {
    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }
}

#[cfg(not(unix))]
impl Drop for GitCommandTempOutputCleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(not(unix))]
fn open_git_command_temp_output_file(
    kind: &str,
) -> Result<GitCommandTempOutputFile, std::io::Error> {
    let path = env::temp_dir().join(format!(
        "termd-git-{}-{}-{kind}.tmp",
        std::process::id(),
        ServerId::new().0
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(GitCommandTempOutputFile { path, file })
}

#[cfg(not(unix))]
fn wait_for_git_child_status(
    child: &mut Child,
    timeout: Duration,
) -> Result<ExitStatus, std::io::Error> {
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_git_child(child);
                return Err(error);
            }
        }
        if started_at.elapsed() >= timeout {
            terminate_git_child(child)?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("git command timed out after {} ms", timeout.as_millis()),
            ));
        }
        let remaining = timeout.saturating_sub(started_at.elapsed());
        std::thread::sleep(GIT_COMMAND_POLL_INTERVAL.min(remaining));
    }
}

fn terminate_git_child(child: &mut Child) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        let process_group = i32::try_from(child.id()).map_err(|source| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("git process id is out of range: {source}"),
            )
        })?;
        // Git hook/credential 子进程可能继承 stdout/stderr；只杀直系进程会让 drain 线程永久等待。
        if unsafe { libc::kill(-process_group, libc::SIGKILL) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
        }
        child.wait()?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        child.kill()?;
        child.wait()?;
        Ok(())
    }
}

#[cfg(not(unix))]
fn read_bounded_git_output(
    mut reader: impl Read,
    max_bytes: usize,
) -> Result<BoundedGitOutput, std::io::Error> {
    let mut output = BoundedGitOutput::with_capacity(max_bytes);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        output.extend_from_slice(&buffer[..read], max_bytes);
    }
    Ok(output)
}

#[cfg(not(unix))]
fn read_bounded_git_output_from_file(
    file: &mut fs::File,
    max_bytes: usize,
) -> Result<BoundedGitOutput, std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    read_bounded_git_output(file, max_bytes)
}

fn current_git_repository_root(cwd: &Path) -> Result<Option<PathBuf>, GitSnapshotError> {
    let output = run_git_command(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| GitSnapshotError::Command)?;
    if output.truncated() {
        return Err(GitSnapshotError::Truncated);
    }
    if !output.success {
        return if git_command_reports_no_worktree(&output) {
            Ok(None)
        } else {
            Err(GitSnapshotError::Command)
        };
    }
    let path = first_non_empty_line(&output.stdout)
        .map(PathBuf::from)
        .ok_or(GitSnapshotError::Command)?;
    path.canonicalize()
        .map(Some)
        .map_err(|_| GitSnapshotError::Command)
}

fn apply_git_file_action(
    worktree: &Path,
    file_path: &str,
    action: SessionGitActionKind,
) -> Result<(), ProtocolError> {
    match action {
        SessionGitActionKind::Stage => run_git_checked(worktree, &["add", "--", file_path]),
        SessionGitActionKind::Unstage => {
            run_git_checked(worktree, &["restore", "--staged", "--", file_path])
        }
        SessionGitActionKind::Discard => discard_git_file_change(worktree, file_path),
    }
}

fn read_git_diff(
    worktree: &Path,
    file_path: Option<&str>,
    staged: bool,
) -> Result<String, ProtocolError> {
    let mut args = vec!["diff", "--no-ext-diff"];
    if staged {
        args.push("--cached");
    }
    args.push("--");
    if let Some(file_path) = file_path {
        args.push(file_path);
    }
    let output = run_git_command(worktree, &args).map_err(|_| ProtocolError::RuntimeFailed)?;
    if !output.success {
        tracing::debug!(stderr = %output.stderr, "git diff failed detail");
        tracing::warn!("git diff failed");
        return Err(ProtocolError::RuntimeFailed);
    }
    Ok(limit_text_for_payload(output.stdout, 256 * 1024))
}

fn limit_text_for_payload(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    text.truncate(max_bytes);
    while !text.is_char_boundary(text.len()) {
        text.pop();
    }
    text.push_str("\n[termd: output truncated]");
    text
}

fn discard_git_file_change(worktree: &Path, file_path: &str) -> Result<(), ProtocolError> {
    let status = run_git_command(
        worktree,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            file_path,
        ],
    )
    .map_err(|_| ProtocolError::RuntimeFailed)?;
    if status.truncated() {
        return Err(ProtocolError::RuntimeFailed);
    }
    if !status.success {
        tracing::debug!(stderr = %status.stderr, "git status for discard failed detail");
        tracing::warn!("git status for discard failed");
        return Err(ProtocolError::RuntimeFailed);
    }
    let has_untracked = status.stdout.lines().any(|line| line.starts_with("??"));
    let has_staged_add = status.stdout.lines().any(|line| {
        let mut chars = line.chars();
        matches!(chars.next(), Some('A'))
    });
    if has_untracked {
        return run_git_checked(worktree, &["clean", "-fd", "--", file_path]);
    }
    if has_staged_add {
        return run_git_checked(worktree, &["rm", "-f", "--", file_path]);
    }

    run_git_checked(
        worktree,
        &["restore", "--staged", "--worktree", "--", file_path],
    )
}

fn run_git_checked(cwd: &Path, args: &[&str]) -> Result<(), ProtocolError> {
    let output = run_git_command(cwd, args).map_err(|_| ProtocolError::RuntimeFailed)?;
    if output.success && !output.truncated() {
        return Ok(());
    }

    tracing::debug!(stderr = %output.stderr, "git action failed detail");
    tracing::warn!("git action failed");
    Err(ProtocolError::RuntimeFailed)
}

fn read_git_worktrees(repo_root: &Path) -> Result<Vec<GitWorktreeInfo>, GitSnapshotError> {
    let output = run_git_command(repo_root, &["worktree", "list", "--porcelain"])
        .map_err(|_| GitSnapshotError::Command)?;
    if output.truncated() {
        return Err(GitSnapshotError::Truncated);
    }
    if !output.success {
        return Err(GitSnapshotError::Command);
    }
    let worktrees = parse_git_worktrees(&output.stdout);
    if worktrees.is_empty() {
        return Err(GitSnapshotError::Command);
    }

    Ok(worktrees)
}

fn parse_git_worktrees(output: &str) -> Vec<GitWorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current: Option<GitWorktreeInfo> = None;
    for line in output.lines() {
        if line.trim().is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(normalize_git_worktree(worktree));
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(worktree) = current.replace(GitWorktreeInfo {
                path: PathBuf::from(path),
                branch: None,
                head: None,
            }) {
                worktrees.push(normalize_git_worktree(worktree));
            }
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(worktree) = current.as_mut()
                && head.bytes().any(|byte| byte != b'0')
            {
                worktree.head = Some(short_git_hash(head));
            }
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(worktree) = current.as_mut()
        {
            worktree.branch = Some(short_branch_name(branch));
        }
    }
    if let Some(worktree) = current {
        worktrees.push(normalize_git_worktree(worktree));
    }

    worktrees
}

fn normalize_git_worktree(mut worktree: GitWorktreeInfo) -> GitWorktreeInfo {
    if let Ok(path) = worktree.path.canonicalize() {
        worktree.path = path;
    }
    worktree
}

fn read_git_worktree_changes(
    worktree: &Path,
    active_upload_targets: &[ActiveSessionFileHttpUploadTarget],
) -> Result<
    (
        Vec<SessionGitFileChangePayload>,
        Vec<SessionGitFileChangePayload>,
    ),
    GitSnapshotError,
> {
    let output = run_git_command(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )
    .map_err(|_| GitSnapshotError::Command)?;
    if output.truncated() {
        return Err(GitSnapshotError::Truncated);
    }
    if !output.success {
        return Err(GitSnapshotError::Command);
    }
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    for change in parse_git_status_entries(&output.stdout) {
        if path_matches_active_session_file_http_upload_target(
            &worktree.join(&change.path),
            active_upload_targets,
        ) {
            // 中文注释：active HTTP upload 目标在 Git 看来是 untracked/modified，
            // 但它还不是已完成文件；hardlink/symlink alias 也必须隐藏。
            continue;
        }
        if change.staged {
            staged.push(SessionGitFileChangePayload {
                path: change.path.clone(),
                status: change.status.clone(),
            });
        }
        if change.unstaged {
            unstaged.push(SessionGitFileChangePayload {
                path: change.path,
                status: change.status,
            });
        }
    }

    Ok((staged, unstaged))
}

fn parse_git_status_entries(output: &str) -> Vec<GitStatusLine> {
    if output.contains('\0') {
        let mut changes = Vec::new();
        let mut fields = output.split('\0').filter(|field| !field.is_empty());
        while let Some(field) = fields.next() {
            let Some(change) = parse_git_status_line(field) else {
                continue;
            };
            if change.status.starts_with('R') || change.status.starts_with('C') {
                // 中文注释：`git status -z` 的 rename/copy 记录会额外跟一个旧路径；
                // UI 当前只展示新路径，旧路径字段必须消费掉，避免错位解析。
                let _ = fields.next();
            }
            changes.push(change);
        }
        return changes;
    }
    output.lines().filter_map(parse_git_status_line).collect()
}

struct GitStatusLine {
    path: String,
    status: String,
    staged: bool,
    unstaged: bool,
}

fn parse_git_status_line(line: &str) -> Option<GitStatusLine> {
    let mut chars = line.chars();
    let index = chars.next()?;
    let worktree = chars.next()?;
    let path = line.get(3..)?;
    if path.is_empty() {
        return None;
    }
    let staged = index != ' ' && index != '?';
    let unstaged = worktree != ' ' || (index == '?' && worktree == '?');
    Some(GitStatusLine {
        path: path.to_owned(),
        status: format!("{index}{worktree}"),
        staged,
        unstaged,
    })
}

fn read_git_graph(repo_root: &Path) -> Result<Vec<String>, GitSnapshotError> {
    let output = run_git_command(
        repo_root,
        &[
            "log",
            "--graph",
            "--decorate",
            "--oneline",
            "--max-count=24",
            "--all",
        ],
    )
    .map_err(|_| GitSnapshotError::Command)?;
    if !output.success {
        return Err(GitSnapshotError::Command);
    }
    let mut graph: Vec<String> = output
        .stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if output.truncated() {
        graph.push(GIT_GRAPH_TRUNCATED_MESSAGE.to_owned());
    }
    Ok(graph)
}

fn git_error_message(output: &GitCommandResult, fallback: &'static str) -> String {
    first_non_empty_line(&output.stderr)
        .or_else(|| first_non_empty_line(&output.stdout))
        .unwrap_or(fallback)
        .to_owned()
}

fn git_command_reports_no_worktree(output: &GitCommandResult) -> bool {
    output.exit_code == Some(128)
        && output.stderr.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("fatal: not a git repository")
                || line.starts_with("fatal: this operation must be run in a work tree")
        })
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn short_branch_name(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .to_owned()
}

fn short_git_hash(head: &str) -> String {
    head.chars().take(7).collect()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
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

fn session_file_http_upload_id() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn session_file_http_upload_id_is_safe(upload_id: &str) -> bool {
    !upload_id.is_empty()
        && upload_id.len() <= 128
        && upload_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn cleanup_persisted_session_file_http_uploads(
    state_path: &Path,
) -> Result<(), StateError> {
    for record in StateStore::list_http_uploads(state_path)? {
        if record.dev == 0 && record.ino == 0 {
            tracing::warn!(
                upload_id = %record.upload_id,
                "quarantined HTTP upload intent without durable object identity"
            );
            continue;
        }
        let expected_identity = session_file_http_upload_identity_from_recovery_record(&record);
        let cleanup = remove_persisted_session_file_http_upload_target(
            &record.target_path,
            expected_identity,
        );
        if !matches!(cleanup, Ok(SessionFileHttpUploadCleanupOutcome::Removed)) {
            if let Err(error) = &cleanup {
                tracing::debug!(
                    %error,
                    upload_id = %record.upload_id,
                    target = %record.target_path.display(),
                    "quarantined stale HTTP upload recovery target detail"
                );
            }
            tracing::warn!(
                upload_id = %record.upload_id,
                outcome = ?cleanup.as_ref().ok(),
                "quarantined stale HTTP upload recovery target"
            );
            continue;
        }
        StateStore::remove_http_upload(state_path, &record.upload_id)?;
        tracing::debug!(
            upload_id = %record.upload_id,
            target = %record.target_path.display(),
            outcome = ?cleanup.as_ref().ok(),
            "discarded stale HTTP upload recovery record detail"
        );
        tracing::warn!(
            upload_id = %record.upload_id,
            "discarded stale HTTP upload recovery record"
        );
    }
    Ok(())
}

fn session_file_http_upload_identity_from_recovery_record(
    record: &HttpUploadRecoveryRecord,
) -> SessionFileHttpUploadFileIdentity {
    SessionFileHttpUploadFileIdentity {
        #[cfg(unix)]
        dev: record.dev,
        #[cfg(unix)]
        ino: record.ino,
        #[cfg(windows)]
        volume_serial_number: if record.dev == SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN {
            None
        } else {
            u32::try_from(record.dev).ok()
        },
        #[cfg(windows)]
        file_index: if record.ino == SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN {
            None
        } else {
            Some(record.ino)
        },
        #[cfg(not(any(unix, windows)))]
        modified_at_ms: Some(UnixTimestampMillis(record.dev)),
        #[cfg(not(any(unix, windows)))]
        created_at_ms: Some(UnixTimestampMillis(record.ino)),
        len: record.size_bytes,
    }
}

pub(crate) fn write_session_file_http_upload_files(
    plan: SessionFileHttpUploadWritePlan,
    chunks: impl IntoIterator<Item = Vec<u8>>,
) -> Result<SessionFileHttpUploadFileWriteResult, ProtocolError> {
    let file = plan.file;
    let mut current_offset = plan.offset_bytes;
    let mut written_ranges = Vec::new();
    let mut saw_chunk = false;
    for chunk in chunks {
        saw_chunk = true;
        let next_offset = current_offset
            .checked_add(chunk.len() as u64)
            .ok_or(ProtocolError::InvalidEnvelope)?;
        if chunk.is_empty() || next_offset > plan.size_bytes {
            tracing::debug!(
                target = %plan.target.display(),
                offset_bytes = current_offset,
                chunk_len = chunk.len(),
                next_offset,
                size_bytes = plan.size_bytes,
                "HTTP upload file write chunk is outside declared range detail"
            );
            tracing::warn!(
                offset_bytes = current_offset,
                chunk_len = chunk.len(),
                next_offset,
                size_bytes = plan.size_bytes,
                "HTTP upload file write chunk is outside declared range"
            );
            return Err(ProtocolError::InvalidEnvelope);
        }
        if range_is_fully_covered(&plan.written_ranges, current_offset, next_offset) {
            let existing = read_session_file_http_upload_range(&file, current_offset, chunk.len())?;
            if existing != chunk {
                tracing::debug!(
                    target = %plan.target.display(),
                    offset_bytes = current_offset,
                    next_offset,
                    "HTTP upload duplicate range does not match existing bytes detail"
                );
                tracing::warn!(
                    offset_bytes = current_offset,
                    next_offset,
                    "HTTP upload duplicate range does not match existing bytes"
                );
                return Err(ProtocolError::InvalidEnvelope);
            }
        } else {
            if range_overlaps(&plan.written_ranges, current_offset, next_offset) {
                tracing::debug!(
                    target = %plan.target.display(),
                    offset_bytes = current_offset,
                    next_offset,
                    written_ranges = plan.written_ranges.len(),
                    "HTTP upload file write overlaps committed range detail"
                );
                tracing::warn!(
                    offset_bytes = current_offset,
                    next_offset,
                    written_ranges = plan.written_ranges.len(),
                    "HTTP upload file write overlaps committed range"
                );
                return Err(ProtocolError::InvalidEnvelope);
            }
            write_session_file_http_upload_range(&file, current_offset, &chunk)?;
            written_ranges.push((current_offset, next_offset));
        }
        current_offset = next_offset;
    }
    http_upload_test_crash_checkpoint("after_write");
    if !saw_chunk && current_offset != plan.size_bytes {
        tracing::debug!(
            target = %plan.target.display(),
            offset_bytes = current_offset,
            size_bytes = plan.size_bytes,
            "HTTP upload empty write did not point at EOF detail"
        );
        tracing::warn!(
            offset_bytes = current_offset,
            size_bytes = plan.size_bytes,
            "HTTP upload empty write did not point at EOF"
        );
        return Err(ProtocolError::InvalidEnvelope);
    }
    ensure_session_file_http_upload_target_identity(&plan.storage_path, plan.file_identity)?;
    let metadata = file.metadata().map_err(map_file_path_error)?;
    if metadata.len() != plan.size_bytes {
        tracing::debug!(
            target = %plan.target.display(),
            actual_len = metadata.len(),
            size_bytes = plan.size_bytes,
            "HTTP upload target length changed during write detail"
        );
        tracing::warn!(
            actual_len = metadata.len(),
            size_bytes = plan.size_bytes,
            "HTTP upload target length changed during write"
        );
        return Err(ProtocolError::InvalidEnvelope);
    }
    Ok(SessionFileHttpUploadFileWriteResult {
        written_ranges,
        reserved_range: plan.reserved_range,
        modified_at_ms: metadata_modified_at_ms(&metadata),
    })
}

pub(crate) fn session_file_http_upload_chunks_len(
    chunks: &[Vec<u8>],
) -> Result<u64, ProtocolError> {
    chunks
        .iter()
        .try_fold(0_u64, |sum, chunk| sum.checked_add(chunk.len() as u64))
        .ok_or(ProtocolError::InvalidEnvelope)
}

fn range_is_fully_covered(ranges: &BTreeMap<u64, u64>, start: u64, end: u64) -> bool {
    if start == end {
        return true;
    }
    let mut cursor = start;
    while cursor < end {
        let Some((&range_start, &len)) = ranges.range(..=cursor).next_back() else {
            return false;
        };
        let Some(range_end) = range_start.checked_add(len) else {
            return false;
        };
        if range_start > cursor || range_end <= cursor {
            return false;
        }
        cursor = range_end;
    }
    true
}

fn range_overlaps(ranges: &BTreeMap<u64, u64>, start: u64, end: u64) -> bool {
    if start == end {
        return false;
    }
    if ranges
        .range(..=start)
        .next_back()
        .and_then(|(&range_start, &len)| range_start.checked_add(len))
        .is_some_and(|range_end| range_end > start)
    {
        return true;
    }
    ranges
        .range(start..)
        .next()
        .is_some_and(|(&next_start, _)| next_start < end)
}

fn read_session_file_http_upload_range(
    file: &fs::File,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = vec![0_u8; len];
    read_session_file_exact_at(file, &mut bytes, offset)?;
    Ok(bytes)
}

fn write_session_file_http_upload_range(
    file: &fs::File,
    offset: u64,
    bytes: &[u8],
) -> Result<(), ProtocolError> {
    write_session_file_all_at(file, bytes, offset)
}

#[cfg(unix)]
fn read_session_file_exact_at(
    file: &fs::File,
    bytes: &mut [u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    // 中文注释：HTTP upload 支持 2 并发分片，必须使用 positional I/O；
    // `try_clone + seek` 会共享文件游标，两个分片会互相改 offset。
    let mut read_len = 0_usize;
    while read_len < bytes.len() {
        let n = file
            .read_at(&mut bytes[read_len..], offset + read_len as u64)
            .map_err(map_file_path_error)?;
        if n == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        read_len += n;
    }
    Ok(())
}

#[cfg(windows)]
fn read_session_file_exact_at(
    file: &fs::File,
    bytes: &mut [u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    let mut read_len = 0_usize;
    while read_len < bytes.len() {
        let n = file
            .seek_read(&mut bytes[read_len..], offset + read_len as u64)
            .map_err(map_file_path_error)?;
        if n == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        read_len += n;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_session_file_exact_at(
    file: &fs::File,
    bytes: &mut [u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    let mut file = file.try_clone().map_err(map_file_path_error)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(map_file_path_error)?;
    file.read_exact(bytes).map_err(map_file_path_error)
}

#[cfg(unix)]
fn write_session_file_all_at(
    file: &fs::File,
    bytes: &[u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    let mut written = 0_usize;
    while written < bytes.len() {
        let n = file
            .write_at(&bytes[written..], offset + written as u64)
            .map_err(map_file_path_error)?;
        if n == 0 {
            return Err(ProtocolError::RuntimeFailed);
        }
        written += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_session_file_all_at(
    file: &fs::File,
    bytes: &[u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    let mut written = 0_usize;
    while written < bytes.len() {
        let n = file
            .seek_write(&bytes[written..], offset + written as u64)
            .map_err(map_file_path_error)?;
        if n == 0 {
            return Err(ProtocolError::RuntimeFailed);
        }
        written += n;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_session_file_all_at(
    file: &fs::File,
    bytes: &[u8],
    offset: u64,
) -> Result<(), ProtocolError> {
    let mut file = file.try_clone().map_err(map_file_path_error)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(map_file_path_error)?;
    file.write_all(bytes).map_err(map_file_path_error)
}

fn session_file_http_upload_temp_path(
    target: &Path,
    upload_id: &str,
) -> Result<PathBuf, ProtocolError> {
    let parent = target.parent().ok_or(ProtocolError::InvalidEnvelope)?;
    let file_name = target.file_name().ok_or(ProtocolError::InvalidEnvelope)?;
    Ok(parent.join(format!(
        ".{}.termd-http-upload-{}.part",
        file_name.to_string_lossy(),
        upload_id
    )))
}

#[cfg(target_os = "linux")]
fn publish_session_file_http_upload_noreplace(
    temp_path: &Path,
    target: &Path,
) -> Result<(), ProtocolError> {
    let temp = std::ffi::CString::new(temp_path.as_os_str().as_bytes())
        .map_err(|_| ProtocolError::InvalidEnvelope)?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| ProtocolError::InvalidEnvelope)?;
    // SAFETY: both C strings remain alive for the duration of renameat2.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            temp.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Err(ProtocolError::InvalidState)
        } else {
            Err(map_file_path_error(error))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn publish_session_file_http_upload_noreplace(
    temp_path: &Path,
    target: &Path,
) -> Result<(), ProtocolError> {
    fs::hard_link(temp_path, target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            ProtocolError::InvalidState
        } else {
            map_file_path_error(error)
        }
    })?;
    fs::remove_file(temp_path).map_err(map_file_path_error)
}

fn sync_session_file_http_upload_parent(target: &Path) -> Result<(), ProtocolError> {
    let parent = target.parent().ok_or(ProtocolError::InvalidEnvelope)?;
    #[cfg(unix)]
    {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(map_file_path_error)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn create_session_file_http_upload_target(
    target: &Path,
    size_bytes: u64,
) -> Result<(fs::File, SessionFileHttpUploadFileIdentity), ProtocolError> {
    create_session_file_http_upload_target_with_set_len(target, size_bytes, |file, size_bytes| {
        file.set_len(size_bytes)
    })
}

fn create_session_file_http_upload_target_with_set_len(
    target: &Path,
    size_bytes: u64,
    set_len: impl FnOnce(&fs::File, u64) -> std::io::Result<()>,
) -> Result<(fs::File, SessionFileHttpUploadFileIdentity), ProtocolError> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    // 中文注释：调用方只传同目录随机隐藏路径；create_new 保证不会接管已有对象。
    let file = match options.open(target) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ProtocolError::InvalidEnvelope);
        }
        Err(error) => return Err(map_file_path_error(error)),
    };
    let created_metadata = file.metadata().map_err(map_file_path_error)?;
    let created_identity = SessionFileHttpUploadFileIdentity::from_metadata(&created_metadata);
    if let Err(error) = set_len(&file, size_bytes) {
        // 中文注释：set_len 失败时只清理本次 create_new 返回的同一个临时对象。
        let _ = remove_session_file_http_upload_target(target, created_identity);
        return Err(map_file_path_error(error));
    }
    let metadata = file.metadata().map_err(map_file_path_error)?;
    let identity = SessionFileHttpUploadFileIdentity::from_metadata(&metadata);
    Ok((file, identity))
}

fn ensure_session_file_http_upload_target_identity(
    target: &Path,
    expected_identity: SessionFileHttpUploadFileIdentity,
) -> Result<(), ProtocolError> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(
                target = %target.display(),
                kind = ?error.kind(),
                "HTTP upload target identity check could not stat target detail"
            );
            tracing::warn!(
                kind = ?error.kind(),
                "HTTP upload target identity check could not stat target"
            );
            return Err(map_file_path_error(error));
        }
    };
    let actual_identity = SessionFileHttpUploadFileIdentity::from_metadata(&metadata);
    if actual_identity == expected_identity {
        Ok(())
    } else {
        tracing::debug!(
            target = %target.display(),
            expected = ?expected_identity,
            actual = ?actual_identity,
            "HTTP upload target identity changed detail"
        );
        tracing::warn!(
            expected = ?expected_identity,
            actual = ?actual_identity,
            "HTTP upload target identity changed"
        );
        Err(ProtocolError::InvalidState)
    }
}

fn session_file_http_upload_target_has_external_links(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.nlink() > 1
    }
    #[cfg(windows)]
    {
        // 中文注释：Windows 也支持 hardlink。active upload 清理前必须检查链接数，
        // 否则删除目标路径后，未完成内容仍可能通过 hardlink alias 暴露。
        return metadata
            .number_of_links()
            .map(|links| links > 1)
            .unwrap_or(true);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        true
    }
}

fn session_file_http_upload_open_file_has_remaining_links(
    file: &fs::File,
    expected_identity: SessionFileHttpUploadFileIdentity,
) -> bool {
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(
                kind = ?error.kind(),
                "HTTP upload open file link check could not stat target detail"
            );
            tracing::warn!("HTTP upload open file link check could not stat target");
            return true;
        }
    };
    let identity = SessionFileHttpUploadFileIdentity::from_metadata(&metadata);
    if !identity.is_same_filesystem_object(expected_identity) {
        return true;
    }
    #[cfg(unix)]
    {
        metadata.nlink() > 0
    }
    #[cfg(windows)]
    {
        return metadata
            .number_of_links()
            .map(|links| links > 0)
            .unwrap_or(true);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        true
    }
}

fn remove_session_file_http_upload_target(
    target: &Path,
    expected_identity: SessionFileHttpUploadFileIdentity,
) -> Result<SessionFileHttpUploadCleanupOutcome, ProtocolError> {
    remove_session_file_http_upload_target_with_mode(
        target,
        expected_identity,
        SessionFileHttpUploadCleanupIdentityMode::InMemoryOpenHandle,
    )
}

fn remove_persisted_session_file_http_upload_target(
    target: &Path,
    expected_identity: SessionFileHttpUploadFileIdentity,
) -> Result<SessionFileHttpUploadCleanupOutcome, ProtocolError> {
    remove_session_file_http_upload_target_with_mode(
        target,
        expected_identity,
        SessionFileHttpUploadCleanupIdentityMode::PersistedRecovery,
    )
}

fn remove_session_file_http_upload_target_with_mode(
    target: &Path,
    expected_identity: SessionFileHttpUploadFileIdentity,
    mode: SessionFileHttpUploadCleanupIdentityMode,
) -> Result<SessionFileHttpUploadCleanupOutcome, ProtocolError> {
    let target_metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match mode {
            SessionFileHttpUploadCleanupIdentityMode::InMemoryOpenHandle => {
                return Ok(SessionFileHttpUploadCleanupOutcome::AlreadyGone);
            }
            // 中文注释：启动 recovery 没有 open file handle；target 缺失时不能证明
            // 原 active 对象没有 hardlink alias，必须保留 recovery record。
            SessionFileHttpUploadCleanupIdentityMode::PersistedRecovery => {
                return Err(ProtocolError::InvalidState);
            }
        },
        Err(error) => return Err(map_file_path_error(error)),
    };
    let target_identity = SessionFileHttpUploadFileIdentity::from_metadata(&target_metadata);
    match session_file_http_upload_cleanup_identity_match(target_identity, expected_identity, mode)?
    {
        SessionFileHttpUploadCleanupIdentityMatch::Same => {}
        SessionFileHttpUploadCleanupIdentityMatch::Replaced => {
            return Ok(SessionFileHttpUploadCleanupOutcome::TargetReplaced);
        }
    }
    if session_file_http_upload_target_has_external_links(&target_metadata) {
        // 中文注释：如果 active upload 目标有 hardlink alias，删除原路径并不能删除
        // 未完成内容。必须保留 active/recovery 状态继续隔离，等 alias 被移除后再清理。
        return Err(ProtocolError::InvalidState);
    }
    match fs::remove_file(target) {
        Ok(()) => Ok(SessionFileHttpUploadCleanupOutcome::Removed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SessionFileHttpUploadCleanupOutcome::AlreadyGone)
        }
        Err(error) => Err(map_file_path_error(error)),
    }
}

fn metadata_modified_at_ms(metadata: &fs::Metadata) -> Option<UnixTimestampMillis> {
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let millis = duration.as_millis().min(u128::from(u64::MAX)) as u64;
    Some(UnixTimestampMillis(millis))
}

#[cfg(not(any(unix, windows)))]
fn metadata_created_at_ms(metadata: &fs::Metadata) -> Option<UnixTimestampMillis> {
    let duration = metadata.created().ok()?.duration_since(UNIX_EPOCH).ok()?;
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
