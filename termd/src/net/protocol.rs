//! termd daemon 的 WebSocket 协议状态机核心。
//!
//! 本模块不依赖真实 socket，便于单元测试直接驱动 hello、E2EE、pair/auth 和 session
//! 操作。Axum 只负责把网络帧转成这里的统一 envelope。

mod recovery;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{FileExt as WindowsFileExt, MetadataExt as WindowsMetadataExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use base64::{
    Engine as _,
    engine::general_purpose::{self, URL_SAFE_NO_PAD},
};
use rand_core::{OsRng, RngCore};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use termd_proto::{
    AttachFramePayload, AttachRole, AuthChallengePayload, AuthPayload, BINARY_PROTOCOL_VERSION,
    ClientHelloPayload, ClientId, ControlGrantPayload, ControlRequestPayload,
    DaemonClientForgetPayload, DaemonClientForgotPayload, DaemonClientSummaryPayload,
    DaemonClientsPayload, DaemonClientsResultPayload, DaemonStatusPayload,
    DaemonStatusResultPayload, DeviceId, E2eeKeyExchangePayload, EncryptedFramePayload, Envelope,
    ErrorPayload, HelloPayload, HttpE2eeAuthPayload, METHOD_AUTH, METHOD_AUTH_SESSION_TOKEN,
    METHOD_AUTH_VERIFY, METHOD_CLIENT_HELLO, METHOD_CONTROL_REQUEST, METHOD_DAEMON_CLIENT_FORGET,
    METHOD_DAEMON_CLIENTS, METHOD_DAEMON_STATUS, METHOD_PAIR_REQUEST, METHOD_PING,
    METHOD_SESSION_ATTACH, METHOD_SESSION_CLOSE, METHOD_SESSION_CREATE, METHOD_SESSION_CURSOR,
    METHOD_SESSION_FILE_DELETE, METHOD_SESSION_FILE_DOWNLOAD_CHUNK,
    METHOD_SESSION_FILE_DOWNLOAD_PREPARE, METHOD_SESSION_FILE_DOWNLOAD_STREAM,
    METHOD_SESSION_FILE_READ, METHOD_SESSION_FILE_UPLOAD_STREAM, METHOD_SESSION_FILE_WRITE,
    METHOD_SESSION_FILES, METHOD_SESSION_GIT, METHOD_SESSION_GIT_ACTION, METHOD_SESSION_GIT_DIFF,
    METHOD_SESSION_LIST, METHOD_SESSION_RENAME, METHOD_SESSION_REORDER, METHOD_SESSION_RESIZE,
    METHOD_SESSION_SEARCH, METHOD_TERMINAL_ATTACH, METHOD_TERMINAL_CREATE, MessageType, Nonce,
    PROTOCOL_PACKET_VERSION, PacketErrorPayload, PacketKind, PacketRequestId, PacketStreamId,
    PairRequestPayload, PingPayload, PongPayload, ProtocolPacket, ProtocolVersion, ServerId,
    SessionActivityPayload, SessionAttachPayload, SessionAttachedPayload, SessionClosePayload,
    SessionClosedPayload, SessionCreatePayload, SessionCreatedPayload, SessionCursorPayload,
    SessionCwdChangedPayload, SessionDataPayload, SessionFileDeletePayload,
    SessionFileDeletedPayload, SessionFileDownloadChunkPayload,
    SessionFileDownloadChunkResultPayload, SessionFileDownloadPreparePayload,
    SessionFileDownloadReadyPayload, SessionFileDownloadStreamPayload,
    SessionFileDownloadStreamReadyPayload, SessionFileEntryPayload, SessionFileHttpDownloadPayload,
    SessionFileHttpUploadReadyPayload, SessionFileHttpUploadStreamPayload, SessionFileKind,
    SessionFileReadPayload, SessionFileReadResultPayload, SessionFileTransferChunkPayload,
    SessionFileUploadPayload, SessionFileUploadProgressPayload, SessionFileUploadReadyPayload,
    SessionFileWritePayload, SessionFileWrittenPayload, SessionFilesPayload,
    SessionFilesResultPayload, SessionGitActionKind, SessionGitActionPayload,
    SessionGitActionResultPayload, SessionGitDiffPayload, SessionGitDiffResultPayload,
    SessionGitFileChangePayload, SessionGitPayload, SessionGitResultPayload,
    SessionGitWorktreePayload, SessionId, SessionListPayload, SessionListResultPayload,
    SessionRenamePayload, SessionRenamedPayload, SessionReorderPayload, SessionReorderedPayload,
    SessionResizePayload, SessionResizedPayload, SessionScopeGrantPayload, SessionSearchPayload,
    SessionState, SessionSummaryPayload, SessionTokenGrantPayload, TerminalSize,
    UnixTimestampMillis, attach_frame_payload_value, decode_binary_protocol_packet,
    encode_binary_protocol_packet, packet_event_method_for_message, protocol_packet_from_binary,
    protocol_packet_to_binary,
};
#[cfg(test)]
use termd_proto::{
    METHOD_SESSION_SCOPE_TOKEN, SessionSearchMatchPayload, SessionSearchResultPayload,
    TerminalFramePayload,
};
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

use crate::auth::{
    AuthChallengeManager, ChallengeResponseService, DaemonE2eeSigningInput, DaemonIdentity,
    DaemonPublicIdentity, DeviceIdentity, E2eeAuthTranscript, HttpE2eeSigningInput,
    InMemoryTrustedDeviceStore, PairingService, PairingTokenManager, ReplayProtector,
    SessionScopeManager, SessionTokenManager, SignatureVerifier, TrustedDevice, TrustedDeviceStore,
    current_unix_timestamp_millis,
};
use crate::config::DaemonConfig;
use crate::pty::{
    CommandSpec, PtyAttachmentBootstrap, PtyBackend, PtyRestoreInfo, PtySupervisorStatus,
};
use crate::runtime::{RuntimeError, SessionRuntime};
use crate::session::{
    AttachRole as RuntimeAttachRole, SessionState as RuntimeSessionState,
    TerminalSize as RuntimeTerminalSize,
};
use crate::state::{
    DaemonIdentitySnapshot, DaemonState, HttpUploadRecoveryRecord, SessionStateRecord, StateError,
    StateStore, TrustedDeviceState,
    client_history::{ClientHistoryRecord, ClientHistoryStore, SessionHistoryRecord},
};

#[cfg(test)]
use super::screen::TerminalScreen;
use super::{
    E2eeError, E2eeKeyPair, E2eePeerPublicKey, E2eeSession, E2eeSessionContext, E2eeSessionRole,
    decode_binary_encrypted_frame, encode_binary_encrypted_frame,
};

const AUTH_CHALLENGE_TTL_MS: u64 = 60_000;
#[cfg(test)]
const LIVE_OUTPUT_MIN_BYTES: usize = 16 * 1024;
#[cfg(test)]
const LIVE_OUTPUT_BYTES_PER_CELL: usize = 8;
// 中文注释：supervisor 会按 PTY read 边界生成 terminal frame，很多命令会变成
// “一行一个 frame”。live drain 不能只取几个小 frame，否则 relay/Web 会看到逐行蹦。
// 真正的上限仍由下面的 MB 级 payload/transport budget 控制。
const LIVE_OUTPUT_DRAIN_MAX_CHUNKS: usize = 512;
const RAW_OUTPUT_BATCH_MAX_CHUNKS: usize = 8;
const TERMINAL_STREAM_BATCH_MAX_BYTES: usize = 512 * 1024;
#[cfg(test)]
const TERMINAL_STREAM_BATCH_MAX_TRANSPORT_BYTES: usize = 768 * 1024;
#[cfg(test)]
#[allow(dead_code)]
const TERMINAL_STREAM_BATCH_TRANSPORT_OVERHEAD_BYTES: usize = 128;
#[cfg(test)]
#[allow(dead_code)]
const TERMINAL_STREAM_FRAME_TRANSPORT_OVERHEAD_BYTES: usize = 256;
const TERMINAL_STREAM_METADATA_CREDIT_BYTES: usize = 1;
#[cfg(test)]
const SESSION_TERMINAL_CWD_PROBE_MIN_INTERVAL_MS: u64 = 1_000;
const SESSION_FILE_DOWNLOAD_TOKEN_TTL_MS: u64 = 60_000;
const SESSION_FILE_DOWNLOAD_GRANT_LIMIT: usize = 128;
const SESSION_FILE_HTTP_UPLOAD_ACTIVE_IDLE_TTL_MS: u64 = 60 * 60 * 1000;
const SESSION_FILE_HTTP_UPLOAD_TOMBSTONE_TTL_MS: u64 = 10 * 60 * 1000;
#[cfg(windows)]
const SESSION_FILE_HTTP_UPLOAD_IDENTITY_UNKNOWN: u64 = u64::MAX;
// 中文注释：RPC file_read/file_write 只服务浏览器内置文本编辑器；大文件传输必须走
// HTTP E2EE 或 binary stream，避免 JSON/base64 RPC 重新变成大文件通道。
const SESSION_FILE_RPC_MAX_BYTES: usize = 1024 * 1024;
const SESSION_FILE_READ_MAX_BYTES: u64 = SESSION_FILE_RPC_MAX_BYTES as u64;
const SESSION_FILE_WRITE_MAX_BYTES: usize = SESSION_FILE_RPC_MAX_BYTES;
const SESSION_FILE_WRITE_MAX_BASE64_BYTES: usize = ((SESSION_FILE_WRITE_MAX_BYTES + 2) / 3) * 4;
const SESSION_FILE_DOWNLOAD_CHUNK_MAX_BYTES: u32 = 256 * 1024;
const SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES: u32 = 256 * 1024;

/// 协议层统一使用的 JSON envelope。
pub type JsonEnvelope = Envelope<Value>;

/// WebSocket 上的真实传输帧。握手继续是 JSON；E2EE 后的 packet 可切到 binary。
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

/// 0.2.0 packet terminal stream 的连接内状态。
///
/// stream id 只在单条 E2EE 连接内有效；它把 terminal attach 的 request/response、后续
/// input/output chunk 和 cancel 都绑定到同一个 session。旧协议里的 flow/credit 字段仍会被
/// 解码和统计，但不再作为 terminal 输出闸门；连接级背压交给 WebSocket/TCP 和外层 bounded queue。
#[derive(Debug, Clone)]
struct PacketTerminalStream {
    session_id: SessionId,
    next_input_seq: u64,
    next_output_seq: u64,
}

impl PacketTerminalStream {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            next_input_seq: 1,
            next_output_seq: 1,
        }
    }
}

/// terminal stream 替换前的连接内状态快照。
///
/// 中文注释：packet terminal stream-open 可能在 payload 已解码后，因 session 权限、
/// session 状态或内部响应转换失败而返回错误。替换旧 stream 前保存这组字段，失败时
/// 精确回滚，避免一次坏请求把仍可用的 terminal 输出订阅和 deferred wakeup 清掉。
#[derive(Debug, Clone)]
struct PacketTerminalStreamStateSnapshot {
    state: ProtocolConnectionState,
    packet_terminal_streams: HashMap<PacketStreamId, PacketTerminalStream>,
    packet_terminal_streams_by_session: HashMap<SessionId, PacketStreamId>,
    attached_sessions: Vec<SessionId>,
    watched_sessions: HashSet<SessionId>,
    watched_cwd_versions: HashMap<SessionId, u64>,
    watched_cwd_paths: HashMap<SessionId, Option<String>>,
    watched_attachment_ids: HashMap<SessionId, String>,
    next_watched_attachment_number: u64,
    stale_watched_sessions: HashSet<SessionId>,
    pending_attach_frames: HashMap<SessionId, VecDeque<Vec<u8>>>,
    output_offsets: HashMap<SessionId, u64>,
    pending_outputs: HashMap<SessionId, VecDeque<Vec<u8>>>,
    deferred_output_wakeups: HashSet<SessionId>,
}

#[derive(Debug, Clone)]
struct PacketFileUploadStream {
    session_id: SessionId,
    path: PathBuf,
    temp_path: PathBuf,
    size_bytes: u64,
    offset_bytes: u64,
    next_input_seq: u64,
    next_output_seq: u64,
}

#[derive(Debug)]
struct PacketFileDownloadStream {
    session_id: SessionId,
    file: fs::File,
    size_bytes: u64,
    offset_bytes: u64,
    next_output_seq: u64,
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
    file: fs::File,
    upload_id: String,
    size_bytes: u64,
    file_identity: SessionFileHttpUploadFileIdentity,
    status: SessionFileHttpUploadStatus,
    written_ranges: BTreeMap<u64, u64>,
    inflight_ranges: BTreeMap<u64, u64>,
    modified_at_ms: Option<UnixTimestampMillis>,
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
        if let Some((&next_start, _)) = self.written_ranges.range(start..).next() {
            if next_start < end {
                return Err(ProtocolError::InvalidEnvelope);
            }
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
            return self.dev == other.dev && self.ino == other.ino;
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

#[cfg(test)]
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

#[cfg(test)]
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
        let retained = self.bytes.iter().copied().collect::<Vec<_>>();
        self.screen = TerminalScreen::new(size.rows, size.cols);
        if !retained.is_empty() {
            self.screen.apply(&retained);
        }
        self.trim_to_live_output_limit();
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        self.screen.snapshot_bytes()
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

    fn has_after(&self, cursor: u64) -> bool {
        cursor.max(self.base_offset) < self.end_offset()
    }
}

fn raw_output_has_more_pending<B, V>(
    protocol: &DaemonProtocol<B, V>,
    connection: &ProtocolConnection,
    session_id: SessionId,
    _internal_session_id: &str,
) -> bool
where
    B: PtyBackend,
    V: SignatureVerifier,
{
    if connection
        .pending_outputs
        .get(&session_id)
        .is_some_and(|pending| !pending.is_empty())
    {
        return true;
    }

    #[cfg(not(test))]
    {
        let _ = protocol;
        false
    }

    #[cfg(test)]
    {
        let cursor = connection
            .output_offsets
            .get(&session_id)
            .copied()
            .unwrap_or_else(|| {
                protocol
                    .session_output_history
                    .get(&session_id)
                    .map(SessionOutputHistory::base_offset)
                    .unwrap_or(0)
            });

        protocol
            .session_output_history
            .get(&session_id)
            .is_some_and(|history| history.has_after(cursor))
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
    session_token_manager: SessionTokenManager,
    session_scope_manager: SessionScopeManager,
    trusted_store: InMemoryTrustedDeviceStore,
    runtime: SessionRuntime<B>,
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
    session_cwd_signals: HashMap<SessionId, watch::Sender<u64>>,
    session_resize_signals: HashMap<SessionId, watch::Sender<TerminalSize>>,
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
            session_token_manager: SessionTokenManager::new(),
            session_scope_manager: SessionScopeManager::new(),
            trusted_store,
            runtime: SessionRuntime::new(backend),
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
            session_cwd_signals: HashMap::new(),
            session_resize_signals: HashMap::new(),
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

    /// 清理单个已确认关闭的 session 记录。只在 PTY terminate 成功后使用。
    fn prune_closed_session(&mut self, session_id: SessionId) -> Result<(), StateError> {
        self.client_history.prune_closed_session(session_id)?;
        StateStore::prune_closed_session(&self.config.state_path, session_id)?;
        Ok(())
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

    pub fn e2ee_public_key(&self) -> E2eePeerPublicKey {
        self.e2ee_keypair.public_key()
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub fn open_http_e2ee_session(
        &mut self,
        auth: HttpE2eeAuthPayload,
    ) -> Result<(DeviceId, E2eeSession), ProtocolError> {
        let now_ms = current_unix_timestamp_millis();
        let trusted = self
            .trusted_store
            .require_trusted(&auth.device_id)
            .map_err(|_| ProtocolError::AuthFailed)?
            .clone();
        self.auth_service
            .replay_protector_mut()
            .check(&auth.device_id, &auth.nonce, auth.timestamp_ms, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        let signing_input =
            HttpE2eeSigningInput::from_payload(&auth, self.daemon_public_identity()).to_bytes();
        self.verifier
            .verify(trusted.public_key(), &signing_input, &auth.signature)
            .map_err(|_| ProtocolError::AuthFailed)?;
        self.auth_service.replay_protector_mut().record_checked(
            &auth.device_id,
            &auth.nonce,
            now_ms,
        );
        self.trusted_store
            .mark_seen(&auth.device_id, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;

        let peer_public_key = E2eePeerPublicKey::try_from(&auth.e2ee_public_key)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;
        let context = E2eeSessionContext::new(
            self.server_id(),
            auth.device_id,
            self.e2ee_keypair.public_key(),
            peer_public_key,
        );
        let e2ee = E2eeSession::new(
            E2eeSessionRole::Daemon,
            &self.e2ee_keypair,
            peer_public_key,
            context,
        )
        .map_err(|_| ProtocolError::InvalidEnvelope)?;

        Ok((auth.device_id, e2ee))
    }

    #[cfg(test)]
    pub(crate) fn runtime_write_input_as_device_for_test(
        &mut self,
        session_id: SessionId,
        device_id: DeviceId,
        bytes: &[u8],
    ) -> Result<(), ProtocolError> {
        // 中文注释：测试专用探针，用 runtime 权限面确认临时 HTTP attach 是否已经被清理。
        // 这里故意绕过 ProtocolConnection 的 attached_sessions，直接检查 runtime 里是否还认这个设备。
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .cloned()
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .write_input(&internal_session_id, &device_key(device_id), bytes)
            .map_err(map_runtime_error)
    }

    #[cfg(test)]
    pub(crate) fn client_history_active_connection_count_for_test(
        &self,
        device_id: DeviceId,
    ) -> Result<Option<i64>, StateError> {
        self.client_history
            .active_connection_count_for_test(device_id)
    }

    /// 本地 CLI 或测试可通过服务层签发 token；WebSocket 不暴露 token 签发入口。
    pub fn issue_pairing_token(
        &mut self,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::PairingResult<crate::auth::PairingTokenRecord> {
        self.pairing_service
            .issue_token(now_ms, self.config.pairing_token_ttl_ms)
    }

    /// 为已认证设备签发短期 session token。
    pub fn issue_session_token(
        &mut self,
        device_id: DeviceId,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::SessionTokenResult<crate::auth::SessionTokenRecord> {
        self.session_token_manager.issue(
            self.server_id(),
            device_id,
            now_ms,
            self.config.pairing_token_ttl_ms,
        )
    }

    /// 为某个 session 的 HTTP control plane 签发短期 scope token。
    pub fn issue_session_scope_token(
        &mut self,
        device_id: DeviceId,
        session_id: SessionId,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::SessionTokenResult<crate::auth::SessionScopeRecord> {
        self.session_scope_manager.issue(
            self.server_id(),
            device_id,
            session_id,
            now_ms,
            self.config.pairing_token_ttl_ms,
        )
    }

    pub fn verify_session_token(
        &mut self,
        token: &termd_proto::SessionToken,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::SessionTokenResult<crate::auth::SessionTokenRecord> {
        self.session_token_manager.verify(token, now_ms)
    }

    pub fn verify_session_scope_token(
        &mut self,
        token: &termd_proto::SessionToken,
        now_ms: UnixTimestampMillis,
    ) -> crate::auth::SessionTokenResult<crate::auth::SessionScopeRecord> {
        self.session_scope_manager.verify(token, now_ms)
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
        connection.attach(session_id, 0, Vec::new(), false);
        connection.state = ProtocolConnectionState::Attached;
        Ok(())
    }

    fn issue_session_scope_grant(
        &mut self,
        device_id: DeviceId,
        session_id: SessionId,
    ) -> Result<SessionScopeGrantPayload, ProtocolError> {
        let now_ms = current_unix_timestamp_millis();
        let grant = self
            .issue_session_scope_token(device_id, session_id, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(SessionScopeGrantPayload {
            session_id: grant.session_id(),
            token: grant.token().clone(),
            expires_at_ms: grant.expires_at_ms(),
        })
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
        let now_ms = current_unix_timestamp_millis();
        let daemon_public_identity = self.daemon_identity.public_identity();
        let mut server_key_exchange = E2eeKeyExchangePayload::new(
            self.server_id(),
            // server 尚不知道真实 device id；该字段在客户端回应时才作为 E2EE context 使用。
            DeviceId::default(),
            self.e2ee_keypair.public_key_wire(),
            nonce(),
            now_ms,
        );
        server_key_exchange.packet_version = Some(ProtocolVersion(PROTOCOL_PACKET_VERSION));
        server_key_exchange.binary_version = Some(ProtocolVersion(BINARY_PROTOCOL_VERSION));
        let signing_input =
            DaemonE2eeSigningInput::from_payload(&server_key_exchange, &daemon_public_identity)
                .to_bytes();
        let signature = self
            .daemon_identity
            .sign_to_wire(&signing_input)
            .expect("persisted daemon identity should sign E2EE key exchange");
        server_key_exchange = server_key_exchange.with_signature(signature);

        let mut connection = ProtocolConnection::new(peer_ip);
        connection.daemon_e2ee_exchange = Some(server_key_exchange.clone());
        let messages = vec![
            envelope_value(
                MessageType::Hello,
                HelloPayload {
                    protocol_version: ProtocolVersion(PROTOCOL_PACKET_VERSION),
                    nonce: nonce(),
                    timestamp_ms: now_ms,
                    server_id: Some(self.server_id()),
                    device_id: None,
                },
            )
            .expect("hello payload should serialize"),
            envelope_value(MessageType::E2eeKeyExchange, server_key_exchange)
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

        let device_key_exchange = payload.clone();
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
        connection.packet_mode = payload
            .packet_version
            .is_some_and(|version| version.0 == PROTOCOL_PACKET_VERSION);
        connection.binary_mode = connection.packet_mode
            && connection
                .daemon_e2ee_exchange
                .as_ref()
                .and_then(|exchange| exchange.binary_version)
                .is_some_and(|version| version.0 == BINARY_PROTOCOL_VERSION)
            && payload
                .binary_version
                .is_some_and(|version| version.0 == BINARY_PROTOCOL_VERSION);
        connection.device_e2ee_exchange = Some(device_key_exchange.clone());
        connection.e2ee_auth_transcript =
            connection
                .daemon_e2ee_exchange
                .as_ref()
                .map(|daemon_key_exchange| {
                    E2eeAuthTranscript::from_key_exchanges(
                        daemon_key_exchange,
                        &device_key_exchange,
                        self.daemon_public_identity(),
                    )
                });
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
            .authenticate_with_transcript(
                payload,
                current_unix_timestamp_millis(),
                &mut self.trusted_store,
                &self.verifier,
                connection
                    .packet_mode
                    .then_some(())
                    .and_then(|_| connection.e2ee_auth_transcript.as_ref()),
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

    fn issue_session_token_grant(
        &mut self,
        connection: &ProtocolConnection,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let device_id = connection.authenticated_device_id()?;
        let now_ms = current_unix_timestamp_millis();
        let grant = self
            .issue_session_token(device_id, now_ms)
            .map_err(|_| ProtocolError::AuthFailed)?;
        Ok(vec![envelope_value(
            MessageType::SessionTokenGrant,
            SessionTokenGrantPayload {
                server_id: grant.server_id(),
                device_id: grant.device_id(),
                token: grant.token().clone(),
                expires_at_ms: grant.expires_at_ms(),
            },
        )?])
    }

    fn create_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionCreatePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        self.create_session_inner(connection, payload, false)
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
        self.runtime
            .create_session_with_id(&wire_session_id.0.to_string(), command, runtime_size)
            .map_err(map_runtime_error)?;
        let internal_session_id = wire_session_id.0.to_string();
        let create_result = (|| -> Result<Vec<JsonEnvelope>, ProtocolError> {
            let session_name = self.default_created_session_name(wire_session_id);

            self.session_index
                .insert(wire_session_id, internal_session_id.clone());
            self.session_names
                .insert(wire_session_id, session_name.clone());
            self.session_roots
                .insert(wire_session_id, session_root.clone());
            #[cfg(test)]
            if !connection.packet_mode {
                self.session_output_history_mut(wire_session_id, payload.size);
            }
            let (cwd_signal, _) = watch::channel(0);
            self.session_cwd_signals.insert(wire_session_id, cwd_signal);
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
            let response_size = self.runtime_size_proto(&internal_session_id)?;
            let (output_offset, initial_output) = if connection.packet_mode {
                if enqueue_terminal_snapshot {
                    // 中文注释：opaque attach path 由 watched attachment 在 bootstrap 阶段
                    // 直接向 supervisor 请求首屏 snapshot/tail，这里不再维护 daemon 侧
                    // terminal drain cursor。
                }
                (0, Vec::new())
            } else {
                #[cfg(test)]
                {
                    self.drain_runtime_output_to_history_until_empty(
                        wire_session_id,
                        &internal_session_id,
                        16 * 1024,
                    )?;
                    self.output_history_attach_snapshot(wire_session_id, response_size)
                }
                #[cfg(not(test))]
                {
                    (0, Vec::new())
                }
            };
            let response_state = self.runtime_state_proto(&internal_session_id)?;
            let response = SessionCreatedPayload {
                session_id: wire_session_id,
                name: Some(session_name),
                role: wire_role,
                state: response_state,
                size: response_size,
                resize_owner: true,
            };
            self.client_history.record_session_runtime_state(
                wire_session_id,
                response_state,
                response_size,
                current_unix_timestamp_millis(),
            )?;

            self.persist_state()?;
            let response_envelope = envelope_value(MessageType::SessionCreated, response)?;
            let pending_watched_attachment = self.start_watched_attachment(
                connection,
                wire_session_id,
                &internal_session_id,
                response_size,
                PtyAttachmentBootstrap::default(),
            )?;
            let scope_grant = envelope_value(
                MessageType::SessionScopeGrant,
                self.issue_session_scope_grant(device_id, wire_session_id)?,
            )?;
            connection.attach(wire_session_id, output_offset, initial_output, true);
            self.commit_watched_attachment_start(connection, pending_watched_attachment);
            self.record_daemon_client_attach(wire_session_id, connection, device_id);
            connection.state = ProtocolConnectionState::Attached;
            Ok(vec![response_envelope, scope_grant])
        })();
        if create_result.is_err() {
            self.rollback_created_session(wire_session_id, &internal_session_id);
        }
        create_result
    }

    pub(crate) fn attach_session_permission(
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
        let attach_result = (|| -> Result<(JsonEnvelope, JsonEnvelope), ProtocolError> {
            let response_size = self.runtime_size_proto(&internal_session_id)?;
            let response_state = self.runtime_state_proto(&internal_session_id)?;
            self.client_history.record_session_runtime_state(
                payload.session_id,
                response_state,
                response_size,
                current_unix_timestamp_millis(),
            )?;
            let response_envelope = envelope_value(
                MessageType::SessionAttached,
                SessionAttachedPayload {
                    session_id: payload.session_id,
                    role: wire_role,
                    state: response_state,
                    size: response_size,
                    resize_owner: false,
                },
            )?;
            let scope_grant = envelope_value(
                MessageType::SessionScopeGrant,
                self.issue_session_scope_grant(device_id, payload.session_id)?,
            )?;
            Ok((response_envelope, scope_grant))
        })();
        let (response_envelope, scope_grant) = match attach_result {
            Ok(result) => result,
            Err(error) => {
                if !was_runtime_attached {
                    // 中文注释：permission-only attach 也会给 supervisor/runtime 增加
                    // operator 角色；任何后续响应构造或历史写入失败，都必须撤销本次新增
                    // attach，避免 HTTP/WS 调用方拿到错误但 runtime 仍认为该设备在线。
                    let _ = self
                        .runtime
                        .detach(&internal_session_id, &runtime_device_key);
                }
                return Err(error);
            }
        };
        connection.attach(payload.session_id, 0, Vec::new(), false);
        // 中文注释：permission-only WebSocket 虽然不订阅终端输出，但它仍是在线
        // attached 连接。detach_connection 依赖 active_connections 判断是否还有同设备
        // 连接持有设备级 operator，必须把它纳入同一张表。HTTP scoped 短连接会在
        // record_daemon_client_attach 内因 track_daemon_client_history=false 自动跳过。
        self.record_daemon_client_attach(payload.session_id, connection, device_id);
        connection.state = ProtocolConnectionState::Attached;

        Ok(vec![response_envelope, scope_grant])
    }

    pub(crate) fn attach_session(
        &mut self,
        connection: &mut ProtocolConnection,
        payload: SessionAttachPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        if payload.watch_updates {
            self.attach_terminal_session(connection, payload)
        } else {
            self.attach_session_permission(connection, payload)
        }
    }

    pub(crate) fn attach_terminal_session(
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
                let (output_offset, initial_output) = if connection.packet_mode {
                    (0, Vec::new())
                } else {
                    #[cfg(test)]
                    {
                        // 中文注释：legacy envelope attach 仍沿用 daemon 侧 plain-text snapshot，
                        // 只为了覆盖旧测试和非 packet 调试路径；生产 packet attach 不再走这里。
                        self.drain_runtime_output_to_history_until_empty(
                            payload.session_id,
                            &internal_session_id,
                            16 * 1024,
                        )?;
                        self.output_history_attach_snapshot(payload.session_id, response_size)
                    }
                    #[cfg(not(test))]
                    {
                        (0, Vec::new())
                    }
                };
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
            let scope_grant = envelope_value(
                MessageType::SessionScopeGrant,
                self.issue_session_scope_grant(device_id, payload.session_id)?,
            )?;
            Ok(vec![response_envelope, scope_grant])
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

        connection.attach(payload.session_id, output_offset, initial_output, true);
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

    #[cfg(test)]
    #[allow(dead_code)]
    fn write_session_data(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionDataPayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let attached = self.require_existing_attached_session(connection, payload.session_id)?;
        let bytes = general_purpose::STANDARD
            .decode(payload.data_base64)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;

        self.runtime
            .write_input(
                &attached.internal_session_id,
                &device_key(attached.device_id),
                &bytes,
            )
            .map_err(map_runtime_error)?;

        Ok(Vec::new())
    }

    fn resize_session(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionResizePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        let attached = self.require_existing_attached_session(connection, payload.session_id)?;

        self.runtime
            .resize(
                &attached.internal_session_id,
                proto_size_to_runtime(payload.size),
            )
            .map_err(map_runtime_error)?;
        #[cfg(test)]
        if !connection.packet_mode {
            if let Some(history) = self.session_output_history.get_mut(&payload.session_id) {
                history.resize(payload.size);
            }
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
        connection.authenticated_device_id()?;
        if payload.row == 0 || payload.col == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let attached = self.require_existing_attached_session(connection, payload.session_id)?;

        let now_ms = current_unix_timestamp_millis();
        let record = self
            .daemon_clients
            .entry(attached.device_id)
            .or_insert_with(|| DaemonClientRecord {
                client_id: stable_client_id_for_device(attached.device_id),
                device_id: attached.device_id,
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
        self.require_attached_session(connection, payload.session_id)?;
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
        let attached = self.require_attached_session(connection, payload.session_id)?;

        let close_result = self.runtime.close(&attached.internal_session_id);
        if let Err(error) = &close_result {
            tracing::warn!(
                %error,
                session_id = %payload.session_id.0,
                "failed to terminate runtime session during explicit close"
            );
            let _ = self.runtime.discard(&attached.internal_session_id);
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
        if let Err(error) = StateStore::record_runtime_session_closed(
            &self.config.state_path,
            payload.session_id,
            current_unix_timestamp_millis(),
        ) {
            tracing::warn!(%error, "failed to mark closed runtime session tombstone");
        }
        if close_result.is_ok() {
            if let Err(error) = self.prune_closed_session(payload.session_id) {
                tracing::warn!(%error, "failed to prune closed session records after close");
            }
        }

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

    fn rollback_created_session(&mut self, session_id: SessionId, internal_session_id: &str) {
        // 中文注释：create 失败后客户端不会持有这个 session，daemon 也不能留下
        // hidden runtime。先尽力关闭 host；如果 terminate 失败，也要丢弃 runtime 句柄。
        if self.runtime.close(internal_session_id).is_err() {
            let _ = self.runtime.discard(internal_session_id);
        }
        self.close_visible_session_state(session_id);
        let now_ms = current_unix_timestamp_millis();
        let _ = self
            .client_history
            .record_session_closed(session_id, now_ms);
        let _ = self.client_history.remove_session_attachments(session_id);
        let _ =
            StateStore::record_runtime_session_closed(&self.config.state_path, session_id, now_ms);
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

        #[cfg(not(test))]
        {
            let _ = attached;
            return Err(ProtocolError::InvalidState);
        }

        #[cfg(test)]
        {
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

    fn prepare_session_file_upload_stream(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileUploadPayload,
    ) -> Result<(SessionFileUploadReadyPayload, PacketFileUploadStream), ProtocolError> {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_writable_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        if target.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let parent = target.parent().ok_or(ProtocolError::InvalidEnvelope)?;
        let file_name = target.file_name().ok_or(ProtocolError::InvalidEnvelope)?;
        let temp_name = format!(
            ".{}.termd-upload-{}.part",
            file_name.to_string_lossy(),
            Uuid::new_v4()
        );
        let temp_path = parent.join(temp_name);
        // 中文注释：上传先写同目录临时文件，完整收到后再 rename，避免中途失败留下半个目标文件。
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(map_file_path_error)?;

        Ok((
            SessionFileUploadReadyPayload {
                session_id: payload.session_id,
                path: absolute_path_string(&target),
                size_bytes: payload.size_bytes,
                offset_bytes: 0,
            },
            PacketFileUploadStream {
                session_id: payload.session_id,
                path: target,
                temp_path,
                size_bytes: payload.size_bytes,
                offset_bytes: 0,
                next_input_seq: 1,
                next_output_seq: 1,
            },
        ))
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
        let (file, file_identity) =
            create_session_file_http_upload_target(&target, payload.size_bytes)?;
        if let Err(error) = StateStore::record_http_upload(
            &self.config.state_path,
            &HttpUploadRecoveryRecord {
                upload_id: upload_id.clone(),
                target_path: target.clone(),
                size_bytes: payload.size_bytes,
                dev: file_identity.dev(),
                ino: file_identity.ino(),
                updated_at_ms: current_unix_timestamp_millis(),
            },
        ) {
            tracing::debug!(%error, "failed to persist HTTP upload recovery record detail");
            tracing::warn!("failed to persist HTTP upload recovery record");
            let cleanup = remove_session_file_http_upload_target(&target, file_identity);
            let keep_guard = match cleanup {
                Ok(SessionFileHttpUploadCleanupOutcome::Removed) => false,
                Ok(
                    SessionFileHttpUploadCleanupOutcome::AlreadyGone
                    | SessionFileHttpUploadCleanupOutcome::TargetReplaced,
                ) => session_file_http_upload_open_file_has_remaining_links(&file, file_identity),
                Err(cleanup_error) => {
                    tracing::debug!(
                        %cleanup_error,
                        target = %target.display(),
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
                        file,
                        upload_id: upload_id.clone(),
                        size_bytes: payload.size_bytes,
                        file_identity,
                        status: SessionFileHttpUploadStatus::Active,
                        written_ranges: BTreeMap::new(),
                        inflight_ranges: BTreeMap::new(),
                        modified_at_ms: None,
                        updated_at_ms: now_ms,
                    },
                );
            }
            return Err(ProtocolError::StateFailed);
        }
        let now_ms = current_unix_timestamp_millis().0;
        // 中文注释：HTTP upload 的 init 就是文件创建事务：直接在最终目标路径创建新文件，
        // 并把长度设置为声明大小。后续 POST 只按 offset seek 写目标文件，不再生成 .part/.chunk。
        self.session_file_http_uploads.insert(
            upload_id.clone(),
            SessionFileHttpUploadState {
                session_id: payload.session_id,
                target: target.clone(),
                file,
                upload_id: upload_id.clone(),
                size_bytes: payload.size_bytes,
                file_identity,
                status: SessionFileHttpUploadStatus::Active,
                written_ranges: BTreeMap::new(),
                inflight_ranges: BTreeMap::new(),
                modified_at_ms: None,
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
                match remove_session_file_http_upload_target(&state.target, state.file_identity) {
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
            // 中文注释：recovery record 是“未完成 upload”的持久化事实；必须先删除它，
            // 再把内存状态切到 Complete，避免崩溃恢复误删已经完整写入的目标文件。
            StateStore::remove_http_upload(&self.config.state_path, &payload.upload_id)
                .map_err(|_| ProtocolError::StateFailed)?;
            let Some(state) = self.session_file_http_uploads.get_mut(&payload.upload_id) else {
                return Err(ProtocolError::InvalidEnvelope);
            };
            state.status = SessionFileHttpUploadStatus::Complete;
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
        // 中文注释：HTTP upload 直接写目标文件；abort 只能删除本 upload_id 创建的
        // 未完成目标。完成后的 tombstone 不能删除用户已经上传成功的文件。
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
        state.inflight_ranges.clear();
        let cleanup_outcome = remove_session_file_http_upload_target(&target, state.file_identity)?;
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

    fn write_session_file_upload_stream_chunk(
        &mut self,
        stream: &mut PacketFileUploadStream,
        payload: SessionFileTransferChunkPayload,
    ) -> Result<(SessionFileUploadProgressPayload, bool), ProtocolError> {
        if payload.session_id != stream.session_id
            || payload.offset_bytes != stream.offset_bytes
            || payload.size_bytes != stream.size_bytes
        {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let bytes = general_purpose::STANDARD
            .decode(payload.data_base64)
            .map_err(|_| ProtocolError::InvalidEnvelope)?;
        if bytes.len() > SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES as usize {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let next_offset = stream.offset_bytes.saturating_add(bytes.len() as u64);
        if next_offset > stream.size_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let mut file = OpenOptions::new()
            .append(true)
            .open(&stream.temp_path)
            .map_err(map_file_path_error)?;
        file.write_all(&bytes).map_err(map_file_path_error)?;
        stream.offset_bytes = next_offset;
        let complete = payload.eof || stream.offset_bytes == stream.size_bytes;
        if complete && stream.offset_bytes != stream.size_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }

        let mut modified_at_ms = None;
        if complete {
            self.ensure_not_active_session_file_http_upload_target(
                stream.session_id,
                &stream.path,
            )?;
            fs::rename(&stream.temp_path, &stream.path).map_err(map_file_path_error)?;
            let metadata = fs::metadata(&stream.path).map_err(map_file_path_error)?;
            modified_at_ms = metadata_modified_at_ms(&metadata);
        }

        Ok((
            SessionFileUploadProgressPayload {
                session_id: stream.session_id,
                path: absolute_path_string(&stream.path),
                offset_bytes: stream.offset_bytes,
                size_bytes: stream.size_bytes,
                eof: complete,
                modified_at_ms,
            },
            complete,
        ))
    }

    fn prepare_session_file_download_stream(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDownloadStreamPayload,
    ) -> Result<
        (
            SessionFileDownloadStreamReadyPayload,
            PacketFileDownloadStream,
        ),
        ProtocolError,
    > {
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        let target = resolve_existing_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
        let file = fs::File::open(&target).map_err(map_file_path_error)?;
        let metadata = file.metadata().map_err(map_file_path_error)?;
        if metadata.is_dir() {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let modified_at_ms = metadata_modified_at_ms(&metadata);
        let name = session_file_download_name(&target);
        let path = absolute_path_string(&target);

        Ok((
            SessionFileDownloadStreamReadyPayload {
                session_id: payload.session_id,
                path,
                name: name.clone(),
                size_bytes: metadata.len(),
                modified_at_ms,
            },
            PacketFileDownloadStream {
                session_id: payload.session_id,
                file,
                size_bytes: metadata.len(),
                offset_bytes: 0,
                next_output_seq: 1,
            },
        ))
    }

    fn read_session_file_download_stream_chunk(
        &mut self,
        stream: &mut PacketFileDownloadStream,
        max_bytes: u32,
    ) -> Result<(SessionFileTransferChunkPayload, bool), ProtocolError> {
        if max_bytes == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        if stream.offset_bytes > stream.size_bytes {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let remaining = stream.size_bytes.saturating_sub(stream.offset_bytes);
        if remaining == 0 {
            return Ok((
                SessionFileTransferChunkPayload {
                    session_id: stream.session_id,
                    offset_bytes: stream.offset_bytes,
                    data_base64: String::new(),
                    size_bytes: stream.size_bytes,
                    eof: true,
                },
                true,
            ));
        }
        let read_len = remaining.min(u64::from(
            max_bytes.min(SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES),
        )) as usize;
        let mut bytes = vec![0_u8; read_len];
        read_session_file_exact_at(&stream.file, &mut bytes, stream.offset_bytes)?;
        let offset = stream.offset_bytes;
        stream.offset_bytes = stream.offset_bytes.saturating_add(read_len as u64);
        let eof = stream.offset_bytes >= stream.size_bytes;

        Ok((
            SessionFileTransferChunkPayload {
                session_id: stream.session_id,
                offset_bytes: offset,
                data_base64: general_purpose::STANDARD.encode(bytes),
                size_bytes: stream.size_bytes,
                eof,
            },
            eof,
        ))
    }

    fn prepare_session_file_download(
        &mut self,
        connection: &ProtocolConnection,
        payload: SessionFileDownloadPreparePayload,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
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
        let attached = self.require_attached_session_root(connection, payload.session_id)?;
        if payload.max_bytes == 0 {
            return Err(ProtocolError::InvalidEnvelope);
        }
        let target = resolve_existing_session_file_target(&attached.root, &payload.path)?;
        self.ensure_not_active_session_file_http_upload_target(payload.session_id, &target)?;
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
        if !connection.track_daemon_client_history {
            return;
        }
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

    #[cfg(test)]
    fn session_output_history_mut(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> &mut SessionOutputHistory {
        self.session_output_history
            .entry(session_id)
            .or_insert_with(|| SessionOutputHistory::new(size))
    }

    #[cfg(test)]
    fn output_history_base_offset(&mut self, session_id: SessionId, size: TerminalSize) -> u64 {
        self.session_output_history_mut(session_id, size)
            .base_offset()
    }

    #[cfg(test)]
    fn output_history_attach_snapshot(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> (u64, Vec<u8>) {
        let history = self.session_output_history_mut(session_id, size);
        history.resize(size);
        (history.end_offset(), history.snapshot_bytes())
    }

    #[cfg(test)]
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
        self.maybe_notify_terminal_cwd_probe(session_id);
        Ok(true)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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

    fn current_session_terminal_cwd_after_refresh(
        &self,
        session_id: SessionId,
        refreshed_cwd: Option<String>,
    ) -> Option<String> {
        if let Some(cwd) = refreshed_cwd {
            return Some(cwd);
        }
        self.session_terminal_cwds
            .get(&session_id)
            .map(|cwd| absolute_path_string(cwd))
    }

    fn session_cwd_value(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<String>, ProtocolError> {
        if !self.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        let refreshed_cwd = self.refresh_session_terminal_cwd(session_id)?;
        // 中文注释：`session.cwd` 是 watcher 事件，不是“仅当本次 RPC 首次发现变化时才返回”。
        // 如果别的 RPC（如 `session.files`）已经先刷新了 cwd cache，这里仍要返回当前
        // 已知 terminal cwd，避免 watcher 事件被缓存更新顺序吞掉。
        Ok(self.current_session_terminal_cwd_after_refresh(session_id, refreshed_cwd))
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
            self.session_terminal_cwds.remove(&session_id);
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
        Ok(Some(cwd_string))
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

    #[cfg(test)]
    fn session_files_result(
        &mut self,
        session_id: SessionId,
        requested_path: Option<String>,
        fallback_to_root: bool,
    ) -> Result<SessionFilesResultPayload, ProtocolError> {
        let refreshed_cwd = self.refresh_session_terminal_cwd(session_id)?;
        self.session_files_result_after_refresh(
            session_id,
            requested_path,
            fallback_to_root,
            refreshed_cwd,
        )
    }

    fn session_files_result_after_refresh(
        &mut self,
        session_id: SessionId,
        requested_path: Option<String>,
        fallback_to_root: bool,
        refreshed_cwd: Option<String>,
    ) -> Result<SessionFilesResultPayload, ProtocolError> {
        // 中文注释：文件列表是 active upload 目标可见性的入口；进入列表前先清理
        // 已超时的 upload 状态，避免断开的旧上传把预分配目标永久隐藏。
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
            // 中文注释：HTTP upload 的 init 会直接创建最终目标并 set_len；在 commit 前
            // 这个文件还不是用户可用文件，文件列表必须隐藏它，避免前端提前判定上传完成。
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
            .map(|state| ActiveSessionFileHttpUploadTarget {
                target: state.target.clone(),
                file_identity: state.file_identity,
            })
            .collect()
    }

    fn ensure_not_active_session_file_http_upload_target(
        &mut self,
        session_id: SessionId,
        target: &Path,
    ) -> Result<(), ProtocolError> {
        // 中文注释：Git 面板也能操作文件。HTTP upload commit 前的预分配目标不能被
        // git add/clean/restore/diff 读取或删除，否则 stream 侧会在后续分片提交时失去目标。
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
            )
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
        let repo_root = current_git_repository_root(&cwd).ok_or(ProtocolError::InvalidEnvelope)?;
        let current_root = current_git_repository_root(&cwd).unwrap_or_else(|| repo_root.clone());
        let requested = Path::new(requested_worktree)
            .canonicalize()
            .map_err(map_file_path_error)?;

        read_git_worktrees(&repo_root, &current_root)
            .into_iter()
            .map(|worktree| worktree.path)
            .find(|worktree| same_path(worktree, &requested))
            .ok_or(ProtocolError::InvalidEnvelope)
    }

    #[cfg(test)]
    fn maybe_notify_terminal_cwd_probe(&mut self, session_id: SessionId) {
        let cwd_receivers = self
            .session_cwd_signals
            .get(&session_id)
            .map(watch::Sender::receiver_count)
            .unwrap_or(0);
        if cwd_receivers == 0 {
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

        // 中文注释：daemon 侧仍保留低频输出探测，但这里现在只负责唤醒 cwd watcher。
        // 共享文件树是 termd 的重资源读取面，client 收到 cwd 轻事件后必须显式再拉
        // `session.files`，这样 direct websocket 和 relay 的语义才能完全一致。
        self.session_terminal_cwd_probe_notified_at_ms
            .insert(session_id, now_ms);
        self.notify_session_cwd_changed(session_id);
    }

    #[cfg(test)]
    fn notify_session_cwd_changed(&self, session_id: SessionId) {
        let Some(signal) = self.session_cwd_signals.get(&session_id) else {
            return;
        };
        let next_version = signal.borrow().saturating_add(1);
        // cwd 变化只是轻量提示，没有 watcher 时也可以直接忽略。
        let _ = signal.send(next_version);
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
        connection.output_offsets.clear();
        connection.pending_attach_frames.clear();
        connection.pending_outputs.clear();
        connection.watched_sessions.clear();
        self.release_watched_attachments(connection.take_all_watched_attachments());
        connection.stale_watched_sessions.clear();
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

    fn watched_attachment_output_signal(
        &self,
        session_id: SessionId,
        attachment_id: &str,
    ) -> Result<Option<watch::Receiver<u64>>, ProtocolError> {
        let internal_session_id = self
            .session_index
            .get(&session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        self.runtime
            .watched_attachment_output_signal(internal_session_id, attachment_id)
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

    fn cwd_signal(
        &self,
        session_id: SessionId,
    ) -> Result<Option<watch::Receiver<u64>>, ProtocolError> {
        if !self.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }
        Ok(self
            .session_cwd_signals
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

/// 单条 WebSocket 连接的状态。E2EE session 只属于当前连接。
pub struct ProtocolConnection {
    client_id: ClientId,
    peer_ip: Option<String>,
    state: ProtocolConnectionState,
    // 中文注释：只有真实 WebSocket daemon/client 连接才写入 daemon_clients / client_history。
    // HTTP 临时文件连接只用于短生命周期文件 RPC，不应影响活跃连接计数。
    track_daemon_client_history: bool,
    device_id: Option<DeviceId>,
    authenticated_device_id: Option<DeviceId>,
    e2ee: Option<E2eeSession>,
    daemon_e2ee_exchange: Option<E2eeKeyExchangePayload>,
    device_e2ee_exchange: Option<E2eeKeyExchangePayload>,
    e2ee_auth_transcript: Option<E2eeAuthTranscript>,
    packet_mode: bool,
    binary_mode: bool,
    packet_terminal_streams: HashMap<PacketStreamId, PacketTerminalStream>,
    packet_terminal_streams_by_session: HashMap<SessionId, PacketStreamId>,
    packet_file_upload_streams: HashMap<PacketStreamId, PacketFileUploadStream>,
    packet_file_download_streams: HashMap<PacketStreamId, PacketFileDownloadStream>,
    attached_sessions: Vec<SessionId>,
    // `attached_sessions` 表示权限范围；`watched_sessions` 才表示该连接要接收实时输出。
    // 文件/Git/search 等短连接会只 attach 权限，避免大流量终端输出堵住 RPC 响应。
    watched_sessions: HashSet<SessionId>,
    // 中文注释：每条连接单独记住自己已经消费到的 cwd watcher version，避免 daemon
    // 因低频 probe 重复返回“当前 cwd”时，把同一个 version 多次升级成 changed 事件。
    watched_cwd_versions: HashMap<SessionId, u64>,
    // 中文注释：version 只能说明 watcher 收到一次“请检查 cwd”的提示，不能说明 cwd
    // 本身一定变化。这里按连接缓存最后一次已下发的 cwd 值，避免 probe-only 场景把
    // 同一 cwd 每秒再推一遍，导致前端 follow 反复重拉 session.files。
    watched_cwd_paths: HashMap<SessionId, Option<String>>,
    // 中文注释：watched terminal 是连接级资源，不是设备级角色。这里保存 runtime
    // attachment id，连接断开或 terminal stream 取消时只释放自己的 supervisor attach。
    watched_attachment_ids: HashMap<SessionId, String>,
    next_watched_attachment_number: u64,
    // 中文注释：快速切换 terminal stream 后，旧 watcher 的通知可能已经在队列里。
    // 这类 session 曾经被当前连接 watch 过，但已经主动取消订阅；迟到输出应当是 no-op，
    // 而从未订阅过的 session 仍必须返回 invalid_state。
    stale_watched_sessions: HashSet<SessionId>,
    pending_attach_frames: HashMap<SessionId, VecDeque<Vec<u8>>>,
    output_offsets: HashMap<SessionId, u64>,
    pending_outputs: HashMap<SessionId, VecDeque<Vec<u8>>>,
    deferred_output_wakeups: HashSet<SessionId>,
    debug_traffic: ProtocolConnectionDebugTraffic,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolConnectionDebugSnapshot {
    pub packet_mode: bool,
    pub binary_mode: bool,
    pub attached_sessions: usize,
    pub watched_sessions: usize,
    pub terminal_streams: usize,
    /// 兼容旧日志字段。terminal 输出不再按 browser render credit 限流，因此这里恒为 0。
    pub zero_credit_terminal_streams: usize,
    /// 兼容旧日志字段。terminal 输出不再维护应用层 output credit，因此这里恒为 0。
    pub total_output_credit: u64,
    pub pending_raw_chunks: usize,
    pub pending_terminal_frames: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProtocolConnectionDebugTraffic {
    pub inbound_legacy_messages: BTreeMap<String, u64>,
    pub inbound_requests: BTreeMap<String, u64>,
    pub inbound_stream_opens: BTreeMap<String, u64>,
    pub inbound_flow_packets: u64,
    pub inbound_flow_credit: u64,
    pub inbound_stream_chunks: u64,
    pub inbound_stream_chunk_payload_bytes: u64,
    pub inbound_stream_ends: u64,
    pub inbound_cancels: u64,
    pub outbound_legacy_messages: BTreeMap<String, u64>,
    pub outbound_responses: BTreeMap<String, u64>,
    pub outbound_events: BTreeMap<String, u64>,
    pub outbound_errors: u64,
    pub outbound_stream_chunks: u64,
    pub outbound_session_data_chunks: u64,
    pub outbound_session_data_bytes: u64,
    pub outbound_terminal_frame_chunks: u64,
    pub outbound_terminal_frame_count: u64,
    pub outbound_terminal_frame_bytes: u64,
    pub outbound_terminal_frame_transport_bytes: u64,
}

impl ProtocolConnectionDebugTraffic {
    fn increment_method(map: &mut BTreeMap<String, u64>, method: Option<&str>) {
        let method = method.unwrap_or("<none>");
        let counter = map.entry(method.to_owned()).or_default();
        *counter = counter.saturating_add(1);
    }

    fn record_inbound_legacy_envelope(&mut self, kind: MessageType) {
        // legacy envelope 没有 packet method/id，只记录消息类型，避免 daemon 日志出现明文 payload。
        Self::increment_method(
            &mut self.inbound_legacy_messages,
            Some(&format!("{kind:?}")),
        );
    }

    fn record_inbound_packet(&mut self, packet: &ProtocolPacket<Value>) {
        match packet.kind {
            PacketKind::Request => {
                Self::increment_method(&mut self.inbound_requests, packet.method.as_deref());
            }
            PacketKind::StreamOpen => {
                Self::increment_method(&mut self.inbound_stream_opens, packet.method.as_deref());
            }
            PacketKind::StreamChunk => {
                self.inbound_stream_chunks = self.inbound_stream_chunks.saturating_add(1);
            }
            PacketKind::StreamEnd => {
                self.inbound_stream_ends = self.inbound_stream_ends.saturating_add(1);
            }
            PacketKind::Cancel => {
                self.inbound_cancels = self.inbound_cancels.saturating_add(1);
            }
            PacketKind::Flow => {
                self.inbound_flow_packets = self.inbound_flow_packets.saturating_add(1);
                self.inbound_flow_credit = self
                    .inbound_flow_credit
                    .saturating_add(u64::from(packet.credit.unwrap_or(0)));
            }
            PacketKind::Response | PacketKind::Event | PacketKind::Error => {}
        }
    }

    fn record_inbound_stream_chunk_payload(&mut self, data_base64: &str) {
        self.inbound_stream_chunk_payload_bytes = self
            .inbound_stream_chunk_payload_bytes
            .saturating_add(base64_payload_decoded_len(data_base64) as u64);
    }

    fn record_outbound_legacy_envelope(&mut self, kind: MessageType) {
        // 只统计内层消息类型，足以定位是否是旧协议 activity/status/files 等路径在刷包。
        Self::increment_method(
            &mut self.outbound_legacy_messages,
            Some(&format!("{kind:?}")),
        );
    }

    fn record_outbound_packet(&mut self, packet: &ProtocolPacket<Value>) {
        match packet.kind {
            PacketKind::Response => {
                Self::increment_method(&mut self.outbound_responses, packet.method.as_deref());
            }
            PacketKind::Event => {
                Self::increment_method(&mut self.outbound_events, packet.method.as_deref());
            }
            PacketKind::Error => {
                self.outbound_errors = self.outbound_errors.saturating_add(1);
            }
            PacketKind::StreamChunk => {
                self.outbound_stream_chunks = self.outbound_stream_chunks.saturating_add(1);
            }
            PacketKind::Request
            | PacketKind::StreamOpen
            | PacketKind::StreamEnd
            | PacketKind::Cancel
            | PacketKind::Flow => {}
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn record_outbound_session_data(&mut self, data_base64: &str) {
        self.outbound_session_data_chunks = self.outbound_session_data_chunks.saturating_add(1);
        self.outbound_session_data_bytes = self
            .outbound_session_data_bytes
            .saturating_add(base64_payload_decoded_len(data_base64) as u64);
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn record_outbound_terminal_frame(&mut self, frame: &TerminalFramePayload) {
        self.outbound_terminal_frame_chunks = self.outbound_terminal_frame_chunks.saturating_add(1);
        self.outbound_terminal_frame_count = self
            .outbound_terminal_frame_count
            .saturating_add(terminal_frame_payload_count(frame) as u64);
        self.outbound_terminal_frame_bytes = self
            .outbound_terminal_frame_bytes
            .saturating_add(terminal_frame_payload_bytes(frame) as u64);
        self.outbound_terminal_frame_transport_bytes = self
            .outbound_terminal_frame_transport_bytes
            .saturating_add(terminal_frame_transport_cost(frame) as u64);
    }

    pub fn merge(&mut self, other: Self) {
        merge_method_counts(
            &mut self.inbound_legacy_messages,
            other.inbound_legacy_messages,
        );
        merge_method_counts(&mut self.inbound_requests, other.inbound_requests);
        merge_method_counts(&mut self.inbound_stream_opens, other.inbound_stream_opens);
        self.inbound_flow_packets = self
            .inbound_flow_packets
            .saturating_add(other.inbound_flow_packets);
        self.inbound_flow_credit = self
            .inbound_flow_credit
            .saturating_add(other.inbound_flow_credit);
        self.inbound_stream_chunks = self
            .inbound_stream_chunks
            .saturating_add(other.inbound_stream_chunks);
        self.inbound_stream_chunk_payload_bytes = self
            .inbound_stream_chunk_payload_bytes
            .saturating_add(other.inbound_stream_chunk_payload_bytes);
        self.inbound_stream_ends = self
            .inbound_stream_ends
            .saturating_add(other.inbound_stream_ends);
        self.inbound_cancels = self.inbound_cancels.saturating_add(other.inbound_cancels);
        merge_method_counts(
            &mut self.outbound_legacy_messages,
            other.outbound_legacy_messages,
        );
        merge_method_counts(&mut self.outbound_responses, other.outbound_responses);
        merge_method_counts(&mut self.outbound_events, other.outbound_events);
        self.outbound_errors = self.outbound_errors.saturating_add(other.outbound_errors);
        self.outbound_stream_chunks = self
            .outbound_stream_chunks
            .saturating_add(other.outbound_stream_chunks);
        self.outbound_session_data_chunks = self
            .outbound_session_data_chunks
            .saturating_add(other.outbound_session_data_chunks);
        self.outbound_session_data_bytes = self
            .outbound_session_data_bytes
            .saturating_add(other.outbound_session_data_bytes);
        self.outbound_terminal_frame_chunks = self
            .outbound_terminal_frame_chunks
            .saturating_add(other.outbound_terminal_frame_chunks);
        self.outbound_terminal_frame_count = self
            .outbound_terminal_frame_count
            .saturating_add(other.outbound_terminal_frame_count);
        self.outbound_terminal_frame_bytes = self
            .outbound_terminal_frame_bytes
            .saturating_add(other.outbound_terminal_frame_bytes);
        self.outbound_terminal_frame_transport_bytes = self
            .outbound_terminal_frame_transport_bytes
            .saturating_add(other.outbound_terminal_frame_transport_bytes);
    }

    pub fn packet_count(&self) -> u64 {
        self.inbound_legacy_messages.values().sum::<u64>()
            + self.inbound_requests.values().sum::<u64>()
            + self.inbound_stream_opens.values().sum::<u64>()
            + self.inbound_flow_packets
            + self.inbound_stream_chunks
            + self.inbound_stream_ends
            + self.inbound_cancels
            + self.outbound_legacy_messages.values().sum::<u64>()
            + self.outbound_responses.values().sum::<u64>()
            + self.outbound_events.values().sum::<u64>()
            + self.outbound_errors
            + self.outbound_stream_chunks
    }

    pub fn method_count_exceeds(&self, threshold: u64) -> bool {
        [
            &self.inbound_legacy_messages,
            &self.inbound_requests,
            &self.inbound_stream_opens,
            &self.outbound_legacy_messages,
            &self.outbound_responses,
            &self.outbound_events,
        ]
        .into_iter()
        .any(|counts| counts.values().any(|count| *count > threshold))
    }
}

fn merge_method_counts(target: &mut BTreeMap<String, u64>, source: BTreeMap<String, u64>) {
    for (method, count) in source {
        let target_count = target.entry(method).or_default();
        *target_count = target_count.saturating_add(count);
    }
}

impl ProtocolConnection {
    fn new(peer_ip: Option<String>) -> Self {
        Self {
            client_id: ClientId::new(),
            peer_ip,
            state: ProtocolConnectionState::Init,
            track_daemon_client_history: true,
            device_id: None,
            authenticated_device_id: None,
            e2ee: None,
            daemon_e2ee_exchange: None,
            device_e2ee_exchange: None,
            e2ee_auth_transcript: None,
            packet_mode: false,
            binary_mode: false,
            packet_terminal_streams: HashMap::new(),
            packet_terminal_streams_by_session: HashMap::new(),
            packet_file_upload_streams: HashMap::new(),
            packet_file_download_streams: HashMap::new(),
            attached_sessions: Vec::new(),
            watched_sessions: HashSet::new(),
            watched_cwd_versions: HashMap::new(),
            watched_cwd_paths: HashMap::new(),
            watched_attachment_ids: HashMap::new(),
            next_watched_attachment_number: 1,
            stale_watched_sessions: HashSet::new(),
            pending_attach_frames: HashMap::new(),
            output_offsets: HashMap::new(),
            pending_outputs: HashMap::new(),
            deferred_output_wakeups: HashSet::new(),
            debug_traffic: ProtocolConnectionDebugTraffic::default(),
        }
    }

    pub fn authenticated_http(device_id: DeviceId) -> Self {
        let mut connection = Self::new(None);
        connection.device_id = Some(device_id);
        connection.authenticated_device_id = Some(device_id);
        connection.state = ProtocolConnectionState::Authenticated;
        connection.track_daemon_client_history = false;
        connection
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

    pub fn debug_snapshot(&self) -> ProtocolConnectionDebugSnapshot {
        // 该快照只暴露队列计数，不包含 PTY 明文或设备密钥，可安全写入 daemon 日志。
        let pending_raw_chunks = self.pending_outputs.values().map(VecDeque::len).sum();
        ProtocolConnectionDebugSnapshot {
            packet_mode: self.packet_mode,
            binary_mode: self.binary_mode,
            attached_sessions: self.attached_sessions.len(),
            watched_sessions: self.watched_sessions.len(),
            terminal_streams: self.packet_terminal_streams.len(),
            zero_credit_terminal_streams: 0,
            total_output_credit: 0,
            pending_raw_chunks,
            pending_terminal_frames: 0,
        }
    }

    pub fn take_debug_traffic(&mut self) -> ProtocolConnectionDebugTraffic {
        std::mem::take(&mut self.debug_traffic)
    }

    /// 取走需要在释放全局 daemon 锁后继续 flush 的 session。
    ///
    /// 中文注释：terminal 输出不再等待浏览器 render ACK / credit。这里的 wakeup 只用于
    /// batch/transport 上限截断后继续排下一轮 push，避免单轮输出占住 daemon 主循环。
    pub fn take_deferred_output_wakeups(&mut self) -> Vec<SessionId> {
        self.deferred_output_wakeups.drain().collect()
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

    /// 处理真实 WebSocket wire frame。binary 模式只影响 E2EE 后的 packet，明文握手仍是 JSON。
    pub fn handle_wire_message<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        message: ProtocolWireMessage,
    ) -> Vec<ProtocolWireMessage>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_handle_wire_message(protocol, message) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response_wire(error)],
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

    pub fn read_session_output_wire<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        max_bytes: usize,
    ) -> Vec<ProtocolWireMessage>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_output_wire(protocol, session_id, max_bytes) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response_wire(error)],
        }
    }

    /// 只从 daemon runtime 收集待发送输出，不做 E2EE/二进制编码。
    ///
    /// server/relay 层会先短暂持有全局 daemon protocol 锁调用这里，然后释放锁再调用
    /// `encrypt_collected_inner_messages_wire`。这样大 snapshot 或大 terminal batch 的加密与
    /// protobuf/JSON 编码不会阻塞其它 direct/relay 连接的控制请求。
    pub fn drain_session_output_messages_for_push<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        max_bytes: usize,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.try_drain_session_output_messages_for_push(protocol, session_id, max_bytes)
    }

    /// 对已经离开 daemon 全局锁的内层消息做 E2EE 封包。
    pub fn encrypt_collected_inner_messages_wire(
        &mut self,
        messages: Result<Vec<JsonEnvelope>, ProtocolError>,
    ) -> Vec<ProtocolWireMessage> {
        match messages {
            Ok(messages) => match self.encrypt_inner_messages_wire(messages) {
                Ok(messages) => messages,
                Err(error) => vec![self.error_response_wire(error)],
            },
            Err(error) => vec![self.error_response_wire(error)],
        }
    }

    /// 读取并加密当前 session 的 cwd 变化事件，用于文件树跟随模式的轻量通知。
    pub fn read_session_cwd_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<JsonEnvelope>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_read_session_cwd_update(protocol, session_id) {
            Ok(messages) => messages,
            Err(error) => vec![self.error_response(error)],
        }
    }

    pub fn read_session_cwd_update_wire<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<ProtocolWireMessage>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_collect_session_cwd_update_messages(protocol, session_id) {
            Ok(messages) => match self.encrypt_inner_messages_wire(messages) {
                Ok(messages) => messages,
                Err(error) => vec![self.error_response_wire(error)],
            },
            Err(error) => vec![self.error_response_wire(error)],
        }
    }

    /// 只读取当前 session 的 cwd 变化事件，不做 E2EE 封包。
    pub fn read_session_cwd_update_messages<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.try_collect_session_cwd_update_messages(protocol, session_id)
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

    pub fn read_session_resize_update_wire<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<ProtocolWireMessage>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_collect_session_resize_update_messages(protocol, session_id) {
            Ok(messages) => match self.encrypt_inner_messages_wire(messages) {
                Ok(messages) => messages,
                Err(error) => vec![self.error_response_wire(error)],
            },
            Err(error) => vec![self.error_response_wire(error)],
        }
    }

    /// 只读取当前 session resize 状态，不做 E2EE 封包；用途同文件树推送收集方法。
    pub fn read_session_resize_update_messages<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.try_collect_session_resize_update_messages(protocol, session_id)
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
        let session_ids: Vec<_> = self
            .attached_sessions
            .iter()
            .copied()
            .filter(|session_id| self.watched_sessions.contains(session_id))
            .collect();
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
            .filter(|session_id| self.watched_sessions.contains(*session_id))
            .filter_map(|session_id| {
                let attachment_id = self.watched_attachment_ids.get(session_id)?;
                protocol
                    .watched_attachment_output_signal(*session_id, attachment_id)
                    .ok()
                    .flatten()
                    .map(|signal| (*session_id, signal))
            })
            .collect()
    }

    /// 返回当前连接可订阅的 session 活动信号。
    ///
    /// activity 只告诉前端“这个 session 有新输出”，不读取 PTY 内容；这样后台 session
    /// 可以在列表里变色，同时避免为了提示而把大块终端输出推给非当前终端视图。
    pub fn session_activity_signals<B, V>(
        &self,
        protocol: &DaemonProtocol<B, V>,
    ) -> Vec<(SessionId, watch::Receiver<u64>)>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        if self.state != ProtocolConnectionState::Attached
            || !self.is_authenticated()
            || self.watched_sessions.is_empty()
        {
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

    /// 返回当前连接已 attach session 的 cwd 变化信号，供 WebSocket/relay 推送轻量 cwd 事件。
    pub fn attached_cwd_signals<B, V>(
        &self,
        protocol: &DaemonProtocol<B, V>,
    ) -> Vec<(SessionId, watch::Receiver<u64>)>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.attached_sessions
            .iter()
            .filter(|session_id| self.watched_sessions.contains(*session_id))
            .filter_map(|session_id| {
                protocol
                    .cwd_signal(*session_id)
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
            .filter(|session_id| self.watched_sessions.contains(*session_id))
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
        for (_, stream) in self.packet_file_upload_streams.drain() {
            cleanup_upload_temp(&stream);
        }
        self.packet_file_download_streams.clear();
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
                if inner.kind == MessageType::Packet {
                    let packet: ProtocolPacket<Value> = decode_payload(inner.payload)?;
                    let packet_responses = self.handle_inner_packet(protocol, packet)?;
                    return self.encrypt_packets(packet_responses);
                }
                if self.packet_mode {
                    return Err(ProtocolError::InvalidState);
                }
                self.debug_traffic
                    .record_inbound_legacy_envelope(inner.kind);
                let inner_responses = self.handle_inner_envelope(protocol, inner)?;
                self.encrypt_inner_messages(inner_responses)
            }
            _ => Err(ProtocolError::InvalidState),
        }
    }

    fn try_handle_wire_message<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        message: ProtocolWireMessage,
    ) -> Result<Vec<ProtocolWireMessage>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match message {
            ProtocolWireMessage::Json(envelope) => match envelope.kind {
                MessageType::E2eeKeyExchange => {
                    let payload = decode_payload(envelope.payload)?;
                    protocol
                        .accept_e2ee_key_exchange(self, payload)
                        .map(|messages| {
                            messages
                                .into_iter()
                                .map(ProtocolWireMessage::Json)
                                .collect()
                        })
                }
                MessageType::EncryptedFrame => {
                    let frame = decode_payload(envelope.payload)?;
                    let inner: JsonEnvelope = self.e2ee_mut()?.decrypt_json_payload(&frame)?;
                    if inner.kind == MessageType::Packet {
                        let packet: ProtocolPacket<Value> = decode_payload(inner.payload)?;
                        let packet_responses = self.handle_inner_packet(protocol, packet)?;
                        return self.encrypt_packets_wire(packet_responses);
                    }
                    if self.packet_mode {
                        return Err(ProtocolError::InvalidState);
                    }
                    self.debug_traffic
                        .record_inbound_legacy_envelope(inner.kind);
                    let inner_responses = self.handle_inner_envelope(protocol, inner)?;
                    self.encrypt_inner_messages_wire(inner_responses)
                }
                _ => Err(ProtocolError::InvalidState),
            },
            ProtocolWireMessage::Binary(raw) => {
                if !self.binary_mode {
                    return Err(ProtocolError::InvalidState);
                }
                let frame = decode_binary_encrypted_frame(&raw)?;
                let plaintext = self.e2ee_mut()?.decrypt_binary_payload(&frame)?;
                let binary_packet = decode_binary_protocol_packet(&plaintext)
                    .map_err(|_| ProtocolError::InvalidEnvelope)?;
                let packet = protocol_packet_from_binary(binary_packet)
                    .map_err(|_| ProtocolError::InvalidEnvelope)?;
                let packet_responses = self.handle_inner_packet(protocol, packet)?;
                self.encrypt_packets_wire(packet_responses)
            }
        }
    }

    fn try_drain_session_output_messages_for_push<B, V>(
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

        if !self.watched_sessions.contains(&session_id) {
            if self.stale_watched_sessions.contains(&session_id) {
                return Ok(Vec::new());
            }
            return Err(ProtocolError::InvalidState);
        }

        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        if self.packet_mode {
            let Some(attachment_id) = self.watched_attachment_ids.get(&session_id).cloned() else {
                return Ok(Vec::new());
            };
            let mut frames = Vec::new();
            let mut frame_bytes = 0_usize;
            let mut drained_chunks = 0_usize;

            while drained_chunks < LIVE_OUTPUT_DRAIN_MAX_CHUNKS {
                let next_frame =
                    if let Some(pending) = self.pending_attach_frames.get_mut(&session_id) {
                        pending.pop_front()
                    } else {
                        None
                    };
                let Some(frame) = (match next_frame {
                    Some(frame) => Some(frame),
                    None => protocol.read_watched_attachment_frame(session_id, &attachment_id)?,
                }) else {
                    break;
                };
                let cost = frame.len().max(TERMINAL_STREAM_METADATA_CREDIT_BYTES);
                if !frames.is_empty()
                    && frame_bytes.saturating_add(cost) > TERMINAL_STREAM_BATCH_MAX_BYTES
                {
                    self.pending_attach_frames
                        .entry(session_id)
                        .or_default()
                        .push_front(frame);
                    break;
                }
                frame_bytes = frame_bytes.saturating_add(cost);
                drained_chunks += 1;
                frames.push(envelope_value(
                    MessageType::AttachFrame,
                    AttachFramePayload {
                        session_id,
                        data_base64: general_purpose::STANDARD.encode(frame),
                    },
                )?);
            }

            if self
                .pending_attach_frames
                .get(&session_id)
                .is_some_and(|pending| !pending.is_empty())
            {
                self.deferred_output_wakeups.insert(session_id);
            } else {
                self.deferred_output_wakeups.remove(&session_id);
            }

            return Ok(frames);
        }
        let max_packet_chunks = if self.packet_mode {
            RAW_OUTPUT_BATCH_MAX_CHUNKS
        } else {
            RAW_OUTPUT_BATCH_MAX_CHUNKS
        };

        let mut chunks = Vec::new();
        if let Some(pending) = self.pending_outputs.get_mut(&session_id) {
            while chunks.len() < max_packet_chunks {
                let Some(chunk) = pending.pop_front() else {
                    break;
                };
                if chunk.len() > max_bytes {
                    // 中文注释：attach snapshot 会作为 pending raw chunk 入队，可能远大于
                    // 单次输出预算。这里按 max_bytes 切开，避免 writer 一次发送超大 E2EE 帧，
                    // 让控制帧、输入和新 attach 能在 snapshot 回放期间插队。
                    let remainder = chunk[max_bytes..].to_vec();
                    chunks.push(chunk[..max_bytes].to_vec());
                    pending.push_front(remainder);
                } else {
                    chunks.push(chunk);
                }
            }
        }

        let mut drained_chunks = 0;
        loop {
            if chunks.len() < max_packet_chunks {
                self.collect_retained_output_chunks(
                    protocol,
                    session_id,
                    &internal_session_id,
                    max_bytes,
                    max_packet_chunks,
                    &mut chunks,
                );
            }
            if drained_chunks >= LIVE_OUTPUT_DRAIN_MAX_CHUNKS {
                break;
            }
            if chunks.len() >= max_packet_chunks {
                break;
            }

            // watch::Receiver 会合并多次 PTY 输出信号；如果一次事件只读一个 chunk，
            // 剩余积压可能要等下一次用户输入才被推送。这里在单次唤醒内继续读空
            // 当前已经就绪的非阻塞 PTY 缓存，同时每轮先 collect 再继续 drain，避免
            // raw history 的保留窗口裁掉已连接客户端还没有收到的片段。
            #[cfg(test)]
            if !protocol.drain_runtime_output_to_history(
                session_id,
                &internal_session_id,
                max_bytes,
            )? {
                break;
            }
            #[cfg(not(test))]
            {
                let mut buffer = vec![0_u8; max_bytes];
                let read = protocol
                    .runtime
                    .read_output(&internal_session_id, &mut buffer)
                    .map_err(map_runtime_error)?;
                if read == 0 {
                    break;
                }
                buffer.truncate(read);
                chunks.push(buffer);
            }
            drained_chunks += 1;
        }

        // 中文注释：如果本轮刚好打满 raw batch 上限，停止原因可能是 history/pending 还有
        // 已缓存内容，也可能是 runtime PTY 还有未 drain 的 backlog。后者不能靠查询 history
        // 发现，所以必须显式排下一轮 flush，让控制面先得到调度机会后再继续读取输出。
        let reached_raw_batch_limit = chunks.len() >= max_packet_chunks;

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
        if reached_raw_batch_limit
            || raw_output_has_more_pending(protocol, self, session_id, &internal_session_id)
        {
            // 中文注释：raw/legacy 输出单轮只发有限 chunk，避免一次历史 backlog 占住
            // daemon 主循环。仍有可发数据时显式排下一轮 wakeup，由 server/relay 释放锁后继续。
            self.deferred_output_wakeups.insert(session_id);
        }

        Ok(messages)
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
        let messages =
            self.try_drain_session_output_messages_for_push(protocol, session_id, max_bytes)?;
        self.encrypt_inner_messages(messages)
    }

    fn try_read_session_output_wire<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
        max_bytes: usize,
    ) -> Result<Vec<ProtocolWireMessage>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let messages =
            self.try_drain_session_output_messages_for_push(protocol, session_id, max_bytes)?;
        self.encrypt_inner_messages_wire(messages)
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

    pub fn read_session_activity_wire<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Vec<ProtocolWireMessage>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match self.try_collect_session_activity_messages(protocol, session_id) {
            Ok(messages) => match self.encrypt_inner_messages_wire(messages) {
                Ok(messages) => messages,
                Err(error) => vec![self.error_response_wire(error)],
            },
            Err(error) => vec![self.error_response_wire(error)],
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
        let messages = self.try_collect_session_activity_messages(protocol, session_id)?;
        self.encrypt_inner_messages(messages)
    }

    fn try_collect_session_activity_messages<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if self.watched_sessions.is_empty() {
            return Err(ProtocolError::InvalidState);
        }
        if !protocol.session_index.contains_key(&session_id) {
            return Err(ProtocolError::SessionNotFound);
        }

        Ok(vec![envelope_value(
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
        max_chunks: usize,
        chunks: &mut Vec<Vec<u8>>,
    ) where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        #[cfg(not(test))]
        {
            let _ = (
                protocol,
                session_id,
                internal_session_id,
                max_bytes,
                max_chunks,
                chunks,
            );
            return;
        }

        #[cfg(test)]
        loop {
            if chunks.len() >= max_chunks {
                break;
            }
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

    fn try_read_session_cwd_update<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let messages = self.try_collect_session_cwd_update_messages(protocol, session_id)?;
        self.encrypt_inner_messages(messages)
    }

    fn try_collect_session_cwd_update_messages<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if !self.watched_sessions.contains(&session_id) {
            if self.stale_watched_sessions.contains(&session_id) {
                return Ok(Vec::new());
            }
            return Err(ProtocolError::InvalidState);
        }

        let current_version = protocol
            .session_cwd_signals
            .get(&session_id)
            .map(|signal| *signal.borrow())
            .unwrap_or(0);
        let last_seen_version = self
            .watched_cwd_versions
            .get(&session_id)
            .copied()
            .unwrap_or(0);
        if current_version == 0 || current_version == last_seen_version {
            return Ok(Vec::new());
        }

        let Some(cwd) = protocol.session_cwd_value(session_id)? else {
            return Ok(Vec::new());
        };
        self.watched_cwd_versions
            .insert(session_id, current_version);
        let last_pushed_cwd = self
            .watched_cwd_paths
            .get(&session_id)
            .and_then(|cwd| cwd.clone());
        if last_pushed_cwd.as_deref() == Some(cwd.as_str()) {
            return Ok(Vec::new());
        }
        self.watched_cwd_paths.insert(session_id, Some(cwd.clone()));
        Ok(vec![envelope_value(
            MessageType::SessionCwdChanged,
            SessionCwdChangedPayload { session_id, cwd },
        )?])
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
        let messages = self.try_collect_session_resize_update_messages(protocol, session_id)?;
        self.encrypt_inner_messages(messages)
    }

    fn try_collect_session_resize_update_messages<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        session_id: SessionId,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.authenticated_device_id()?;
        if !self.watched_sessions.contains(&session_id) {
            if self.stale_watched_sessions.contains(&session_id) {
                return Ok(Vec::new());
            }
            return Err(ProtocolError::InvalidState);
        }
        let internal_session_id = protocol
            .session_index
            .get(&session_id)
            .ok_or(ProtocolError::SessionNotFound)?;
        let size = protocol.runtime_size_proto(internal_session_id)?;

        Ok(vec![envelope_value(
            MessageType::SessionResized,
            SessionResizedPayload {
                session_id,
                size,
                resize_owner: true,
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
            #[cfg(test)]
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
            MessageType::SessionSearch => {
                let payload = decode_payload(envelope.payload)?;
                protocol.search_session_output(self, payload)
            }
            MessageType::SessionFiles => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_session_files(self, payload)
            }
            MessageType::SessionGit => {
                let payload = decode_payload(envelope.payload)?;
                protocol.list_session_git(self, payload)
            }
            MessageType::SessionGitAction => {
                let payload = decode_payload(envelope.payload)?;
                protocol.apply_session_git_action(self, payload)
            }
            MessageType::SessionGitDiff => {
                let payload = decode_payload(envelope.payload)?;
                protocol.read_session_git_diff(self, payload)
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

    pub(crate) fn dispatch_http_control_packet<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        self.handle_inner_packet(protocol, packet)
    }

    fn handle_inner_packet<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        if packet.version != PROTOCOL_PACKET_VERSION {
            return Ok(packet_bound_error(packet, ProtocolError::InvalidEnvelope)
                .into_iter()
                .collect());
        }
        self.debug_traffic.record_inbound_packet(&packet);

        match packet.kind {
            PacketKind::Request => self.handle_packet_request(protocol, packet),
            PacketKind::StreamOpen => self.handle_packet_stream_open(protocol, packet),
            PacketKind::StreamChunk => self.handle_packet_stream_chunk(protocol, packet),
            PacketKind::StreamEnd => self.handle_packet_stream_end(protocol, packet),
            PacketKind::Cancel => self.handle_packet_cancel(protocol, packet),
            PacketKind::Flow => self.handle_packet_flow(protocol, packet),
            PacketKind::Response | PacketKind::Event | PacketKind::Error => {
                Err(ProtocolError::InvalidState)
            }
        }
    }

    fn handle_packet_request<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let id = packet.id.ok_or(ProtocolError::InvalidEnvelope)?;
        let method = packet
            .method
            .clone()
            .ok_or(ProtocolError::InvalidEnvelope)?;
        let responses = self.dispatch_packet_request(protocol, method.as_str(), packet.payload);

        match responses {
            Ok(envelopes) => packetize_handler_responses(id, method.as_str(), envelopes),
            Err(error) => Ok(vec![packet_request_error(id, error)?]),
        }
    }

    fn dispatch_packet_request<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        method: &str,
        payload: Value,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        match method {
            METHOD_PAIR_REQUEST => {
                let payload = decode_payload(payload)?;
                protocol.handle_pair_request(self, payload)
            }
            METHOD_AUTH | METHOD_AUTH_VERIFY => {
                let payload = decode_payload(payload)?;
                protocol.handle_auth(self, payload)
            }
            METHOD_AUTH_SESSION_TOKEN => protocol.issue_session_token_grant(self),
            METHOD_CLIENT_HELLO => {
                let payload = decode_payload(payload)?;
                protocol.record_client_hello(self, payload)
            }
            METHOD_SESSION_CREATE => {
                let payload = decode_payload(payload)?;
                protocol.create_session(self, payload)
            }
            METHOD_SESSION_ATTACH => {
                let payload = decode_payload(payload)?;
                protocol.attach_session_permission(self, payload)
            }
            METHOD_SESSION_CURSOR => {
                let payload = decode_payload(payload)?;
                protocol.record_session_cursor(self, payload)
            }
            METHOD_SESSION_RESIZE => {
                let payload = decode_payload(payload)?;
                protocol.resize_session(self, payload)
            }
            METHOD_SESSION_RENAME => {
                let payload = decode_payload(payload)?;
                protocol.rename_session(self, payload)
            }
            METHOD_SESSION_REORDER => {
                let payload = decode_payload(payload)?;
                protocol.reorder_sessions(self, payload)
            }
            METHOD_SESSION_CLOSE => {
                let payload = decode_payload(payload)?;
                protocol.close_session(self, payload)
            }
            METHOD_SESSION_SEARCH => {
                let payload = decode_payload(payload)?;
                protocol.search_session_output(self, payload)
            }
            METHOD_SESSION_FILES => {
                let payload = decode_payload(payload)?;
                protocol.list_session_files(self, payload)
            }
            METHOD_SESSION_GIT => {
                let payload = decode_payload(payload)?;
                protocol.list_session_git(self, payload)
            }
            METHOD_SESSION_GIT_ACTION => {
                let payload = decode_payload(payload)?;
                protocol.apply_session_git_action(self, payload)
            }
            METHOD_SESSION_GIT_DIFF => {
                let payload = decode_payload(payload)?;
                protocol.read_session_git_diff(self, payload)
            }
            METHOD_SESSION_FILE_READ => {
                let payload = decode_payload(payload)?;
                protocol.read_session_file(self, payload)
            }
            METHOD_SESSION_FILE_WRITE => {
                let payload = decode_payload(payload)?;
                protocol.write_session_file(self, payload)
            }
            METHOD_SESSION_FILE_DELETE => {
                let payload = decode_payload(payload)?;
                protocol.delete_session_file(self, payload)
            }
            METHOD_SESSION_FILE_DOWNLOAD_PREPARE => {
                let payload = decode_payload(payload)?;
                protocol.prepare_session_file_download(self, payload)
            }
            METHOD_SESSION_FILE_DOWNLOAD_CHUNK => {
                let payload = decode_payload(payload)?;
                protocol.read_session_file_download_chunk(self, payload)
            }
            METHOD_CONTROL_REQUEST => {
                let payload = decode_payload(payload)?;
                protocol.request_control(self, payload)
            }
            METHOD_SESSION_LIST => {
                let payload = decode_payload(payload)?;
                protocol.list_sessions(self, payload)
            }
            METHOD_DAEMON_CLIENTS => {
                let payload = decode_payload(payload)?;
                protocol.list_daemon_clients(self, payload)
            }
            METHOD_DAEMON_CLIENT_FORGET => {
                let payload = decode_payload(payload)?;
                protocol.forget_daemon_client(self, payload)
            }
            METHOD_DAEMON_STATUS => {
                let payload = decode_payload(payload)?;
                protocol.daemon_status(self, payload)
            }
            METHOD_PING => {
                let payload: PingPayload = decode_payload(payload)?;
                Ok(vec![envelope_value(
                    MessageType::Pong,
                    PongPayload {
                        nonce: payload.nonce,
                        timestamp_ms: current_unix_timestamp_millis(),
                    },
                )?])
            }
            _ => Err(ProtocolError::InvalidEnvelope),
        }
    }

    fn handle_packet_stream_open<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let id = packet.id.ok_or(ProtocolError::InvalidEnvelope)?;
        let stream_id = packet.stream_id.ok_or(ProtocolError::InvalidEnvelope)?;
        let method = packet
            .method
            .clone()
            .ok_or(ProtocolError::InvalidEnvelope)?;
        if method == METHOD_TERMINAL_CREATE {
            let payload = match decode_payload(packet.payload) {
                Ok(payload) => payload,
                Err(error) => return Ok(vec![packet_request_error(id, error)?]),
            };
            return match self.replace_packet_terminal_streams_with_rollback(
                |connection| protocol.create_terminal_stream_session(connection, payload),
                |connection, envelopes| {
                    connection.finish_packet_terminal_stream_open(
                        id,
                        stream_id,
                        method.as_str(),
                        envelopes,
                    )
                },
            ) {
                Ok((packets, removed_attachments)) => {
                    protocol.release_watched_attachments(removed_attachments);
                    Ok(packets)
                }
                Err((error, created_attachments)) => {
                    protocol.release_watched_attachments(created_attachments);
                    Ok(vec![packet_request_error(id, error)?])
                }
            };
        }
        if method == METHOD_TERMINAL_ATTACH {
            let payload = match decode_payload(packet.payload) {
                Ok(payload) => payload,
                Err(error) => return Ok(vec![packet_request_error(id, error)?]),
            };
            return match self.replace_packet_terminal_streams_with_rollback(
                |connection| protocol.attach_terminal_session(connection, payload),
                |connection, envelopes| {
                    connection.finish_packet_terminal_stream_open(
                        id,
                        stream_id,
                        method.as_str(),
                        envelopes,
                    )
                },
            ) {
                Ok((packets, removed_attachments)) => {
                    protocol.release_watched_attachments(removed_attachments);
                    Ok(packets)
                }
                Err((error, created_attachments)) => {
                    protocol.release_watched_attachments(created_attachments);
                    Ok(vec![packet_request_error(id, error)?])
                }
            };
        }

        let responses = match method.as_str() {
            METHOD_SESSION_FILE_UPLOAD_STREAM => {
                let payload = decode_payload(packet.payload)?;
                let (ready, stream) = protocol.prepare_session_file_upload_stream(self, payload)?;
                self.packet_file_upload_streams.insert(stream_id, stream);
                Ok(vec![envelope_value(
                    MessageType::SessionFileWritten,
                    ready,
                )?])
            }
            METHOD_SESSION_FILE_DOWNLOAD_STREAM => {
                let payload = decode_payload(packet.payload)?;
                let (ready, stream) =
                    protocol.prepare_session_file_download_stream(self, payload)?;
                self.packet_file_download_streams.insert(stream_id, stream);
                Ok(vec![envelope_value(
                    MessageType::SessionFileDownloadReady,
                    ready,
                )?])
            }
            _ => Err(ProtocolError::InvalidEnvelope),
        };

        let envelopes = match responses {
            Ok(envelopes) => envelopes,
            Err(error) => return Ok(vec![packet_request_error(id, error)?]),
        };
        packetize_handler_stream_open_response(id, stream_id, method.as_str(), envelopes)
    }

    fn handle_packet_stream_chunk<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let stream_id = packet.stream_id.ok_or(ProtocolError::InvalidEnvelope)?;
        if self.packet_file_upload_streams.contains_key(&stream_id) {
            return self.handle_packet_file_upload_chunk(protocol, stream_id, packet);
        }
        let Some(stream) = self.packet_terminal_streams.get(&stream_id).cloned() else {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidState,
            )?]);
        };
        if packet.seq != stream.next_input_seq {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidEnvelope,
            )?]);
        }
        let payload: AttachFramePayload = match decode_payload(packet.payload) {
            Ok(payload) => payload,
            Err(error) => return Ok(vec![packet_stream_error(stream_id, error)?]),
        };
        self.debug_traffic
            .record_inbound_stream_chunk_payload(&payload.data_base64);
        if payload.session_id != stream.session_id {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidEnvelope,
            )?]);
        }
        let Some(attachment_id) = self.watched_attachment_ids.get(&stream.session_id).cloned()
        else {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidState,
            )?]);
        };
        let bytes = match general_purpose::STANDARD.decode(&payload.data_base64) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Ok(vec![packet_stream_error(
                    stream_id,
                    ProtocolError::InvalidEnvelope,
                )?]);
            }
        };

        if let Err(error) =
            protocol.write_watched_attachment_frame(stream.session_id, &attachment_id, &bytes)
        {
            return Ok(vec![packet_stream_error(stream_id, error)?]);
        }
        if let Some(stream) = self.packet_terminal_streams.get_mut(&stream_id) {
            stream.next_input_seq = stream.next_input_seq.saturating_add(1);
        }
        Ok(Vec::new())
    }

    fn handle_packet_file_upload_chunk<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        stream_id: PacketStreamId,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let Some(mut stream) = self.packet_file_upload_streams.remove(&stream_id) else {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidState,
            )?]);
        };
        if packet.seq != stream.next_input_seq {
            self.packet_file_upload_streams.insert(stream_id, stream);
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidEnvelope,
            )?]);
        }
        let payload: SessionFileTransferChunkPayload = match decode_payload(packet.payload) {
            Ok(payload) => payload,
            Err(error) => {
                self.packet_file_upload_streams.insert(stream_id, stream);
                return Ok(vec![packet_stream_error(stream_id, error)?]);
            }
        };
        let (progress, complete) =
            match protocol.write_session_file_upload_stream_chunk(&mut stream, payload) {
                Ok(result) => result,
                Err(error) => {
                    cleanup_upload_temp(&stream);
                    return Ok(vec![packet_stream_error(stream_id, error)?]);
                }
            };
        let seq = stream.next_output_seq;
        stream.next_output_seq = stream.next_output_seq.saturating_add(1);
        stream.next_input_seq = stream.next_input_seq.saturating_add(1);
        let mut packets = vec![ProtocolPacket::stream_chunk(
            stream_id,
            seq,
            serde_json::to_value(progress).map_err(|_| ProtocolError::InvalidEnvelope)?,
        )];
        if complete {
            packets.push(ProtocolPacket::stream_end(
                stream_id,
                stream.next_output_seq,
                serde_json::json!({}),
            ));
        } else {
            self.packet_file_upload_streams.insert(stream_id, stream);
        }
        Ok(packets)
    }

    fn handle_packet_stream_end<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let stream_id = packet.stream_id.ok_or(ProtocolError::InvalidEnvelope)?;
        if let Some(stream) = self.packet_file_upload_streams.remove(&stream_id) {
            cleanup_upload_temp(&stream);
            return Ok(Vec::new());
        }
        if self
            .packet_file_download_streams
            .remove(&stream_id)
            .is_some()
        {
            return Ok(Vec::new());
        }
        if let Some(stream) = self.packet_terminal_streams.get_mut(&stream_id) {
            if packet.seq == stream.next_input_seq {
                stream.next_input_seq = stream.next_input_seq.saturating_add(1);
            }
        }
        if let Some((session_id, attachment_id)) = self.remove_packet_terminal_stream(stream_id) {
            protocol.release_watched_attachment(session_id, attachment_id);
        }
        Ok(Vec::new())
    }

    fn handle_packet_cancel<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        if let Some(stream_id) = packet.stream_id {
            if let Some(stream) = self.packet_file_upload_streams.remove(&stream_id) {
                cleanup_upload_temp(&stream);
                return Ok(Vec::new());
            }
            if self
                .packet_file_download_streams
                .remove(&stream_id)
                .is_some()
            {
                return Ok(Vec::new());
            }
            if let Some((session_id, attachment_id)) = self.remove_packet_terminal_stream(stream_id)
            {
                protocol.release_watched_attachment(session_id, attachment_id);
            }
            return Ok(Vec::new());
        }
        if packet.id.is_some() {
            return Ok(Vec::new());
        }
        Err(ProtocolError::InvalidEnvelope)
    }

    fn handle_packet_flow<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let stream_id = packet.stream_id.ok_or(ProtocolError::InvalidEnvelope)?;
        if self.packet_file_download_streams.contains_key(&stream_id) {
            return self.handle_packet_file_download_flow(protocol, stream_id, packet);
        }
        // 中文注释：旧客户端可能继续发送 flow。新模型中 WebSocket/TCP 已保证可靠有序，
        // terminal 输出不再等待 render ACK/credit；flow 只保留为兼容 no-op，不能驱动输出。
        let _ = self.packet_terminal_streams.get(&stream_id);
        Ok(Vec::new())
    }

    fn handle_packet_file_download_flow<B, V>(
        &mut self,
        protocol: &mut DaemonProtocol<B, V>,
        stream_id: PacketStreamId,
        packet: ProtocolPacket<Value>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>
    where
        B: PtyBackend,
        V: SignatureVerifier,
    {
        let Some(mut stream) = self.packet_file_download_streams.remove(&stream_id) else {
            return Ok(vec![packet_stream_error(
                stream_id,
                ProtocolError::InvalidState,
            )?]);
        };
        let max_bytes = packet
            .credit
            .unwrap_or(SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES)
            .min(SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES);
        let (chunk, eof) =
            match protocol.read_session_file_download_stream_chunk(&mut stream, max_bytes) {
                Ok(result) => result,
                Err(error) => return Ok(vec![packet_stream_error(stream_id, error)?]),
            };
        let seq = stream.next_output_seq;
        stream.next_output_seq = stream.next_output_seq.saturating_add(1);
        let mut packets = vec![ProtocolPacket::stream_chunk(
            stream_id,
            seq,
            serde_json::to_value(chunk).map_err(|_| ProtocolError::InvalidEnvelope)?,
        )];
        if eof {
            packets.push(ProtocolPacket::stream_end(
                stream_id,
                stream.next_output_seq,
                serde_json::json!({}),
            ));
        } else {
            self.packet_file_download_streams.insert(stream_id, stream);
        }
        Ok(packets)
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
        if self.packet_mode {
            let packets = messages
                .into_iter()
                .map(|message| packet_from_envelope(self, message))
                .collect::<Result<Vec<_>, _>>()?;
            return self.encrypt_packets(packets);
        }

        messages
            .into_iter()
            .map(|message| {
                self.debug_traffic
                    .record_outbound_legacy_envelope(message.kind);
                let frame = self.e2ee_mut()?.encrypt_json_payload(&message)?;
                envelope_value(MessageType::EncryptedFrame, frame)
            })
            .collect()
    }

    fn encrypt_inner_messages_wire(
        &mut self,
        messages: Vec<JsonEnvelope>,
    ) -> Result<Vec<ProtocolWireMessage>, ProtocolError> {
        if self.packet_mode {
            let packets = messages
                .into_iter()
                .map(|message| packet_from_envelope(self, message))
                .collect::<Result<Vec<_>, _>>()?;
            return self.encrypt_packets_wire(packets);
        }

        messages
            .into_iter()
            .map(|message| {
                self.debug_traffic
                    .record_outbound_legacy_envelope(message.kind);
                let frame = self.e2ee_mut()?.encrypt_json_payload(&message)?;
                let envelope = envelope_value(MessageType::EncryptedFrame, frame)?;
                Ok(ProtocolWireMessage::Json(envelope))
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

    fn error_response_wire(&mut self, error: ProtocolError) -> ProtocolWireMessage {
        let error_envelope = envelope_value(
            MessageType::Error,
            ErrorPayload {
                code: error.code().to_owned(),
                message: error.safe_message().to_owned(),
            },
        )
        .expect("error payload should serialize");

        if self.e2ee.is_some() {
            if let Ok(mut encrypted) =
                self.encrypt_inner_messages_wire(vec![error_envelope.clone()])
            {
                if let Some(message) = encrypted.pop() {
                    return message;
                }
            }
        }

        ProtocolWireMessage::Json(error_envelope)
    }

    fn encrypt_packets(
        &mut self,
        packets: Vec<ProtocolPacket<Value>>,
    ) -> Result<Vec<JsonEnvelope>, ProtocolError> {
        packets
            .into_iter()
            .map(|packet| {
                self.debug_traffic.record_outbound_packet(&packet);
                let frame = self
                    .e2ee_mut()?
                    .encrypt_json_payload(&Envelope::new(MessageType::Packet, packet))?;
                envelope_value(MessageType::EncryptedFrame, frame)
            })
            .collect()
    }

    fn encrypt_packets_wire(
        &mut self,
        packets: Vec<ProtocolPacket<Value>>,
    ) -> Result<Vec<ProtocolWireMessage>, ProtocolError> {
        packets
            .into_iter()
            .map(|packet| {
                self.debug_traffic.record_outbound_packet(&packet);
                if self.binary_mode {
                    let binary = protocol_packet_to_binary(packet)
                        .map_err(|_| ProtocolError::InvalidEnvelope)?;
                    let plaintext = encode_binary_protocol_packet(&binary);
                    let frame = self.e2ee_mut()?.encrypt_binary_payload(&plaintext)?;
                    return Ok(ProtocolWireMessage::Binary(encode_binary_encrypted_frame(
                        &frame,
                    )));
                }
                let frame = self
                    .e2ee_mut()?
                    .encrypt_json_payload(&Envelope::new(MessageType::Packet, packet))?;
                let envelope = envelope_value(MessageType::EncryptedFrame, frame)?;
                Ok(ProtocolWireMessage::Json(envelope))
            })
            .collect()
    }

    fn register_packet_terminal_stream(
        &mut self,
        stream_id: PacketStreamId,
        session_id: SessionId,
    ) {
        self.packet_terminal_streams_by_session
            .insert(session_id, stream_id);
        self.packet_terminal_streams
            .insert(stream_id, PacketTerminalStream::new(session_id));
    }

    fn replace_packet_terminal_streams_with_rollback(
        &mut self,
        replace: impl FnOnce(&mut Self) -> Result<Vec<JsonEnvelope>, ProtocolError>,
        finish: impl FnOnce(
            &mut Self,
            Vec<JsonEnvelope>,
        ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError>,
    ) -> Result<
        (Vec<ProtocolPacket<Value>>, Vec<(SessionId, String)>),
        (ProtocolError, Vec<(SessionId, String)>),
    > {
        let snapshot = self.snapshot_packet_terminal_stream_state();
        // 中文注释：同一条 WebSocket 连接只有一个活跃 terminal 输出流。只有在新
        // stream-open payload 已成功解码后才清旧流；如果后续 attach/create 失败，
        // 或响应转换失败，立即回滚，避免坏请求切断仍可用的旧输出流。
        let removed_attachments = self.clear_packet_terminal_streams();
        let envelopes = match replace(self) {
            Ok(envelopes) => envelopes,
            Err(error) => {
                let created_attachments = self.watched_attachments_created_since(&snapshot);
                self.restore_packet_terminal_stream_state(snapshot);
                return Err((error, created_attachments));
            }
        };
        match finish(self, envelopes) {
            Ok(packets) => Ok((packets, removed_attachments)),
            Err(error) => {
                let created_attachments = self.watched_attachments_created_since(&snapshot);
                self.restore_packet_terminal_stream_state(snapshot);
                Err((error, created_attachments))
            }
        }
    }

    fn finish_packet_terminal_stream_open(
        &mut self,
        id: PacketRequestId,
        stream_id: PacketStreamId,
        method: &str,
        envelopes: Vec<JsonEnvelope>,
    ) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError> {
        let session_id = packet_stream_session_id(method, &envelopes)?;
        self.register_packet_terminal_stream(stream_id, session_id);
        packetize_handler_stream_open_response(id, stream_id, method, envelopes)
    }

    fn snapshot_packet_terminal_stream_state(&self) -> PacketTerminalStreamStateSnapshot {
        PacketTerminalStreamStateSnapshot {
            state: self.state,
            packet_terminal_streams: self.packet_terminal_streams.clone(),
            packet_terminal_streams_by_session: self.packet_terminal_streams_by_session.clone(),
            attached_sessions: self.attached_sessions.clone(),
            watched_sessions: self.watched_sessions.clone(),
            watched_cwd_versions: self.watched_cwd_versions.clone(),
            watched_cwd_paths: self.watched_cwd_paths.clone(),
            watched_attachment_ids: self.watched_attachment_ids.clone(),
            next_watched_attachment_number: self.next_watched_attachment_number,
            stale_watched_sessions: self.stale_watched_sessions.clone(),
            pending_attach_frames: self.pending_attach_frames.clone(),
            output_offsets: self.output_offsets.clone(),
            pending_outputs: self.pending_outputs.clone(),
            deferred_output_wakeups: self.deferred_output_wakeups.clone(),
        }
    }

    fn restore_packet_terminal_stream_state(
        &mut self,
        snapshot: PacketTerminalStreamStateSnapshot,
    ) {
        self.state = snapshot.state;
        self.packet_terminal_streams = snapshot.packet_terminal_streams;
        self.packet_terminal_streams_by_session = snapshot.packet_terminal_streams_by_session;
        self.attached_sessions = snapshot.attached_sessions;
        self.watched_sessions = snapshot.watched_sessions;
        self.watched_cwd_versions = snapshot.watched_cwd_versions;
        self.watched_cwd_paths = snapshot.watched_cwd_paths;
        self.watched_attachment_ids = snapshot.watched_attachment_ids;
        self.next_watched_attachment_number = snapshot.next_watched_attachment_number;
        self.stale_watched_sessions = snapshot.stale_watched_sessions;
        self.pending_attach_frames = snapshot.pending_attach_frames;
        self.output_offsets = snapshot.output_offsets;
        self.pending_outputs = snapshot.pending_outputs;
        self.deferred_output_wakeups = snapshot.deferred_output_wakeups;
    }

    fn watched_attachments_created_since(
        &self,
        snapshot: &PacketTerminalStreamStateSnapshot,
    ) -> Vec<(SessionId, String)> {
        self.watched_attachment_ids
            .iter()
            .filter(|(session_id, attachment_id)| {
                snapshot
                    .watched_attachment_ids
                    .get(session_id)
                    .map_or(true, |snapshot_id| snapshot_id != *attachment_id)
            })
            .map(|(session_id, attachment_id)| (*session_id, attachment_id.clone()))
            .collect()
    }

    fn clear_packet_terminal_streams(&mut self) -> Vec<(SessionId, String)> {
        let mut removed_attachments = Vec::new();
        for session_id in self.packet_terminal_streams_by_session.keys() {
            // 中文注释：packet terminal stream 表示当前终端输出订阅。
            // 快速切换 session 时旧 stream 清掉后也必须取消 watched 状态，否则 relay/直连
            // watcher 仍会为旧 session 产生唤醒，继续占用输出队列。
            if self.watched_sessions.remove(session_id) {
                self.watched_cwd_versions.remove(session_id);
                self.watched_cwd_paths.remove(session_id);
                self.stale_watched_sessions.insert(*session_id);
                if let Some(attachment_id) = self.watched_attachment_ids.remove(session_id) {
                    removed_attachments.push((*session_id, attachment_id));
                }
            }
        }
        self.packet_terminal_streams.clear();
        self.packet_terminal_streams_by_session.clear();
        self.pending_attach_frames.clear();
        self.deferred_output_wakeups.clear();
        removed_attachments
    }

    fn remove_packet_terminal_stream(
        &mut self,
        stream_id: PacketStreamId,
    ) -> Option<(SessionId, String)> {
        let Some(stream) = self.packet_terminal_streams.remove(&stream_id) else {
            return None;
        };
        self.packet_terminal_streams_by_session
            .remove(&stream.session_id);
        self.pending_attach_frames.remove(&stream.session_id);
        self.deferred_output_wakeups.remove(&stream.session_id);
        if self.watched_sessions.remove(&stream.session_id) {
            self.watched_cwd_versions.remove(&stream.session_id);
            self.watched_cwd_paths.remove(&stream.session_id);
            self.stale_watched_sessions.insert(stream.session_id);
            return self
                .watched_attachment_ids
                .remove(&stream.session_id)
                .map(|attachment_id| (stream.session_id, attachment_id));
        }
        None
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

    fn packet_stream_id_for_session(&self, session_id: SessionId) -> Option<PacketStreamId> {
        self.packet_terminal_streams_by_session
            .get(&session_id)
            .copied()
    }

    fn next_packet_stream_output_seq(
        &mut self,
        session_id: SessionId,
    ) -> Option<(PacketStreamId, u64)> {
        let stream_id = self.packet_stream_id_for_session(session_id)?;
        let stream = self.packet_terminal_streams.get_mut(&stream_id)?;

        let seq = stream.next_output_seq;
        stream.next_output_seq = stream.next_output_seq.saturating_add(1);
        Some((stream_id, seq))
    }

    fn attach(
        &mut self,
        session_id: SessionId,
        output_base_offset: u64,
        initial_output: Vec<u8>,
        watch_updates: bool,
    ) {
        if !self.attached_sessions.contains(&session_id) {
            self.attached_sessions.push(session_id);
        }
        if watch_updates {
            self.stale_watched_sessions.remove(&session_id);
            self.watched_sessions.insert(session_id);
            self.watched_cwd_versions.insert(session_id, 0);
            self.watched_cwd_paths.insert(session_id, None);
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

#[cfg(test)]
#[allow(dead_code)]
fn base64_payload_credit_cost(data_base64: &str) -> u32 {
    let decoded_len = base64_payload_decoded_len(data_base64);
    decoded_len
        .max(TERMINAL_STREAM_METADATA_CREDIT_BYTES)
        .min(u32::MAX as usize) as u32
}

fn base64_payload_decoded_len(data_base64: &str) -> usize {
    let trimmed = data_base64.trim_end_matches('=');
    trimmed.len().saturating_mul(3) / 4
}

#[cfg(test)]
#[allow(dead_code)]
fn terminal_frame_payload_bytes(frame: &TerminalFramePayload) -> usize {
    match frame {
        TerminalFramePayload::Snapshot { data_base64, .. }
        | TerminalFramePayload::Output { data_base64, .. } => {
            base64_payload_credit_cost(data_base64) as usize
        }
        TerminalFramePayload::Resize { .. } | TerminalFramePayload::Exit { .. } => {
            TERMINAL_STREAM_METADATA_CREDIT_BYTES
        }
        TerminalFramePayload::Batch { frames, .. } => frames
            .iter()
            .map(terminal_frame_payload_bytes)
            .sum::<usize>()
            .max(TERMINAL_STREAM_METADATA_CREDIT_BYTES),
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn terminal_frame_payload_count(frame: &TerminalFramePayload) -> usize {
    match frame {
        TerminalFramePayload::Batch { frames, .. } => frames
            .iter()
            .map(terminal_frame_payload_count)
            .sum::<usize>()
            .max(1),
        TerminalFramePayload::Snapshot { .. }
        | TerminalFramePayload::Output { .. }
        | TerminalFramePayload::Resize { .. }
        | TerminalFramePayload::Exit { .. } => 1,
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn terminal_frame_transport_cost(frame: &TerminalFramePayload) -> usize {
    match frame {
        TerminalFramePayload::Batch { frames, .. } => {
            TERMINAL_STREAM_BATCH_TRANSPORT_OVERHEAD_BYTES.saturating_add(
                frames
                    .iter()
                    .map(terminal_frame_transport_cost)
                    .sum::<usize>(),
            )
        }
        TerminalFramePayload::Snapshot { data_base64, .. }
        | TerminalFramePayload::Output { data_base64, .. } => {
            // 中文注释：输出热路径还在 daemon-wide protocol 锁内；这里不能为了估算
            // batch 大小再做一次完整 JSON 序列化。base64 长度加固定元数据余量足够作为
            // transport 上限近似，真正编码和加密仍在释放锁后完成。
            data_base64
                .len()
                .saturating_add(TERMINAL_STREAM_FRAME_TRANSPORT_OVERHEAD_BYTES)
        }
        TerminalFramePayload::Resize { .. } | TerminalFramePayload::Exit { .. } => {
            TERMINAL_STREAM_FRAME_TRANSPORT_OVERHEAD_BYTES
        }
    }
}

#[cfg(test)]
fn terminal_frame_fits_output_batch(
    current_bytes: usize,
    current_transport_bytes: usize,
    frame_bytes: usize,
    frame_transport_bytes: usize,
) -> bool {
    let frame_bytes = frame_bytes.max(TERMINAL_STREAM_METADATA_CREDIT_BYTES);
    if current_bytes == 0 {
        // 中文注释：terminal frame 是不可拆的协议边界；单个 snapshot/output 可能大于
        // batch 或 transport 上限，此时允许它独占一个 stream_chunk，避免大 snapshot 永久卡住。
        return true;
    }
    current_bytes.saturating_add(frame_bytes) <= TERMINAL_STREAM_BATCH_MAX_BYTES
        && current_transport_bytes.saturating_add(frame_transport_bytes)
            <= TERMINAL_STREAM_BATCH_MAX_TRANSPORT_BYTES
}

fn packetize_handler_responses(
    id: PacketRequestId,
    method: &str,
    envelopes: Vec<JsonEnvelope>,
) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError> {
    if envelopes.is_empty() {
        return Ok(vec![ProtocolPacket::response(
            id,
            method,
            serde_json::json!({}),
        )]);
    }

    let mut packets = Vec::with_capacity(envelopes.len());
    for (index, envelope) in envelopes.into_iter().enumerate() {
        if envelope.kind == MessageType::Error {
            let error: ErrorPayload = decode_payload(envelope.payload)?;
            packets.push(packet_request_error_from_payload(
                id,
                error.code,
                error.message,
                false,
            )?);
            continue;
        }

        if index == 0 {
            packets.push(ProtocolPacket::response(id, method, envelope.payload));
        } else {
            let event_method = packet_event_method_for_message(envelope.kind)
                .unwrap_or(method)
                .to_owned();
            packets.push(ProtocolPacket::event(event_method, envelope.payload));
        }
    }

    Ok(packets)
}

fn packetize_handler_stream_open_response(
    id: PacketRequestId,
    stream_id: PacketStreamId,
    method: &str,
    envelopes: Vec<JsonEnvelope>,
) -> Result<Vec<ProtocolPacket<Value>>, ProtocolError> {
    let mut packets = packetize_handler_responses(id, method, envelopes)?;
    if let Some(first) = packets.first_mut() {
        first.stream_id = Some(stream_id);
    }
    Ok(packets)
}

fn packet_stream_session_id(
    method: &str,
    envelopes: &[JsonEnvelope],
) -> Result<SessionId, ProtocolError> {
    let first = envelopes.first().ok_or(ProtocolError::InvalidEnvelope)?;
    match method {
        METHOD_TERMINAL_CREATE => {
            let payload: SessionCreatedPayload = decode_payload(first.payload.clone())?;
            Ok(payload.session_id)
        }
        METHOD_TERMINAL_ATTACH => {
            let payload: SessionAttachedPayload = decode_payload(first.payload.clone())?;
            Ok(payload.session_id)
        }
        _ => Err(ProtocolError::InvalidEnvelope),
    }
}

fn packet_from_envelope(
    connection: &mut ProtocolConnection,
    envelope: JsonEnvelope,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    if envelope.kind == MessageType::Error {
        let error: ErrorPayload = decode_payload(envelope.payload)?;
        return packet_unbound_error_from_payload(error.code, error.message, false);
    }

    #[cfg(test)]
    if envelope.kind == MessageType::SessionData {
        let payload: SessionDataPayload = decode_payload(envelope.payload)?;
        let (stream_id, seq) = connection
            .next_packet_stream_output_seq(payload.session_id)
            .ok_or(ProtocolError::InvalidState)?;
        let payload = serde_json::to_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)?;
        return Ok(ProtocolPacket::stream_chunk(stream_id, seq, payload));
    }

    #[cfg(test)]
    if envelope.kind == MessageType::TerminalFrame {
        let payload: TerminalFramePayload = decode_payload(envelope.payload)?;
        let (stream_id, seq) = connection
            .next_packet_stream_output_seq(payload.session_id())
            .ok_or(ProtocolError::InvalidState)?;
        let payload = serde_json::to_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)?;
        return Ok(ProtocolPacket::stream_chunk(stream_id, seq, payload));
    }

    if envelope.kind == MessageType::AttachFrame {
        let payload: AttachFramePayload = decode_payload(envelope.payload)?;
        let (stream_id, seq) = connection
            .next_packet_stream_output_seq(payload.session_id)
            .ok_or(ProtocolError::InvalidState)?;
        let payload = if connection.binary_mode {
            attach_frame_payload_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)?
        } else {
            serde_json::to_value(payload).map_err(|_| ProtocolError::InvalidEnvelope)?
        };
        return Ok(ProtocolPacket::stream_chunk(stream_id, seq, payload));
    }

    let method =
        packet_event_method_for_message(envelope.kind).ok_or(ProtocolError::InvalidEnvelope)?;
    Ok(ProtocolPacket::event(method, envelope.payload))
}

fn packet_bound_error(
    packet: ProtocolPacket<Value>,
    error: ProtocolError,
) -> Option<ProtocolPacket<Value>> {
    if let Some(id) = packet.id {
        return packet_request_error(id, error).ok();
    }
    if let Some(stream_id) = packet.stream_id {
        return packet_stream_error(stream_id, error).ok();
    }
    packet_unbound_error(error).ok()
}

fn packet_request_error(
    id: PacketRequestId,
    error: ProtocolError,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    packet_request_error_from_payload(
        id,
        error.code().to_owned(),
        error.safe_message().to_owned(),
        false,
    )
}

fn packet_stream_error(
    stream_id: PacketStreamId,
    error: ProtocolError,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    packet_stream_error_from_payload(
        stream_id,
        error.code().to_owned(),
        error.safe_message().to_owned(),
        false,
    )
}

fn packet_unbound_error(error: ProtocolError) -> Result<ProtocolPacket<Value>, ProtocolError> {
    packet_unbound_error_from_payload(
        error.code().to_owned(),
        error.safe_message().to_owned(),
        false,
    )
}

fn packet_request_error_from_payload(
    id: PacketRequestId,
    code: String,
    message: String,
    retryable: bool,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    let payload = packet_error_payload_value(code, message, retryable)?;
    Ok(ProtocolPacket {
        version: PROTOCOL_PACKET_VERSION,
        kind: PacketKind::Error,
        id: Some(id),
        stream_id: None,
        method: None,
        seq: 0,
        ack: None,
        credit: None,
        payload,
    })
}

fn packet_stream_error_from_payload(
    stream_id: PacketStreamId,
    code: String,
    message: String,
    retryable: bool,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    let payload = packet_error_payload_value(code, message, retryable)?;
    Ok(ProtocolPacket {
        version: PROTOCOL_PACKET_VERSION,
        kind: PacketKind::Error,
        id: None,
        stream_id: Some(stream_id),
        method: None,
        seq: 0,
        ack: None,
        credit: None,
        payload,
    })
}

fn packet_unbound_error_from_payload(
    code: String,
    message: String,
    retryable: bool,
) -> Result<ProtocolPacket<Value>, ProtocolError> {
    let payload = packet_error_payload_value(code, message, retryable)?;
    Ok(ProtocolPacket {
        version: PROTOCOL_PACKET_VERSION,
        kind: PacketKind::Error,
        id: None,
        stream_id: None,
        method: None,
        seq: 0,
        ack: None,
        credit: None,
        payload,
    })
}

fn packet_error_payload_value(
    code: String,
    message: String,
    retryable: bool,
) -> Result<Value, ProtocolError> {
    serde_json::to_value(PacketErrorPayload {
        code,
        message,
        retryable,
    })
    .map_err(|_| ProtocolError::InvalidEnvelope)
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
) -> bool {
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
    let Ok(output) = output else {
        return false;
    };
    if !output.success {
        return false;
    }
    parse_git_status_entries(&output.stdout)
        .into_iter()
        .any(|change| {
            path_matches_active_session_file_http_upload_target(
                &worktree.join(change.path),
                active_targets,
            )
        })
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

#[cfg(test)]
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
    stdout: String,
    stderr: String,
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

    let current_root_output =
        run_git_command(cwd, &["rev-parse", "--show-toplevel"]).unwrap_or(repo_output);
    let current_root = first_non_empty_line(&current_root_output.stdout)
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| canonical_repo_root.clone());
    let worktree_infos = read_git_worktrees(&canonical_repo_root, &current_root);
    let worktrees = worktree_infos
        .into_iter()
        .map(|worktree| {
            let (staged, unstaged) =
                read_git_worktree_changes(&worktree.path, active_upload_targets);
            let is_current = same_path(&worktree.path, &current_root);
            SessionGitWorktreePayload {
                path: absolute_path_string(&worktree.path),
                branch: worktree.branch,
                head: worktree.head,
                is_current,
                staged,
                unstaged,
            }
        })
        .collect();

    SessionGitResultPayload {
        session_id,
        cwd: cwd_text,
        repository_root: Some(repository_root_text),
        worktrees,
        graph: read_git_graph(&canonical_repo_root),
        error: None,
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
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    Ok(GitCommandResult {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn current_git_repository_root(cwd: &Path) -> Option<PathBuf> {
    let output = run_git_command(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    if !output.success {
        return None;
    }
    first_non_empty_line(&output.stdout)
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
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
    if output.success {
        return Ok(());
    }

    tracing::debug!(stderr = %output.stderr, "git action failed detail");
    tracing::warn!("git action failed");
    Err(ProtocolError::RuntimeFailed)
}

fn read_git_worktrees(repo_root: &Path, current_root: &Path) -> Vec<GitWorktreeInfo> {
    let mut worktrees = run_git_command(repo_root, &["worktree", "list", "--porcelain"])
        .ok()
        .filter(|output| output.success)
        .map(|output| parse_git_worktrees(&output.stdout))
        .unwrap_or_default();
    if worktrees.is_empty() {
        worktrees.push(GitWorktreeInfo {
            path: current_root.to_path_buf(),
            branch: read_git_branch(current_root),
            head: read_git_head(current_root),
        });
    }

    worktrees
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
            if let Some(worktree) = current.as_mut() {
                worktree.head = Some(short_git_hash(head));
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(worktree) = current.as_mut() {
                worktree.branch = Some(short_branch_name(branch));
            }
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

fn read_git_branch(worktree: &Path) -> Option<String> {
    let output = run_git_command(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    if !output.success {
        return None;
    }
    let branch = first_non_empty_line(&output.stdout)?;
    if branch == "HEAD" {
        None
    } else {
        Some(branch.to_owned())
    }
}

fn read_git_head(worktree: &Path) -> Option<String> {
    let output = run_git_command(worktree, &["rev-parse", "--short", "HEAD"]).ok()?;
    if output.success {
        first_non_empty_line(&output.stdout).map(ToOwned::to_owned)
    } else {
        None
    }
}

fn read_git_worktree_changes(
    worktree: &Path,
    active_upload_targets: &[ActiveSessionFileHttpUploadTarget],
) -> (
    Vec<SessionGitFileChangePayload>,
    Vec<SessionGitFileChangePayload>,
) {
    let Some(output) = run_git_command(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )
    .ok()
    .filter(|output| output.success) else {
        return (Vec::new(), Vec::new());
    };
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

    (staged, unstaged)
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

fn read_git_graph(repo_root: &Path) -> Vec<String> {
    run_git_command(
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
    .ok()
    .filter(|output| output.success)
    .map(|output| {
        output
            .stdout
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

fn git_error_message(output: &GitCommandResult, fallback: &'static str) -> String {
    first_non_empty_line(&output.stderr)
        .or_else(|| first_non_empty_line(&output.stdout))
        .unwrap_or(fallback)
        .to_owned()
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
        // 中文注释：存在 recovery record 就表示 upload 尚未完成 commit；启动恢复时
        // 只删除与 record identity 匹配的预分配目标。如果用户已经替换了该路径，保留新文件。
        let expected_identity = session_file_http_upload_identity_from_recovery_record(&record);
        let cleanup = remove_persisted_session_file_http_upload_target(
            &record.target_path,
            expected_identity,
        );
        if let Err(error) = &cleanup {
            tracing::debug!(
                %error,
                upload_id = %record.upload_id,
                target = %record.target_path.display(),
                "failed to cleanup stale HTTP upload recovery target detail"
            );
            tracing::warn!(
                upload_id = %record.upload_id,
                "failed to cleanup stale HTTP upload recovery target"
            );
            return Err(StateError::Read {
                path: record.target_path,
                source: std::io::Error::other("failed to cleanup stale HTTP upload target"),
            });
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
    ensure_session_file_http_upload_target_identity(&plan.target, plan.file_identity)?;
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
        options.custom_flags(libc::O_NOFOLLOW);
    }
    // 中文注释：init 在最终路径创建新文件并设置声明大小；后续请求只 seek 写这个文件。
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
        // 中文注释：state 尚未登记时 set_len 失败，必须清理刚创建的最终目标文件；
        // 否则下一次 init 会被 create_new 的 AlreadyExists 卡住。
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
        return metadata.nlink() > 1;
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
        return metadata.nlink() > 0;
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

fn cleanup_upload_temp(stream: &PacketFileUploadStream) {
    let _ = fs::remove_file(&stream.temp_path);
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
        BinaryPacketKind, BinaryProtocolPacket, BinaryTerminalFrameKind,
        BinaryTerminalFramePayload, BinaryTerminalSize, METHOD_AUTH_CHALLENGE, PairAcceptPayload,
        PairingToken, PublicKey, SessionFileDeletePayload, SessionFileDeletedPayload,
        SessionFileKind, SessionFileReadPayload, SessionFileReadResultPayload,
        SessionFileWritePayload, SessionFileWrittenPayload, SessionFilesPayload,
        SessionFilesResultPayload, SessionGitActionKind, SessionGitActionPayload,
        SessionGitActionResultPayload, SessionGitPayload, SessionGitResultPayload, Signature,
        binary_protocol_packet,
    };

    use super::*;
    use crate::auth::{AuthSigningInput, HttpE2eeSigningInput};
    use crate::net::signature::Ed25519SignatureVerifier;
    use crate::pty::supervisor::{
        SupervisorTerminalClientFrame, SupervisorTerminalServerFrame,
        decode_supervisor_terminal_client_frame, decode_supervisor_terminal_server_frame,
        encode_supervisor_terminal_client_frame, encode_supervisor_terminal_server_frame,
    };
    use crate::pty::{
        PtyAttachment, PtyBackend, PtyError, PtyExitStatus, PtyRestoreInfo, PtyResult, PtySession,
        PtySize, PtySnapshot, PtySupervisorStatus, PtyTerminalFrame,
    };
    use crate::session::TerminalSize as RuntimeTerminalSize;
    use crate::state::{StateStore, client_history::ClientHistoryStore};
    use tokio::sync::watch;

    static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn terminal_output_batch_allows_megabyte_scale_aggregation() {
        let nearly_full = 496 * 1024;
        let next_frame = 16 * 1024;

        // 中文注释：relay/direct 输出不应该被 64KB 级别的小 batch 限死。
        // 512KB 仍低于 WebSocket 单帧上限，但已经能显著减少加密、封包和调度次数。
        assert!(terminal_frame_fits_output_batch(
            nearly_full,
            nearly_full,
            next_frame,
            next_frame,
        ));
    }

    #[derive(Clone, Default)]
    struct FakePtyBackend {
        state: Arc<Mutex<FakePtyState>>,
    }

    #[derive(Clone, Debug)]
    struct FakeAttachmentHandle {
        attachment_id: String,
        pending_frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
        output_signal_tx: watch::Sender<u64>,
    }

    #[derive(Debug, Default)]
    struct FakePtyState {
        outputs: VecDeque<Vec<u8>>,
        outputs_by_session: HashMap<String, VecDeque<Vec<u8>>>,
        terminal_seq_by_session: HashMap<String, u64>,
        terminal_journal_by_session: HashMap<String, Vec<PtyTerminalFrame>>,
        terminal_frames_by_session: HashMap<String, VecDeque<PtyTerminalFrame>>,
        terminal_screen_by_session: HashMap<String, TerminalScreen>,
        terminal_size_by_session: HashMap<String, PtySize>,
        terminal_snapshot_count_by_session: HashMap<String, usize>,
        cwd_by_session: HashMap<String, PathBuf>,
        cwd_read_count_by_session: HashMap<String, usize>,
        writes: Vec<Vec<u8>>,
        attachment_handles_by_session: HashMap<String, Vec<FakeAttachmentHandle>>,
        attachment_writes_by_session: HashMap<String, Vec<Vec<u8>>>,
        attachment_starts: Vec<String>,
        attachment_drops: Vec<String>,
        reconnects: Vec<String>,
        reconnect_sizes: Vec<PtySize>,
        read_error: Option<String>,
        reconnect_error: Option<String>,
        terminate_error: Option<String>,
        terminate_count: usize,
    }

    impl FakePtyBackend {
        fn push_output(&self, bytes: impl Into<Vec<u8>>) {
            self.state.lock().unwrap().outputs.push_back(bytes.into());
        }

        fn push_output_for_session(&self, session_id: SessionId, bytes: impl Into<Vec<u8>>) {
            let mut state = self.state.lock().unwrap();
            let session_key = session_id.0.to_string();
            let bytes = bytes.into();
            state
                .outputs_by_session
                .entry(session_key.clone())
                .or_default()
                .push_back(bytes.clone());

            let seq = next_fake_terminal_seq(&mut state, &session_key);
            let frame = PtyTerminalFrame::Output {
                terminal_seq: seq,
                data: bytes,
            };
            push_fake_terminal_journal(&mut state, &session_key, frame.clone());
            apply_fake_terminal_frame_to_screen(&mut state, &session_key, &frame);
            broadcast_fake_attachment_frame(&mut state, &session_key, frame);
        }

        fn set_cwd_for_session(&self, session_id: SessionId, cwd: impl Into<PathBuf>) {
            self.state
                .lock()
                .unwrap()
                .cwd_by_session
                .insert(session_id.0.to_string(), cwd.into());
        }

        fn cwd_read_count_for_session(&self, session_id: SessionId) -> usize {
            self.state
                .lock()
                .unwrap()
                .cwd_read_count_by_session
                .get(&session_id.0.to_string())
                .copied()
                .unwrap_or(0)
        }

        fn push_terminal_journal_frame_for_session(
            &self,
            session_id: SessionId,
            frame: PtyTerminalFrame,
        ) {
            let mut state = self.state.lock().unwrap();
            let session_key = session_id.0.to_string();
            let terminal_seq = frame.terminal_seq().unwrap_or(0);
            state
                .terminal_seq_by_session
                .entry(session_key.clone())
                .and_modify(|current| *current = (*current).max(terminal_seq))
                .or_insert(terminal_seq);
            push_fake_terminal_journal(&mut state, &session_key, frame.clone());
            apply_fake_terminal_frame_to_screen(&mut state, &session_key, &frame);
            broadcast_fake_attachment_frame(&mut state, &session_key, frame);
        }

        fn terminal_snapshot_count_for_session(&self, session_id: SessionId) -> usize {
            self.state
                .lock()
                .unwrap()
                .terminal_snapshot_count_by_session
                .get(&session_id.0.to_string())
                .copied()
                .unwrap_or(0)
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        fn attachment_writes_for_session(&self, session_id: SessionId) -> Vec<Vec<u8>> {
            self.state
                .lock()
                .unwrap()
                .attachment_writes_by_session
                .get(&session_id.0.to_string())
                .cloned()
                .unwrap_or_default()
        }

        fn attachment_starts(&self) -> Vec<String> {
            self.state.lock().unwrap().attachment_starts.clone()
        }

        fn attachment_drops(&self) -> Vec<String> {
            self.state.lock().unwrap().attachment_drops.clone()
        }

        fn terminate_count(&self) -> usize {
            self.state.lock().unwrap().terminate_count
        }

        fn reconnects(&self) -> Vec<String> {
            self.state.lock().unwrap().reconnects.clone()
        }

        fn reconnect_sizes(&self) -> Vec<PtySize> {
            self.state.lock().unwrap().reconnect_sizes.clone()
        }

        fn fail_reconnects(&self, message: impl Into<String>) {
            self.state.lock().unwrap().reconnect_error = Some(message.into());
        }

        fn fail_reads(&self, message: impl Into<String>) {
            self.state.lock().unwrap().read_error = Some(message.into());
        }

        fn allow_reads(&self) {
            self.state.lock().unwrap().read_error = None;
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
            size: PtySize,
        ) -> PtyResult<Box<dyn PtySession>> {
            let mut state = self.state.lock().unwrap();
            state.reconnects.push(session_id.to_owned());
            state.reconnect_sizes.push(size);
            if let Some(message) = state.reconnect_error.clone() {
                return Err(PtyError::Backend(message));
            }

            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
                session_id: Some(session_id.to_owned()),
                restore_info: Some(restore_info.clone()),
            }))
        }

        fn attach_client(
            &self,
            session_id: &str,
            _restore_info: Option<&PtyRestoreInfo>,
            _size: PtySize,
            attachment_id: &str,
            bootstrap: PtyAttachmentBootstrap,
        ) -> PtyResult<Box<dyn PtyAttachment>> {
            let (output_signal_tx, output_signal_rx) = watch::channel(0);
            let pending_frames = Arc::new(Mutex::new(VecDeque::new()));
            let mut state = self.state.lock().unwrap();
            state.attachment_starts.push(attachment_id.to_owned());
            state
                .attachment_handles_by_session
                .entry(session_id.to_owned())
                .or_default()
                .push(FakeAttachmentHandle {
                    attachment_id: attachment_id.to_owned(),
                    pending_frames: Arc::clone(&pending_frames),
                    output_signal_tx: output_signal_tx.clone(),
                });
            pending_frames
                .lock()
                .unwrap()
                .push_back(fake_attachment_attach_sync_frame(
                    &state, session_id, bootstrap,
                )?);
            let _ = output_signal_tx.send(1);
            Ok(Box::new(FakePtyAttachment {
                state: Arc::clone(&self.state),
                session_id: session_id.to_owned(),
                attachment_id: attachment_id.to_owned(),
                pending_frames,
                output_signal_tx,
                output_signal_rx,
            }))
        }
    }

    struct FakePtyAttachment {
        state: Arc<Mutex<FakePtyState>>,
        session_id: String,
        attachment_id: String,
        pending_frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
        output_signal_tx: watch::Sender<u64>,
        output_signal_rx: watch::Receiver<u64>,
    }

    impl PtyAttachment for FakePtyAttachment {
        fn output_signal(&self) -> Option<watch::Receiver<u64>> {
            Some(self.output_signal_rx.clone())
        }

        fn read_frame(&mut self) -> PtyResult<Option<Vec<u8>>> {
            let frame = self.pending_frames.lock().unwrap().pop_front();
            if self.pending_frames.lock().unwrap().front().is_some() {
                let next = self.output_signal_tx.borrow().wrapping_add(1);
                let _ = self.output_signal_tx.send(next);
            }
            Ok(frame)
        }

        fn write_frame(&mut self, bytes: &[u8]) -> PtyResult<()> {
            let mut state = self.state.lock().unwrap();
            state
                .attachment_writes_by_session
                .entry(self.session_id.clone())
                .or_default()
                .push(bytes.to_vec());

            match decode_supervisor_terminal_client_frame(bytes)? {
                SupervisorTerminalClientFrame::Input { data } => {
                    state.writes.push(data);
                }
                SupervisorTerminalClientFrame::Resize { size } => {
                    let seq = next_fake_terminal_seq(&mut state, &self.session_id);
                    let frame = PtyTerminalFrame::Resize {
                        terminal_seq: seq,
                        size,
                    };
                    push_fake_terminal_journal(&mut state, &self.session_id, frame.clone());
                    apply_fake_terminal_frame_to_screen(&mut state, &self.session_id, &frame);
                    broadcast_fake_attachment_frame(&mut state, &self.session_id, frame);
                }
                SupervisorTerminalClientFrame::HeartbeatPong { .. }
                | SupervisorTerminalClientFrame::BootstrapAttach { .. } => {}
            }
            Ok(())
        }
    }

    impl Drop for FakePtyAttachment {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            state.attachment_drops.push(self.attachment_id.clone());
            if let Some(handles) = state
                .attachment_handles_by_session
                .get_mut(&self.session_id)
            {
                handles.retain(|handle| handle.attachment_id != self.attachment_id);
                if handles.is_empty() {
                    state.attachment_handles_by_session.remove(&self.session_id);
                }
            }
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
            if let Some(message) = state.read_error.clone() {
                return Err(PtyError::Backend(message));
            }
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

        fn resize(&mut self, size: PtySize) -> PtyResult<()> {
            let Some(session_id) = &self.session_id else {
                return Ok(());
            };
            let mut state = self.state.lock().unwrap();
            let seq = next_fake_terminal_seq(&mut state, session_id);
            push_fake_terminal_journal(
                &mut state,
                session_id,
                PtyTerminalFrame::Resize {
                    terminal_seq: seq,
                    size,
                },
            );
            // fake PTY 也把 resize 放进 terminal frame 队列，确保协议层测试能覆盖
            // “resize 也是 session 级 terminal_seq 事件”这个不变量。
            apply_fake_terminal_frame_to_screen(
                &mut state,
                session_id,
                &PtyTerminalFrame::Resize {
                    terminal_seq: seq,
                    size,
                },
            );
            state
                .terminal_frames_by_session
                .entry(session_id.clone())
                .or_default()
                .push_back(PtyTerminalFrame::Resize {
                    terminal_seq: seq,
                    size,
                });
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::new(24, 80),
                process_id: Some(7),
                retained_output: Vec::new(),
            })
        }

        fn terminal_snapshot(
            &mut self,
            last_terminal_seq: Option<u64>,
        ) -> PtyResult<Vec<PtyTerminalFrame>> {
            let Some(session_id) = &self.session_id else {
                return Ok(vec![PtyTerminalFrame::Snapshot {
                    base_seq: 0,
                    size: PtySize::new(24, 80),
                    data: Vec::new(),
                }]);
            };
            let mut state = self.state.lock().unwrap();
            *state
                .terminal_snapshot_count_by_session
                .entry(session_id.clone())
                .or_default() += 1;
            let base_seq = state
                .terminal_seq_by_session
                .get(session_id)
                .copied()
                .unwrap_or(0);
            let size = state
                .terminal_size_by_session
                .get(session_id)
                .copied()
                .unwrap_or_else(|| PtySize::new(24, 80));
            if let Some(last_terminal_seq) = last_terminal_seq {
                if last_terminal_seq == base_seq {
                    return Ok(Vec::new());
                }
                if let Some(journal) = state.terminal_journal_by_session.get(session_id) {
                    let journal_base_seq = journal
                        .first()
                        .and_then(PtyTerminalFrame::terminal_seq)
                        .unwrap_or(base_seq.saturating_add(1));
                    if last_terminal_seq < base_seq
                        && last_terminal_seq.saturating_add(1) >= journal_base_seq
                    {
                        return Ok(journal
                            .iter()
                            .filter(|frame| {
                                frame
                                    .terminal_seq()
                                    .is_some_and(|seq| seq > last_terminal_seq)
                            })
                            .cloned()
                            .collect());
                    }
                }
            }
            Ok(vec![PtyTerminalFrame::Snapshot {
                base_seq,
                size,
                data: fake_terminal_snapshot_bytes(&state, session_id, size),
            }])
        }

        fn read_terminal_frame(&mut self) -> PtyResult<Option<PtyTerminalFrame>> {
            let mut state = self.state.lock().unwrap();
            if let Some(session_id) = &self.session_id {
                if let Some(frames) = state.terminal_frames_by_session.get_mut(session_id) {
                    if let Some(frame) = frames.pop_front() {
                        return Ok(Some(frame));
                    }
                }

                if let Some(outputs) = state.outputs_by_session.get_mut(session_id) {
                    if let Some(data) = pop_fake_output_queue(outputs, 16 * 1024) {
                        let seq = next_fake_terminal_seq(&mut state, session_id);
                        let frame = PtyTerminalFrame::Output {
                            terminal_seq: seq,
                            data,
                        };
                        push_fake_terminal_journal(&mut state, session_id, frame.clone());
                        apply_fake_terminal_frame_to_screen(&mut state, session_id, &frame);
                        return Ok(Some(frame));
                    }
                }
            }

            // 没有关联到具体 session 的旧测试仍走全局输出队列；为了不伪造跨
            // session 的顺序关系，这条兼容路径只使用 `terminal_seq=0`。
            Ok(
                pop_fake_output_queue(&mut state.outputs, 16 * 1024).map(|data| {
                    PtyTerminalFrame::Output {
                        terminal_seq: 0,
                        data,
                    }
                }),
            )
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
            let mut state = self.state.lock().unwrap();
            *state
                .cwd_read_count_by_session
                .entry(session_id.clone())
                .or_default() += 1;
            state.cwd_by_session.get(session_id).cloned()
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

    fn pop_fake_output_queue(outputs: &mut VecDeque<Vec<u8>>, max_bytes: usize) -> Option<Vec<u8>> {
        let Some(output) = outputs.pop_front() else {
            return None;
        };
        let read = output.len().min(max_bytes);
        if read < output.len() {
            outputs.push_front(output[read..].to_vec());
        }
        Some(output[..read].to_vec())
    }

    fn next_fake_terminal_seq(state: &mut FakePtyState, session_id: &str) -> u64 {
        let next = state
            .terminal_seq_by_session
            .entry(session_id.to_owned())
            .or_insert(0);
        *next = next.saturating_add(1);
        *next
    }

    fn push_fake_terminal_journal(
        state: &mut FakePtyState,
        session_id: &str,
        frame: PtyTerminalFrame,
    ) {
        // 中文注释：fake backend 的 journal 模拟 supervisor 的 raw event log，
        // 用于验证 protocol 层正确透传 last_terminal_seq，而不是总是拿 snapshot。
        state
            .terminal_journal_by_session
            .entry(session_id.to_owned())
            .or_default()
            .push(frame);
    }

    fn apply_fake_terminal_frame_to_screen(
        state: &mut FakePtyState,
        session_id: &str,
        frame: &PtyTerminalFrame,
    ) {
        // 中文注释：fake backend 要模拟真实 supervisor 权威快照；
        // 已经从 PTY 读出的 output/resize 必须进入 runtime screen mirror。
        match frame {
            PtyTerminalFrame::Output { data, .. } => {
                let size = state
                    .terminal_size_by_session
                    .get(session_id)
                    .copied()
                    .unwrap_or_else(|| PtySize::new(24, 80));
                state
                    .terminal_screen_by_session
                    .entry(session_id.to_owned())
                    .or_insert_with(|| TerminalScreen::new(size.rows, size.cols))
                    .apply(data);
            }
            PtyTerminalFrame::Resize { size, .. } => {
                state
                    .terminal_size_by_session
                    .insert(session_id.to_owned(), *size);
                state
                    .terminal_screen_by_session
                    .entry(session_id.to_owned())
                    .or_insert_with(|| TerminalScreen::new(size.rows, size.cols))
                    .resize(size.rows, size.cols);
            }
            PtyTerminalFrame::Snapshot {
                base_seq: _,
                size,
                data,
            } => {
                state
                    .terminal_size_by_session
                    .insert(session_id.to_owned(), *size);
                let mut screen = TerminalScreen::new(size.rows, size.cols);
                screen.apply(data);
                state
                    .terminal_screen_by_session
                    .insert(session_id.to_owned(), screen);
            }
            PtyTerminalFrame::Exit { .. } => {}
        }
    }

    fn fake_terminal_snapshot_bytes(
        state: &FakePtyState,
        session_id: &str,
        size: PtySize,
    ) -> Vec<u8> {
        state
            .terminal_screen_by_session
            .get(session_id)
            .map(TerminalScreen::snapshot_bytes)
            .unwrap_or_else(|| TerminalScreen::new(size.rows, size.cols).snapshot_bytes())
    }

    fn fake_attachment_snapshot_frame(
        state: &FakePtyState,
        session_id: &str,
        base_seq: u64,
    ) -> PtyTerminalFrame {
        let size = state
            .terminal_size_by_session
            .get(session_id)
            .copied()
            .unwrap_or_else(|| PtySize::new(24, 80));
        PtyTerminalFrame::Snapshot {
            base_seq,
            size,
            data: fake_terminal_snapshot_bytes(state, session_id, size),
        }
    }

    fn fake_attachment_attach_sync_frame(
        state: &FakePtyState,
        session_id: &str,
        bootstrap: PtyAttachmentBootstrap,
    ) -> PtyResult<Vec<u8>> {
        let size = state
            .terminal_size_by_session
            .get(session_id)
            .copied()
            .unwrap_or_else(|| PtySize::new(24, 80));
        let (base_seq, frames) =
            fake_attachment_tail_from_state(state, session_id, bootstrap.last_terminal_seq);
        encode_supervisor_terminal_server_frame(&SupervisorTerminalServerFrame::AttachSync {
            session_id: session_id.to_owned(),
            base_seq,
            snapshot: crate::pty::supervisor::SupervisorSnapshotPayload {
                size,
                process_id: Some(7),
                // 中文注释：fake backend 必须模拟生产 supervisor：terminal attach 的
                // 屏幕内容只通过 frames 传输，不能同时塞进 legacy retained_output。
                retained_output: Vec::new(),
            },
            frames,
        })
    }

    fn fake_attachment_tail_from_state(
        state: &FakePtyState,
        session_id: &str,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        let current_seq = state
            .terminal_seq_by_session
            .get(session_id)
            .copied()
            .unwrap_or(0);
        let Some(last_terminal_seq) = last_terminal_seq else {
            return (
                current_seq,
                vec![fake_attachment_snapshot_frame(
                    state,
                    session_id,
                    current_seq,
                )],
            );
        };
        if last_terminal_seq >= current_seq {
            return (current_seq, Vec::new());
        }
        let Some(journal) = state.terminal_journal_by_session.get(session_id) else {
            return (
                current_seq,
                vec![fake_attachment_snapshot_frame(
                    state,
                    session_id,
                    current_seq,
                )],
            );
        };
        let journal_base_seq = journal
            .first()
            .and_then(PtyTerminalFrame::terminal_seq)
            .unwrap_or(current_seq.saturating_add(1));
        if last_terminal_seq.saturating_add(1) < journal_base_seq {
            return (
                current_seq,
                vec![fake_attachment_snapshot_frame(
                    state,
                    session_id,
                    current_seq,
                )],
            );
        }
        let frames = journal
            .iter()
            .filter(|frame| {
                frame
                    .terminal_seq()
                    .is_some_and(|seq| seq > last_terminal_seq)
            })
            .cloned()
            .collect::<Vec<_>>();
        if frames
            .iter()
            .any(|frame| matches!(frame, PtyTerminalFrame::Resize { .. }))
        {
            return (
                current_seq,
                vec![fake_attachment_snapshot_frame(
                    state,
                    session_id,
                    current_seq,
                )],
            );
        }
        (current_seq, frames)
    }

    fn broadcast_fake_attachment_frame(
        state: &mut FakePtyState,
        session_id: &str,
        frame: PtyTerminalFrame,
    ) {
        let Some(handles) = state.attachment_handles_by_session.get(session_id).cloned() else {
            return;
        };
        let Ok(encoded) = encode_supervisor_terminal_server_frame(
            &SupervisorTerminalServerFrame::TerminalFrame {
                session_id: session_id.to_owned(),
                frame,
            },
        ) else {
            return;
        };

        for handle in handles {
            handle
                .pending_frames
                .lock()
                .unwrap()
                .push_back(encoded.clone());
            let next = handle.output_signal_tx.borrow().wrapping_add(1);
            let _ = handle.output_signal_tx.send(next);
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

    fn protocol_from_state(
        state: crate::state::DaemonState,
    ) -> (
        DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        FakePtyBackend,
    ) {
        let backend = FakePtyBackend::default();
        let config =
            DaemonConfig::default_for_state_path(temp_state_path("protocol-restored.json"));
        (
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap(),
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

    fn run_test_git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_test_git_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
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

    fn create_test_packet_session(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
    ) -> SessionId {
        let responses = send_encrypted_packet(
            protocol,
            connection,
            device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created = decrypt_first_packet(device_session, responses);
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_keypair.public_key_wire(),
                nonce(),
                UnixTimestampMillis(1_000),
            ),
        )
        .unwrap();

        let responses = connection.handle_wire_envelope(protocol, handshake);
        assert!(responses.is_empty());

        (device_keypair, device_session)
    }

    fn open_auth_e2ee(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
    ) -> (E2eeSession, AuthChallengePayload) {
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_e2ee_keypair.public_key_wire(),
                nonce(),
                current_unix_timestamp_millis(),
            ),
        )
        .unwrap();
        let challenge_response = connection.handle_wire_envelope(protocol, handshake);
        let challenge_envelope = decrypt_first(&mut device_session, challenge_response);

        assert_eq!(challenge_envelope.kind, MessageType::AuthChallenge);
        let challenge: AuthChallengePayload = decode_payload(challenge_envelope.payload).unwrap();
        (device_session, challenge)
    }

    fn open_packet_e2ee(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
    ) -> (E2eeKeyPair, E2eeSession, Vec<JsonEnvelope>) {
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_keypair.public_key_wire(),
                nonce(),
                UnixTimestampMillis(1_000),
            )
            .with_packet_version(ProtocolVersion(PROTOCOL_PACKET_VERSION)),
        )
        .unwrap();

        let responses = connection.handle_wire_envelope(protocol, handshake);
        assert!(connection.packet_mode);

        (device_keypair, device_session, responses)
    }

    fn open_binary_packet_e2ee(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_id: DeviceId,
    ) -> (E2eeKeyPair, E2eeSession, Vec<ProtocolWireMessage>) {
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_keypair.public_key_wire(),
                nonce(),
                UnixTimestampMillis(1_000),
            )
            .with_packet_version(ProtocolVersion(PROTOCOL_PACKET_VERSION))
            .with_binary_version(ProtocolVersion(BINARY_PROTOCOL_VERSION)),
        )
        .unwrap();

        let responses =
            connection.handle_wire_message(protocol, ProtocolWireMessage::Json(handshake));
        assert!(connection.debug_snapshot().binary_mode);

        (device_keypair, device_session, responses)
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

    fn send_encrypted_packet(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
        packet: ProtocolPacket<Value>,
    ) -> Vec<JsonEnvelope> {
        send_encrypted(
            protocol,
            connection,
            device_session,
            Envelope::new(
                MessageType::Packet,
                serde_json::to_value(packet).expect("packet should serialize"),
            ),
        )
    }

    fn send_binary_packet(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
        device_session: &mut E2eeSession,
        packet: ProtocolPacket<Value>,
    ) -> Vec<ProtocolWireMessage> {
        let binary = protocol_packet_to_binary(packet).unwrap();
        let plaintext = encode_binary_protocol_packet(&binary);
        let frame = device_session.encrypt_binary_payload(&plaintext).unwrap();
        connection.handle_wire_message(
            protocol,
            ProtocolWireMessage::Binary(encode_binary_encrypted_frame(&frame)),
        )
    }

    fn decrypt_first(
        device_session: &mut E2eeSession,
        messages: Vec<JsonEnvelope>,
    ) -> JsonEnvelope {
        decrypt_first_and_drain_scope_grants(device_session, messages)
    }

    fn decrypt_first_and_drain_scope_grants(
        device_session: &mut E2eeSession,
        messages: Vec<JsonEnvelope>,
    ) -> JsonEnvelope {
        let mut iter = messages.into_iter();
        let first = iter
            .next()
            .expect("expected at least one encrypted response");
        let first_frame = encrypted_frame_from_envelope(first).unwrap();
        let first_envelope = device_session.decrypt_json_payload(&first_frame).unwrap();

        for trailing in iter {
            let trailing_frame = encrypted_frame_from_envelope(trailing).unwrap();
            let trailing_envelope: JsonEnvelope = device_session
                .decrypt_json_payload(&trailing_frame)
                .unwrap();
            assert_eq!(
                trailing_envelope.kind,
                MessageType::SessionScopeGrant,
                "create/attach tests must consume only trailing scope grants, got {trailing_envelope:?}"
            );
        }

        first_envelope
    }

    fn decrypt_first_packet(
        device_session: &mut E2eeSession,
        messages: Vec<JsonEnvelope>,
    ) -> ProtocolPacket<Value> {
        let packets = decrypt_packets(device_session, messages);
        let mut iter = packets.into_iter();
        let first = iter.next().expect("expected at least one packet");
        assert!(
            first.method.as_deref() != Some(METHOD_SESSION_SCOPE_TOKEN),
            "packet tests must receive the business response before any scope token, got {first:?}"
        );
        for trailing in iter {
            assert!(
                trailing.kind == PacketKind::Event
                    && trailing.method.as_deref() == Some(METHOD_SESSION_SCOPE_TOKEN),
                "packet tests must not leave non-scope trailing packets unread, got {trailing:?}"
            );
        }
        first
    }

    fn decrypt_packets(
        device_session: &mut E2eeSession,
        messages: Vec<JsonEnvelope>,
    ) -> Vec<ProtocolPacket<Value>> {
        messages
            .into_iter()
            .map(|message| {
                let frame = encrypted_frame_from_envelope(message).unwrap();
                let envelope: JsonEnvelope = device_session.decrypt_json_payload(&frame).unwrap();
                assert_eq!(envelope.kind, MessageType::Packet);
                decode_payload(envelope.payload).unwrap()
            })
            .collect()
    }

    fn decrypt_binary_packets(
        device_session: &mut E2eeSession,
        messages: Vec<ProtocolWireMessage>,
    ) -> Vec<(BinaryProtocolPacket, ProtocolPacket<Value>, Vec<u8>)> {
        messages
            .into_iter()
            .map(|message| match message {
                ProtocolWireMessage::Binary(raw) => {
                    let frame = decode_binary_encrypted_frame(&raw).unwrap();
                    let plaintext = device_session.decrypt_binary_payload(&frame).unwrap();
                    let binary = decode_binary_protocol_packet(&plaintext).unwrap();
                    let packet = protocol_packet_from_binary(binary.clone()).unwrap();
                    (binary, packet, plaintext)
                }
                ProtocolWireMessage::Json(other) => {
                    panic!("expected binary response, got {other:?}")
                }
            })
            .collect()
    }

    fn decode_attach_frame_payload(packet: &ProtocolPacket<Value>) -> AttachFramePayload {
        decode_payload(packet.payload.clone()).expect("packet payload should be attach_frame")
    }

    fn decode_supervisor_frames(
        packets: &[ProtocolPacket<Value>],
    ) -> Vec<SupervisorTerminalServerFrame> {
        packets
            .iter()
            .map(|packet| {
                let payload = decode_attach_frame_payload(packet);
                decode_supervisor_terminal_server_frame(
                    &general_purpose::STANDARD
                        .decode(payload.data_base64)
                        .expect("attach frame should carry base64 data"),
                )
                .expect("attach frame should decode as supervisor terminal frame")
            })
            .collect()
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

    fn pair_packet_device(
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
        let responses = send_encrypted_packet(
            protocol,
            connection,
            device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: public_key,
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let response = decrypt_first_packet(device_session, responses);
        let accepted: PairAcceptPayload = decode_payload(response.payload).unwrap();

        assert_eq!(response.kind, PacketKind::Response);
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_e2ee_keypair.public_key_wire(),
                nonce(),
                current_unix_timestamp_millis(),
            ),
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

    fn signed_http_e2ee_auth(
        protocol: &DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        device_id: DeviceId,
        signing_key: &SigningKey,
        request_nonce: Nonce,
        method: &str,
        path: &str,
    ) -> HttpE2eeAuthPayload {
        let http_keypair = E2eeKeyPair::generate();
        let mut auth = HttpE2eeAuthPayload {
            device_id,
            e2ee_public_key: http_keypair.public_key_wire(),
            nonce: request_nonce,
            timestamp_ms: current_unix_timestamp_millis(),
            method: method.to_owned(),
            path: path.to_owned(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input =
            HttpE2eeSigningInput::from_payload(&auth, protocol.daemon_public_identity()).to_bytes();
        auth.signature = Signature(wire(&signing_key.sign(&signing_input).to_bytes()));
        auth
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
    fn packet_pair_and_session_list_use_request_response_ids() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_request_id = PacketRequestId::new();

        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                pair_request_id,
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let pair_response = decrypt_first_packet(&mut device_session, pair_responses);
        assert_eq!(pair_response.kind, PacketKind::Response);
        assert_eq!(pair_response.id, Some(pair_request_id));
        assert_eq!(pair_response.method.as_deref(), Some(METHOD_PAIR_REQUEST));
        let accepted: PairAcceptPayload = decode_payload(pair_response.payload).unwrap();
        assert_eq!(accepted.device_id, device_id);

        let list_request_id = PacketRequestId::new();
        let list_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(list_request_id, METHOD_SESSION_LIST, serde_json::json!({})),
        );
        let list_response = decrypt_first_packet(&mut device_session, list_responses);
        assert_eq!(list_response.kind, PacketKind::Response);
        assert_eq!(list_response.id, Some(list_request_id));
        assert_eq!(list_response.method.as_deref(), Some(METHOD_SESSION_LIST));
        let list: SessionListResultPayload = decode_payload(list_response.payload).unwrap();
        assert!(list.sessions.is_empty());
    }

    #[test]
    fn debug_traffic_counts_legacy_inner_messages_without_payloads() {
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

        let traffic = connection.take_debug_traffic();
        assert_eq!(
            traffic.inbound_legacy_messages.get("PairRequest").copied(),
            Some(1)
        );
        assert_eq!(
            traffic.outbound_legacy_messages.get("PairAccept").copied(),
            Some(1)
        );
        assert_eq!(traffic.packet_count(), 2);
        assert_eq!(
            connection.take_debug_traffic(),
            ProtocolConnectionDebugTraffic::default()
        );
    }

    #[test]
    fn debug_traffic_counts_packet_flow_and_terminal_output() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();

        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);
        let pair_traffic = connection.take_debug_traffic();
        assert_eq!(
            pair_traffic
                .inbound_requests
                .get(METHOD_PAIR_REQUEST)
                .copied(),
            Some(1)
        );
        assert_eq!(
            pair_traffic
                .outbound_responses
                .get(METHOD_PAIR_REQUEST)
                .copied(),
            Some(1)
        );

        let stream_id = PacketStreamId::new();
        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        let create_traffic = connection.take_debug_traffic();
        assert_eq!(
            create_traffic
                .inbound_stream_opens
                .get(METHOD_TERMINAL_CREATE)
                .copied(),
            Some(1)
        );
        assert_eq!(
            create_traffic
                .outbound_responses
                .get(METHOD_TERMINAL_CREATE)
                .copied(),
            Some(1)
        );
        assert_eq!(create_traffic.outbound_stream_chunks, 0);

        let attach_sync_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(attach_sync_packets.len(), 1);
        let attach_sync_traffic = connection.take_debug_traffic();
        assert_eq!(attach_sync_traffic.outbound_stream_chunks, 1);

        backend.push_output_for_session(created.session_id, b"hello");
        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let frames = decode_supervisor_frames(&output_packets);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SupervisorTerminalServerFrame::TerminalFrame { session_id, frame } => {
                assert_eq!(*session_id, created.session_id.0.to_string());
                match frame {
                    PtyTerminalFrame::Output { terminal_seq, data } => {
                        assert_eq!(*terminal_seq, 1);
                        assert_eq!(data, b"hello");
                    }
                    other => panic!("expected output terminal frame, got {other:?}"),
                }
            }
            other => panic!("expected terminal_frame, got {other:?}"),
        }
        let output_traffic = connection.take_debug_traffic();
        assert_eq!(output_traffic.outbound_stream_chunks, 1);
        assert_eq!(output_traffic.outbound_terminal_frame_chunks, 0);
        assert_eq!(output_traffic.outbound_terminal_frame_count, 0);

        let _ = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::flow(stream_id, 1, 1024),
        );
        let flow_traffic = connection.take_debug_traffic();
        assert_eq!(flow_traffic.inbound_flow_packets, 1);
        assert_eq!(flow_traffic.inbound_flow_credit, 1024);
    }

    #[test]
    fn packet_terminal_stream_open_polls_snapshot_with_stream_sequence() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);
        let stream_id = PacketStreamId::new();
        let create_request_id = PacketRequestId::new();

        let created_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                create_request_id,
                stream_id,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, created_responses);
        assert_eq!(created_packet.kind, PacketKind::Response);
        assert_eq!(created_packet.stream_id, Some(stream_id));
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        let visible_record = protocol
            .client_history
            .session_record_including_closed(created.session_id)
            .unwrap()
            .expect("created session should be present in daemon history");
        assert_eq!(visible_record.state, SessionState::Running);

        let attach_sync_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(attach_sync_packets.len(), 1);
        assert_eq!(attach_sync_packets[0].kind, PacketKind::StreamChunk);
        assert_eq!(attach_sync_packets[0].stream_id, Some(stream_id));
        assert_eq!(attach_sync_packets[0].seq, 1);
        let attach_sync_frames = decode_supervisor_frames(&attach_sync_packets);
        assert_eq!(attach_sync_frames.len(), 1);
        match &attach_sync_frames[0] {
            SupervisorTerminalServerFrame::AttachSync {
                session_id,
                base_seq,
                snapshot,
                frames,
            } => {
                assert_eq!(session_id, &created.session_id.0.to_string());
                assert_eq!(*base_seq, 0);
                assert_eq!(snapshot.size, PtySize::new(24, 80));
                assert!(snapshot.retained_output.is_empty());
                assert!(
                    matches!(
                        frames.as_slice(),
                        [PtyTerminalFrame::Snapshot {
                            base_seq: 0,
                            size,
                            data
                        }] if *size == PtySize::new(24, 80) && data.is_empty()
                    ),
                    "full terminal.create attach must seed through a single snapshot frame"
                );
            }
            other => panic!("expected attach_sync, got {other:?}"),
        }

        backend.push_output_for_session(created.session_id, b"hello");
        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let output_packet = &output_packets[0];
        assert_eq!(output_packet.kind, PacketKind::StreamChunk);
        assert_eq!(output_packet.stream_id, Some(stream_id));
        assert_eq!(output_packet.seq, 2);
        let frames = decode_supervisor_frames(&output_packets);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SupervisorTerminalServerFrame::TerminalFrame { session_id, frame } => {
                assert_eq!(session_id, &created.session_id.0.to_string());
                match frame {
                    PtyTerminalFrame::Output { terminal_seq, data } => {
                        assert_eq!(*terminal_seq, 1);
                        assert_eq!(data, b"hello");
                    }
                    other => panic!("expected output terminal frame, got {other:?}"),
                }
            }
            other => panic!("expected terminal_frame, got {other:?}"),
        }
    }

    #[test]
    fn binary_packet_terminal_stream_uses_raw_bytes_without_data_base64_wire() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_binary_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_binary_packets(&mut device_session, pair_responses);

        let stream_id = PacketStreamId::new();
        let created_responses = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packets = decrypt_binary_packets(&mut device_session, created_responses);
        let created: SessionCreatedPayload =
            decode_payload(created_packets[0].1.payload.clone()).unwrap();

        let attach_sync_packets = decrypt_binary_packets(
            &mut device_session,
            connection.read_session_output_wire(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(attach_sync_packets.len(), 1);
        let attach_sync_frame = match attach_sync_packets[0].0.payload.clone() {
            Some(binary_protocol_packet::Payload::AttachFrame(payload)) => {
                decode_supervisor_terminal_server_frame(&payload.data).unwrap()
            }
            other => panic!("expected binary attach frame payload, got {other:?}"),
        };
        assert!(matches!(
            attach_sync_frame,
            SupervisorTerminalServerFrame::AttachSync { .. }
        ));

        let input_frame =
            encode_supervisor_terminal_client_frame(&SupervisorTerminalClientFrame::Input {
                data: b"stream-input".to_vec(),
            })
            .unwrap();
        let input_packet = ProtocolPacket::stream_chunk(
            stream_id,
            1,
            attach_frame_payload_value(AttachFramePayload {
                session_id: created.session_id,
                data_base64: general_purpose::STANDARD.encode(&input_frame),
            })
            .unwrap(),
        );
        let input_binary = protocol_packet_to_binary(input_packet.clone()).unwrap();
        let input_plaintext = encode_binary_protocol_packet(&input_binary);
        assert!(!String::from_utf8_lossy(&input_plaintext).contains("data_base64"));
        match input_binary.payload.clone() {
            Some(binary_protocol_packet::Payload::AttachFrame(payload)) => {
                assert_eq!(payload.session_id, created.session_id.0.as_bytes().to_vec());
                assert_eq!(payload.data, input_frame);
            }
            other => panic!("expected binary attach frame payload, got {other:?}"),
        }
        let _ = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            input_packet,
        );
        let attachment_writes = backend.attachment_writes_for_session(created.session_id);
        assert_eq!(attachment_writes.len(), 1);
        let decoded_input = decode_supervisor_terminal_client_frame(&attachment_writes[0]).unwrap();
        match decoded_input {
            SupervisorTerminalClientFrame::Input { data } => {
                assert_eq!(data, b"stream-input");
            }
            other => panic!("expected input supervisor frame, got {other:?}"),
        }

        backend.push_output_for_session(created.session_id, b"hello");
        let output_messages =
            connection.read_session_output_wire(&mut protocol, created.session_id, 1024);
        let output_packets = decrypt_binary_packets(&mut device_session, output_messages);
        assert!(!String::from_utf8_lossy(&output_packets[0].2).contains("data_base64"));
        match output_packets[0].0.payload.clone() {
            Some(binary_protocol_packet::Payload::AttachFrame(payload)) => {
                let frame = decode_supervisor_terminal_server_frame(&payload.data).unwrap();
                match frame {
                    SupervisorTerminalServerFrame::TerminalFrame { frame, .. } => match frame {
                        PtyTerminalFrame::Output { terminal_seq, data } => {
                            assert_eq!(terminal_seq, 1);
                            assert_eq!(data, b"hello");
                        }
                        other => panic!("expected output terminal frame, got {other:?}"),
                    },
                    other => panic!("expected supervisor terminal_frame, got {other:?}"),
                }
            }
            other => panic!("expected binary attach frame payload, got {other:?}"),
        }
    }

    #[test]
    fn binary_packet_terminal_frame_output_keeps_terminal_metadata() {
        let session_id = SessionId::new();
        let stream_id = PacketStreamId::new();
        let frame = TerminalFramePayload::Output {
            session_id,
            terminal_seq: 9,
            data_base64: general_purpose::STANDARD.encode(b"single-frame-output"),
        };
        let packet = ProtocolPacket::stream_chunk(
            stream_id,
            3,
            serde_json::to_value(frame.clone()).unwrap(),
        );

        let binary = protocol_packet_to_binary(packet).unwrap();
        let Some(binary_protocol_packet::Payload::TerminalFrame(binary_frame)) =
            binary.payload.clone()
        else {
            panic!("terminal frame must not be encoded as session_data");
        };

        assert_eq!(binary_frame.kind, BinaryTerminalFrameKind::Output as i32);
        assert_eq!(binary_frame.terminal_seq, 9);
        assert_eq!(binary_frame.data, b"single-frame-output");

        let roundtrip = protocol_packet_from_binary(binary).unwrap();
        let decoded: TerminalFramePayload = decode_payload(roundtrip.payload).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn binary_packet_terminal_frame_snapshot_encodes_nonzero_kind() {
        let session_id = SessionId::new();
        let stream_id = PacketStreamId::new();
        let frame = TerminalFramePayload::Snapshot {
            session_id,
            base_seq: 7,
            size: TerminalSize::new(24, 80),
            data_base64: general_purpose::STANDARD.encode(b"snapshot"),
        };
        let packet = ProtocolPacket::stream_chunk(
            stream_id,
            3,
            serde_json::to_value(frame.clone()).unwrap(),
        );

        let binary = protocol_packet_to_binary(packet).unwrap();
        let Some(binary_protocol_packet::Payload::TerminalFrame(binary_frame)) =
            binary.payload.clone()
        else {
            panic!("snapshot must be encoded as a terminal frame");
        };

        assert_eq!(BinaryTerminalFrameKind::Snapshot as i32, 1);
        assert_eq!(binary_frame.kind, 1);

        let roundtrip = protocol_packet_from_binary(binary).unwrap();
        let decoded: TerminalFramePayload = decode_payload(roundtrip.payload).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn binary_packet_terminal_frame_snapshot_accepts_legacy_default_kind() {
        let session_id = SessionId::new();
        let stream_id = PacketStreamId::new();
        let binary = BinaryProtocolPacket {
            version: u32::from(PROTOCOL_PACKET_VERSION),
            kind: BinaryPacketKind::StreamChunk as i32,
            id: Vec::new(),
            stream_id: stream_id.0.as_bytes().to_vec(),
            method: String::new(),
            seq: 3,
            ack: 0,
            credit: 0,
            payload: Some(binary_protocol_packet::Payload::TerminalFrame(
                BinaryTerminalFramePayload {
                    kind: BinaryTerminalFrameKind::Unspecified as i32,
                    session_id: session_id.0.as_bytes().to_vec(),
                    base_seq: 7,
                    terminal_seq: 0,
                    size: Some(BinaryTerminalSize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    }),
                    data: b"legacy-snapshot".to_vec(),
                    frames: Vec::new(),
                    exit_code: None,
                },
            )),
        };

        let roundtrip = protocol_packet_from_binary(binary).unwrap();
        let decoded: TerminalFramePayload = decode_payload(roundtrip.payload).unwrap();

        assert_eq!(
            decoded,
            TerminalFramePayload::Snapshot {
                session_id,
                base_seq: 7,
                size: TerminalSize::new(24, 80),
                data_base64: general_purpose::STANDARD.encode(b"legacy-snapshot"),
            }
        );
    }

    #[test]
    fn packet_terminal_attach_uses_last_terminal_seq_for_tail() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        backend.push_terminal_journal_frame_for_session(
            created.session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"already-rendered".to_vec(),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            created.session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"tail-only".to_vec(),
            },
        );

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(1),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);
        assert_eq!(attached_packet.stream_id, Some(stream_id));

        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let frames = decode_supervisor_frames(&output_packets);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SupervisorTerminalServerFrame::AttachSync {
                session_id,
                base_seq,
                snapshot,
                frames,
            } => {
                assert_eq!(session_id, &created.session_id.0.to_string());
                assert_eq!(*base_seq, 2);
                assert!(snapshot.retained_output.is_empty());
                assert_eq!(frames.len(), 1);
                match &frames[0] {
                    PtyTerminalFrame::Output { terminal_seq, data } => {
                        assert_eq!(*terminal_seq, 2);
                        assert_eq!(data, b"tail-only");
                    }
                    other => panic!("expected attach tail output frame, got {other:?}"),
                }
            }
            other => panic!("expected attach_sync tail, got {other:?}"),
        }
    }

    #[test]
    fn packet_terminal_attach_rebases_to_snapshot_when_tail_crosses_resize() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"before-resize".to_vec(),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Resize {
                terminal_seq: 2,
                size: PtySize::new(40, 120),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 3,
                data: b"after-resize".to_vec(),
            },
        );

        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(1),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, attach_responses);

        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let frames = decode_supervisor_frames(&output_packets);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SupervisorTerminalServerFrame::AttachSync {
                session_id: snapshot_session_id,
                base_seq,
                snapshot,
                frames,
            } => {
                assert_eq!(snapshot_session_id, &session_id.0.to_string());
                assert_eq!(*base_seq, 3);
                assert_eq!(snapshot.size, PtySize::new(40, 120));
                assert!(snapshot.retained_output.is_empty());
                assert!(
                    matches!(
                        frames.as_slice(),
                        [PtyTerminalFrame::Snapshot {
                            base_seq: 3,
                            size,
                            data,
                        }] if *size == PtySize::new(40, 120)
                            && data.windows(b"after-resize".len()).any(|window| window == b"after-resize")
                    ),
                    "resize-crossing tail must seed through a snapshot frame"
                );
            }
            other => {
                panic!("resize-crossing tail must rebase to attach_sync snapshot, got {other:?}")
            }
        }
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_snapshot_attach_advances_connection_cursor() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"old-1".to_vec(),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"old-2".to_vec(),
            },
        );

        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, attach_responses);
        let snapshot_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(snapshot_packets.len(), 1);
        let snapshot: TerminalFramePayload =
            decode_payload(snapshot_packets[0].payload.clone()).unwrap();
        assert!(matches!(
            snapshot,
            TerminalFramePayload::Snapshot { base_seq: 2, .. }
        ));

        backend.push_output_for_session(session_id, b"fresh-3".to_vec());
        let live_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(live_packets.len(), 1);
        let live: TerminalFramePayload = decode_payload(live_packets[0].payload.clone()).unwrap();
        match live {
            TerminalFramePayload::Output {
                terminal_seq,
                data_base64,
                ..
            } => {
                assert_eq!(terminal_seq, 3);
                assert_eq!(
                    general_purpose::STANDARD.decode(data_base64).unwrap(),
                    b"fresh-3"
                );
            }
            other => panic!("expected only fresh live output after snapshot, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_live_output_rebases_to_snapshot_when_poll_crosses_resize() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);

        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, attach_responses);
        let initial = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(initial.len(), 1);
        let initial_frame: TerminalFramePayload =
            decode_payload(initial[0].payload.clone()).unwrap();
        assert!(matches!(
            initial_frame,
            TerminalFramePayload::Snapshot { base_seq: 0, .. }
        ));

        let resize_size = TerminalSize::new(40, 120);
        let resize_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_RESIZE,
                serde_json::to_value(SessionResizePayload {
                    session_id,
                    size: resize_size,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, resize_responses);
        backend.push_output_for_session(session_id, b"after-resize".to_vec());

        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let snapshot: TerminalFramePayload =
            decode_payload(output_packets[0].payload.clone()).unwrap();
        match snapshot {
            TerminalFramePayload::Snapshot { base_seq, size, .. } => {
                assert_eq!(base_seq, 2);
                assert_eq!(size, resize_size);
            }
            other => panic!("live resize-crossing drain must return snapshot, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_empty_tail_attach_advances_connection_cursor() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"old-1".to_vec(),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"old-2".to_vec(),
            },
        );

        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(2),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, attach_responses);

        assert!(
            connection
                .read_session_output(&mut protocol, session_id, 1024)
                .is_empty(),
            "已追平 attach 不应重放 seq<=last_terminal_seq 的旧 frame"
        );
        backend.push_output_for_session(session_id, b"fresh-3".to_vec());
        let live_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(live_packets.len(), 1);
        let live: TerminalFramePayload = decode_payload(live_packets[0].payload.clone()).unwrap();
        assert_eq!(live.terminal_seq(), Some(3));
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_empty_tail_from_daemon_cache_advances_connection_cursor() {
        let (mut protocol, backend) = protocol();
        let device_id = DeviceId::new();
        let (mut first_connection, _) = protocol.start_connection();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        backend.push_output_for_session(session_id, b"cached-1".to_vec());
        backend.push_output_for_session(session_id, b"cached-2".to_vec());

        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, first_attach);
        let first_packets = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert!(!first_packets.is_empty());

        let mut second_connection = protocol.start_connection().0;
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(2),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);

        assert!(
            second_connection
                .read_session_output(&mut protocol, session_id, 1024)
                .is_empty(),
            "daemon cache 命中的空 tail attach 不应重放旧 frame"
        );
        backend.push_output_for_session(session_id, b"fresh-3".to_vec());
        let second_packets = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(second_packets.len(), 1);
        let live: TerminalFramePayload = decode_payload(second_packets[0].payload.clone()).unwrap();
        assert_eq!(live.terminal_seq(), Some(3));
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_daemon_cache_gap_falls_back_to_runtime_snapshot() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"first\n".to_vec(),
            },
        );
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 4,
                data: b"after-gap\n".to_vec(),
            },
        );

        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, first_attach);
        let _ = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        let mut second_connection = protocol.start_connection().0;
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);

        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            2,
            "daemon mirror 一旦发现 terminal_seq gap，后续 attach 必须回源 supervisor，而不能用不完整本地 screen 生成 snapshot"
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_full_snapshot_poll_uses_daemon_mirror_when_log_is_continuous() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        backend.push_output_for_session(session_id, b"alpha\n".to_vec());
        backend.push_output_for_session(session_id, b"beta\n".to_vec());

        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, first_attach);
        let first_packets = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert!(!first_packets.is_empty());
        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            0,
            "连续 tail 仍应使用 daemon live log，避免普通 reattach 频繁回源权威快照"
        );

        let mut second_connection = protocol.start_connection().0;
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);
        let second_packets = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(second_packets.len(), 1);
        let snapshot: TerminalFramePayload =
            decode_payload(second_packets[0].payload.clone()).unwrap();
        assert!(matches!(snapshot, TerminalFramePayload::Snapshot { .. }));
        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            0,
            "last_terminal_seq=None 仍然表示 full snapshot，但当 daemon mirror 已连续且权威时，应直接用 mirror 生成完整当前画面"
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_full_snapshot_after_resize_reflows_live_bootstrap_history_without_runtime_snapshot()
     {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        let long_line =
            b"1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdefghijklmnopqrstuvwxyz\r\n";
        backend.push_output_for_session(session_id, long_line.to_vec());

        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, first_attach);
        let _ = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        let resize_responses = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_RESIZE,
                serde_json::to_value(SessionResizePayload {
                    session_id,
                    size: TerminalSize::new(24, 120),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, resize_responses);
        let _ = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        let (mut second_connection, _) = protocol.start_connection();
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);
        let second_packets = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(second_packets.len(), 1);
        let snapshot: TerminalFramePayload =
            decode_payload(second_packets[0].payload.clone()).unwrap();
        let TerminalFramePayload::Snapshot { data_base64, .. } = snapshot else {
            panic!("expected terminal snapshot after resize reflow");
        };
        let snapshot_text =
            String::from_utf8(general_purpose::STANDARD.decode(data_base64).unwrap()).unwrap();
        assert!(
            snapshot_text.contains(
                "1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdefghijklmnopqrstuvwxyz"
            ),
            "resize 后 full snapshot 应该按新列宽重放 live output，而不是保留旧换行: {snapshot_text:?}"
        );
        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            0,
            "live bootstrap session 在 resize 后的 full snapshot 应直接复用 daemon 原始输出重建 screen，避免再次回源 runtime/supervisor"
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_full_snapshot_after_resize_for_live_bootstrap_alt_screen_waits_for_runtime_snapshot()
     {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        backend.push_output_for_session(
            session_id,
            b"shell before\r\n\x1b[?1049h\x1b[2J\x1b[Hcodex alt view".to_vec(),
        );

        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, first_attach);
        let _ = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        let resize_responses = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_RESIZE,
                serde_json::to_value(SessionResizePayload {
                    session_id,
                    size: TerminalSize::new(24, 120),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, resize_responses);
        let _ = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        let (mut second_connection, _) = protocol.start_connection();
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);
        let second_packets = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );

        assert_eq!(second_packets.len(), 1);
        let snapshot: TerminalFramePayload =
            decode_payload(second_packets[0].payload.clone()).unwrap();
        assert!(
            matches!(snapshot, TerminalFramePayload::Snapshot { .. }),
            "alternate screen reopen 应返回 full snapshot"
        );
        assert!(
            backend.terminal_snapshot_count_for_session(session_id) > 0,
            "alternate screen 在 resize 后未 redraw 前，full snapshot 仍应回源 runtime，不能直接把旧尺寸原始输出按新列宽重放"
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_reattach_uses_daemon_terminal_log_without_runtime_snapshot() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        backend.push_output_for_session(session_id, b"alpha\n".to_vec());
        backend.push_output_for_session(session_id, b"beta\n".to_vec());

        let first_stream = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                first_stream,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut first_device_session, attach_responses);
        let first_packets = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert!(!first_packets.is_empty());
        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            0,
            "last_terminal_seq=0 且 live tail 连续时，第一次 drain 应直接使用 daemon log tail，不请求 supervisor snapshot"
        );

        let second_device_id = DeviceId::new();
        let (mut second_connection, _) = protocol.start_connection();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );
        let second_stream = PacketStreamId::new();
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                second_stream,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(1),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut second_device_session, second_attach);

        assert_eq!(
            backend.terminal_snapshot_count_for_session(session_id),
            0,
            "daemon 已有 terminal mirror/log 后，新 browser attach 不应再请求 supervisor snapshot"
        );
        let second_packets = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(second_packets.len(), 1);
        let tail: TerminalFramePayload = decode_payload(second_packets[0].payload.clone()).unwrap();
        assert_eq!(tail.terminal_seq(), Some(2));
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_output_batches_frames_by_output_bytes_without_credit() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        for (terminal_seq, data) in [
            (1, b"abcdefghij".to_vec()),
            (2, b"klmnopqrst".to_vec()),
            (3, b"uvwxyz1234".to_vec()),
        ] {
            backend.push_terminal_journal_frame_for_session(
                created.session_id,
                PtyTerminalFrame::Output { terminal_seq, data },
            );
        }

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                0,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);

        let first_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(first_packets.len(), 1);
        assert_eq!(first_packets[0].kind, PacketKind::StreamChunk);
        assert_eq!(first_packets[0].seq, 1);
        let first_batch: TerminalFramePayload =
            decode_payload(first_packets[0].payload.clone()).unwrap();
        match first_batch {
            TerminalFramePayload::Batch { session_id, frames } => {
                assert_eq!(session_id, created.session_id);
                assert_eq!(frames.len(), 3);
                assert_eq!(frames[0].terminal_seq(), Some(1));
                assert_eq!(frames[1].terminal_seq(), Some(2));
                assert_eq!(frames[2].terminal_seq(), Some(3));
            }
            other => panic!("expected first byte-budgeted terminal batch, got {other:?}"),
        }

        let drained = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert!(
            drained.is_empty(),
            "all frames should already be sent without waiting for render credit: {drained:?}"
        );

        let flow_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::flow(stream_id, 1, 10),
        );
        assert!(
            flow_responses.is_empty(),
            "legacy flow packets are accepted as no-op compatibility frames"
        );
        assert!(connection.take_deferred_output_wakeups().is_empty());
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_output_does_not_wait_for_stream_credit() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"zero-credit-still-streams".to_vec(),
            },
        );

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                0,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );

        // 中文注释：新模型不再把 browser render ACK / credit 作为终端输出闸门。
        // 即使旧客户端没有发送初始 credit，daemon 也必须按 WebSocket/TCP 背压直接推流。
        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        assert_eq!(output_packets[0].kind, PacketKind::StreamChunk);
        assert_eq!(output_packets[0].stream_id, Some(stream_id));
        let frame: TerminalFramePayload =
            decode_payload(output_packets[0].payload.clone()).unwrap();
        assert_eq!(frame.terminal_seq(), Some(1));
        assert_eq!(connection.debug_snapshot().zero_credit_terminal_streams, 0);
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_attach_is_drained_without_pending_frame_queue() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        backend.push_terminal_journal_frame_for_session(
            session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"attach-wakeup-output".to_vec(),
            },
        );

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                0,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );

        // 中文注释：packet terminal attach 只建立 stream 与 drain cursor，不再把
        // snapshot/tail 放进 per-client pending 队列；server/relay 后续通过一次
        // output wakeup 调用 daemon push drain 即可取到初始输出。
        assert_eq!(connection.debug_snapshot().pending_terminal_frames, 0);
        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let frame: TerminalFramePayload =
            decode_payload(output_packets[0].payload.clone()).unwrap();
        assert_eq!(frame.terminal_seq(), Some(1));
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            Vec::<SessionId>::new(),
            "attach 本身不再依赖连接内 pending 队列自唤醒"
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_live_output_bursts_are_batched_beyond_tiny_frame_count() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                0,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );
        let _ = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );

        for index in 1..=200 {
            backend.push_output_for_session(session_id, format!("live-burst-{index:04}\n"));
        }

        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let payload: TerminalFramePayload =
            decode_payload(output_packets[0].payload.clone()).unwrap();

        // 中文注释：实时输出已经在 daemon log 中形成 backlog 时，不能按 8 个小 frame
        // 慢慢推；应该尽量填满 terminal batch 的字节预算，避免 relay Web 看起来逐行蹦。
        assert!(
            terminal_frame_payload_count(&payload) > 128,
            "live burst 应按批量发送，实际只发送 {} 个 frame",
            terminal_frame_payload_count(&payload)
        );
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_output_batches_frames_by_transport_size() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                created.session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"small-output-frame\n".to_vec(),
                },
            );
        }

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                1024 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);

        let encrypted_messages =
            connection.read_session_output(&mut protocol, created.session_id, 1024);
        assert_eq!(encrypted_messages.len(), 1);
        let encrypted_wire_bytes = serde_json::to_string(&encrypted_messages[0])
            .expect("encrypted envelope should serialize")
            .len();
        assert!(
            encrypted_wire_bytes <= 1024 * 1024,
            "transport cap should keep one websocket message small, got {encrypted_wire_bytes} bytes"
        );

        let output_packets = decrypt_packets(&mut device_session, encrypted_messages);
        assert_eq!(output_packets.len(), 1);
        let payload: TerminalFramePayload =
            decode_payload(output_packets[0].payload.clone()).unwrap();
        assert!(
            terminal_frame_payload_count(&payload) > 128,
            "credit 充足时不应被小 frame 数预算限速，实际 {} 个 frame",
            terminal_frame_payload_count(&payload)
        );
        assert!(
            terminal_frame_payload_count(&payload) < 6000,
            "transport cap should leave the remaining tiny frames pending instead of one huge batch"
        );
        assert_eq!(
            connection.debug_snapshot().pending_terminal_frames,
            0,
            "bounded terminal output should leave unsent frames behind the cursor, not in a per-client queue"
        );
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            vec![created.session_id],
            "仍有待发 terminal frames 时，应由 server/relay 下一轮 push 继续发送，不能等浏览器 ACK 才推进"
        );
        let second_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(second_packets.len(), 1);
        assert_eq!(second_packets[0].seq, 2);
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_flow_is_noop_and_pending_frames_drain_on_push() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                created.session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"small-output-frame\n".to_vec(),
                },
            );
        }

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                20,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);

        let first_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(first_packets.len(), 1);
        assert_eq!(first_packets[0].seq, 1);
        assert_eq!(
            connection.debug_snapshot().pending_terminal_frames,
            0,
            "部分 frame 应留在 daemon session log 中等待下一次 push drain，而不是暂存在连接队列"
        );
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            vec![created.session_id],
            "pending frame 应由下一轮 push drain 推进，而不是等 flow ACK"
        );

        let flow_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::flow(stream_id, 1, 10),
        );
        assert_eq!(
            flow_responses.len(),
            0,
            "flow 只是旧协议兼容 no-op，不能成为输出推进条件"
        );
        assert!(
            connection.take_deferred_output_wakeups().is_empty(),
            "flow no-op 不应重新登记输出 wakeup"
        );

        let second_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(second_packets.len(), 1);
        assert_eq!(second_packets[0].kind, PacketKind::StreamChunk);
        assert_eq!(second_packets[0].seq, 2);
        let second: TerminalFramePayload =
            decode_payload(second_packets[0].payload.clone()).unwrap();
        assert!(terminal_frame_payload_count(&second) > 0);
    }

    #[test]
    fn packet_terminal_switch_clears_previous_stream_and_pending_frames() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );

        let first_session =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        let second_session =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                first_session,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"first-output-frame\n".to_vec(),
                },
            );
        }
        backend.push_terminal_journal_frame_for_session(
            second_session,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"second-ready".to_vec(),
            },
        );

        let first_stream = PacketStreamId::new();
        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                first_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id: first_session,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, first_attach).kind,
            PacketKind::Response
        );
        let first_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, first_session, 1024),
        );
        assert_eq!(first_output.len(), 1);
        assert_eq!(
            connection.debug_snapshot().pending_terminal_frames,
            0,
            "bounded batch should not leave per-client terminal frame debt"
        );

        let second_stream = PacketStreamId::new();
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                second_stream,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id: second_session,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, second_attach).kind,
            PacketKind::Response
        );
        let snapshot = connection.debug_snapshot();
        assert_eq!(snapshot.terminal_streams, 1);
        assert_eq!(snapshot.zero_credit_terminal_streams, 0);

        let old_session_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, first_session, 1024),
        );
        assert!(
            old_session_output.is_empty(),
            "切走后的旧 session 不能继续通过残留 stream/pending frames 推送输出"
        );
        let second_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, second_session, 1024),
        );
        assert_eq!(second_output.len(), 1);
        assert_eq!(second_output[0].stream_id, Some(second_stream));
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_stream_open_failure_keeps_previous_stream_and_deferred_wakeup() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );

        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"failure-safe-output-frame\n".to_vec(),
                },
            );
        }

        let old_stream = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                old_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );
        let first_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(first_output.len(), 1);
        assert_eq!(first_output[0].stream_id, Some(old_stream));
        assert_eq!(first_output[0].seq, 1);

        let malformed_open = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::json!({
                    "watch_updates": true,
                    "last_terminal_seq": 0
                }),
            ),
        );
        let malformed_response = decrypt_first_packet(&mut device_session, malformed_open);
        assert_eq!(malformed_response.kind, PacketKind::Error);
        assert_eq!(
            connection.debug_snapshot().terminal_streams,
            1,
            "malformed replacement stream-open 不能清掉旧 terminal stream"
        );
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            vec![session_id],
            "malformed replacement stream-open 不能丢失旧 stream 的 deferred wakeup"
        );

        let second_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(second_output.len(), 1);
        assert_eq!(second_output[0].kind, PacketKind::StreamChunk);
        assert_eq!(second_output[0].stream_id, Some(old_stream));
        assert_eq!(second_output[0].seq, 2);
        let second_payload: TerminalFramePayload =
            decode_payload(second_output[0].payload.clone()).unwrap();
        assert!(terminal_frame_payload_count(&second_payload) > 0);
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_stream_open_handler_failure_rolls_back_previous_stream() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );

        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"rollback-safe-output-frame\n".to_vec(),
                },
            );
        }

        let old_stream = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                old_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );
        let first_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(first_output.len(), 1);
        assert_eq!(first_output[0].stream_id, Some(old_stream));
        assert_eq!(first_output[0].seq, 1);
        assert_eq!(backend.attachment_starts().len(), 2);
        assert_eq!(backend.attachment_drops().len(), 1);

        let invalid_session_open = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                PacketStreamId::new(),
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id: SessionId::new(),
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let invalid_session_response =
            decrypt_first_packet(&mut device_session, invalid_session_open);
        assert_eq!(invalid_session_response.kind, PacketKind::Error);
        assert_eq!(
            connection.debug_snapshot().terminal_streams,
            1,
            "valid payload 进入 handler 后失败时也必须恢复旧 terminal stream"
        );
        assert_eq!(
            backend.attachment_starts().len(),
            2,
            "handler 失败不能启动新的 watched attachment"
        );
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "handler 失败不能释放仍在使用的旧 watched attachment"
        );
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            vec![session_id],
            "handler 失败回滚不能丢失旧 stream 的 deferred wakeup"
        );

        let second_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(second_output.len(), 1);
        assert_eq!(second_output[0].kind, PacketKind::StreamChunk);
        assert_eq!(second_output[0].stream_id, Some(old_stream));
        assert_eq!(second_output[0].seq, 2);
        let second_payload: TerminalFramePayload =
            decode_payload(second_output[0].payload.clone()).unwrap();
        assert!(terminal_frame_payload_count(&second_payload) > 0);
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_stream_open_late_failure_rolls_back_connection_attach_state() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );

        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"late-rollback-output-frame\n".to_vec(),
                },
            );
        }

        let old_stream = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                old_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attach_responses).kind,
            PacketKind::Response
        );
        let first_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(first_output.len(), 1);
        assert_eq!(first_output[0].stream_id, Some(old_stream));
        assert_eq!(connection.debug_snapshot().attached_sessions, 1);
        assert_eq!(backend.attachment_starts().len(), 2);
        assert_eq!(backend.attachment_drops().len(), 1);

        // 中文注释：让状态文件的父路径变成普通文件，persist_state 必然失败。
        // terminal.create 会在 connection.attach 之后写状态，用来覆盖“清旧流后晚失败”的
        // rollback 分支。
        let parent_file = temp_state_path("state-parent-file");
        std::fs::write(&parent_file, b"not-a-directory").unwrap();
        protocol.config.state_path = parent_file.join("daemon-state.json");
        let request_id = PacketRequestId::new();
        let late_failure = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                request_id,
                PacketStreamId::new(),
                METHOD_TERMINAL_CREATE,
                16,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let late_failure_packet = decrypt_first_packet(&mut device_session, late_failure);
        assert_eq!(late_failure_packet.kind, PacketKind::Error);
        assert_eq!(late_failure_packet.id, Some(request_id));
        let error: PacketErrorPayload = decode_payload(late_failure_packet.payload).unwrap();
        assert_eq!(error.code, "state_failed");
        assert_eq!(
            connection.debug_snapshot().attached_sessions,
            1,
            "terminal.create late failure 不能把失败的新 session 留在连接 attach 集合里"
        );
        assert_eq!(
            protocol.session_index.len(),
            1,
            "terminal.create late failure 不能把失败的新 session 留在 daemon session index"
        );
        assert_eq!(
            backend.terminate_count(),
            1,
            "terminal.create late failure 必须终止失败的新 runtime host"
        );
        assert_eq!(
            connection.debug_snapshot().terminal_streams,
            1,
            "terminal.create late failure 必须恢复旧 terminal stream"
        );
        assert_eq!(
            backend.attachment_starts().len(),
            2,
            "terminal.create late failure 发生在持久化阶段时不能启动新 watched attachment"
        );
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "terminal.create late failure 不能释放仍然恢复中的旧 watched attachment"
        );
        assert_eq!(connection.take_deferred_output_wakeups(), vec![session_id]);

        let second_output = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(second_output.len(), 1);
        assert_eq!(second_output[0].stream_id, Some(old_stream));
        assert_eq!(second_output[0].seq, 2);
    }

    #[test]
    fn packet_terminal_live_frames_are_fanned_out_to_multiple_connections() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let first_device_id = DeviceId::new();
        let (_, mut first_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut first_connection, first_device_id);
        pair_packet_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            first_device_id,
            PublicKey("first-device-public-key".to_owned()),
        );
        let session_id = create_test_packet_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );

        let (mut second_connection, _) = protocol.start_connection();
        let second_device_id = DeviceId::new();
        let (_, mut second_device_session, _) =
            open_packet_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_packet_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            PublicKey("second-device-public-key".to_owned()),
        );

        let first_stream = PacketStreamId::new();
        let first_attach = send_encrypted_packet(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                first_stream,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut first_device_session, first_attach).kind,
            PacketKind::Response
        );
        let first_bootstrap = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(
            first_bootstrap.len(),
            1,
            "每个 attach connection 都应先各自收到一份 attach bootstrap"
        );

        let second_stream = PacketStreamId::new();
        let second_attach = send_encrypted_packet(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                second_stream,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut second_device_session, second_attach).kind,
            PacketKind::Response
        );
        let second_bootstrap = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(
            second_bootstrap.len(),
            1,
            "第二个 attach connection 也应先收到自己的 bootstrap"
        );

        backend.push_output_for_session(session_id, b"shared-live-frame");

        let first_output = decrypt_packets(
            &mut first_device_session,
            first_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        let second_output = decrypt_packets(
            &mut second_device_session,
            second_connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(
            first_output.len(),
            1,
            "每个 attach connection 都应收到自己的 live supervisor frame"
        );
        assert_eq!(
            second_output.len(),
            1,
            "live terminal frame 不能被第一个连接从 session 队列里独占消费"
        );
        assert_eq!(first_output[0].stream_id, Some(first_stream));
        assert_eq!(second_output[0].stream_id, Some(second_stream));
        let first_frames = decode_supervisor_frames(&first_output);
        let second_frames = decode_supervisor_frames(&second_output);
        match (&first_frames[0], &second_frames[0]) {
            (
                SupervisorTerminalServerFrame::TerminalFrame {
                    frame: first_frame, ..
                },
                SupervisorTerminalServerFrame::TerminalFrame {
                    frame: second_frame,
                    ..
                },
            ) => {
                assert_eq!(first_frame.terminal_seq(), Some(1));
                assert_eq!(second_frame.terminal_seq(), Some(1));
            }
            other => panic!("expected fanned-out terminal_frame packets, got {other:?}"),
        }
    }

    #[test]
    fn packet_terminal_cancel_and_end_clear_pending_frames() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            PublicKey("device-public-key".to_owned()),
        );
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        for terminal_seq in 1..=6000 {
            backend.push_terminal_journal_frame_for_session(
                session_id,
                PtyTerminalFrame::Output {
                    terminal_seq,
                    data: b"cancel-output-frame\n".to_vec(),
                },
            );
        }

        let cancel_stream = PacketStreamId::new();
        let cancel_attach = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                cancel_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, cancel_attach).kind,
            PacketKind::Response
        );
        let _ = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(connection.debug_snapshot().pending_terminal_frames, 0);

        let cancel_result = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket {
                version: PROTOCOL_PACKET_VERSION,
                kind: PacketKind::Cancel,
                id: None,
                stream_id: Some(cancel_stream),
                method: None,
                seq: 0,
                ack: None,
                credit: None,
                payload: serde_json::json!({"reason": "switch"}),
            },
        );
        assert!(cancel_result.is_empty());
        assert_eq!(connection.debug_snapshot().terminal_streams, 0);
        assert_eq!(connection.debug_snapshot().pending_terminal_frames, 0);

        let end_stream = PacketStreamId::new();
        let end_attach = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                end_stream,
                METHOD_TERMINAL_ATTACH,
                16,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, end_attach).kind,
            PacketKind::Response
        );
        let _ = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, session_id, 1024),
        );
        assert_eq!(connection.debug_snapshot().pending_terminal_frames, 0);
        let end_result = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket {
                version: PROTOCOL_PACKET_VERSION,
                kind: PacketKind::StreamEnd,
                id: None,
                stream_id: Some(end_stream),
                method: None,
                seq: 1,
                ack: None,
                credit: None,
                payload: serde_json::json!({}),
            },
        );
        assert!(end_result.is_empty());
        assert_eq!(connection.debug_snapshot().terminal_streams, 0);
        assert_eq!(connection.debug_snapshot().pending_terminal_frames, 0);
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_output_allows_one_frame_larger_than_batch_limit() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        backend.push_terminal_journal_frame_for_session(
            created.session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: vec![b'x'; 30],
            },
        );
        backend.push_terminal_journal_frame_for_session(
            created.session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"next".to_vec(),
            },
        );

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                20,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);

        let first_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(
            first_packets.len(),
            1,
            "单个 terminal frame 大于 batch 上限时也必须向前推进，否则大 snapshot 会卡死"
        );
        assert_eq!(first_packets[0].kind, PacketKind::StreamChunk);
        assert_eq!(first_packets[0].seq, 1);
        let first: TerminalFramePayload = decode_payload(first_packets[0].payload.clone()).unwrap();
        match first {
            TerminalFramePayload::Batch { frames, .. } => {
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[0].terminal_seq(), Some(1));
                assert_eq!(frames[1].terminal_seq(), Some(2));
            }
            other => assert_eq!(other.terminal_seq(), Some(1)),
        }

        let drained = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert!(drained.is_empty());

        let flow_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::flow(stream_id, 1, 30),
        );
        assert!(
            flow_responses.is_empty(),
            "legacy flow stays accepted but does not drive terminal output"
        );
        let drained = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert!(drained.is_empty());
    }

    #[test]
    fn packet_terminal_attach_falls_back_to_snapshot_when_tail_window_is_missing() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: PublicKey("device-public-key".to_owned()),
                    token,
                    nonce: nonce(),
                    timestamp_ms: current_unix_timestamp_millis(),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_first_packet(&mut device_session, pair_responses);

        let create_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_SESSION_CREATE,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        let created_packet = decrypt_first_packet(&mut device_session, create_responses);
        let created: SessionCreatedPayload = decode_payload(created_packet.payload).unwrap();
        backend.push_terminal_journal_frame_for_session(
            created.session_id,
            PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"journal-starts-after-gap".to_vec(),
            },
        );

        let stream_id = PacketStreamId::new();
        let attach_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id: created.session_id,
                    watch_updates: true,
                    last_terminal_seq: Some(0),
                })
                .unwrap(),
            ),
        );
        let attached_packet = decrypt_first_packet(&mut device_session, attach_responses);
        assert_eq!(attached_packet.kind, PacketKind::Response);
        assert_eq!(attached_packet.stream_id, Some(stream_id));

        let output_packets = decrypt_packets(
            &mut device_session,
            connection.read_session_output(&mut protocol, created.session_id, 1024),
        );
        assert_eq!(output_packets.len(), 1);
        let frames = decode_supervisor_frames(&output_packets);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            SupervisorTerminalServerFrame::AttachSync {
                session_id,
                base_seq,
                snapshot,
                frames,
            } => {
                assert_eq!(session_id, &created.session_id.0.to_string());
                assert_eq!(*base_seq, 2);
                assert!(snapshot.retained_output.is_empty());
                assert!(
                    matches!(
                        frames.as_slice(),
                        [PtyTerminalFrame::Snapshot { base_seq: 2, .. }]
                    ),
                    "tail gap fallback must return a snapshot frame, not retained_output"
                );
            }
            other => panic!("expected fallback attach_sync snapshot, got {other:?}"),
        }
    }

    #[test]
    fn packet_auth_signature_is_bound_to_e2ee_transcript() {
        let (mut bootstrap_protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (mut pair_connection, _) = bootstrap_protocol.start_connection();
        let (_, mut pair_device_session) =
            open_e2ee(&mut bootstrap_protocol, &mut pair_connection, device_id);
        pair_device(
            &mut bootstrap_protocol,
            &mut pair_connection,
            &mut pair_device_session,
            device_id,
            public_key,
        );

        let state = bootstrap_protocol.snapshot_state();
        let (mut protocol, _) = protocol_from_state(state);
        let (mut connection, _) = protocol.start_connection();
        let (_, mut device_session, challenge_response) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        let challenge_packet = decrypt_first_packet(&mut device_session, challenge_response);
        assert_eq!(challenge_packet.kind, PacketKind::Event);
        assert_eq!(
            challenge_packet.method.as_deref(),
            Some(METHOD_AUTH_CHALLENGE)
        );
        let challenge: AuthChallengePayload = decode_payload(challenge_packet.payload).unwrap();
        let mut auth_payload = AuthPayload {
            device_id,
            challenge: challenge.challenge,
            nonce: nonce(),
            timestamp_ms: current_unix_timestamp_millis(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input = AuthSigningInput::from_payload_with_e2ee_transcript(
            &auth_payload,
            protocol.daemon_public_identity(),
            connection.e2ee_auth_transcript.as_ref(),
        )
        .to_bytes();
        auth_payload.signature = Signature(wire(&signing_key.sign(&signing_input).to_bytes()));

        let auth_request_id = PacketRequestId::new();
        let auth_responses = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                auth_request_id,
                METHOD_AUTH_VERIFY,
                serde_json::to_value(auth_payload).unwrap(),
            ),
        );
        let auth_response = decrypt_first_packet(&mut device_session, auth_responses);
        assert_eq!(auth_response.kind, PacketKind::Response);
        assert_eq!(auth_response.id, Some(auth_request_id));
        assert!(connection.is_authenticated());
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
            E2eeKeyExchangePayload::new(
                protocol.server_id(),
                device_id,
                device_e2ee_keypair.public_key_wire(),
                nonce(),
                UnixTimestampMillis(2_000),
            ),
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
    fn websocket_auth_invalid_signature_does_not_consume_replay_nonce() {
        let (mut protocol, _) = protocol();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let shared_nonce = Nonce("websocket-auth-replay-nonce".to_owned());

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

        let (mut invalid_connection, _) = protocol.start_connection();
        let (mut invalid_device_session, invalid_challenge) =
            open_auth_e2ee(&mut protocol, &mut invalid_connection, device_id);
        let invalid_payload = AuthPayload {
            device_id,
            challenge: invalid_challenge.challenge,
            nonce: shared_nonce.clone(),
            timestamp_ms: current_unix_timestamp_millis(),
            signature: Signature(wire(&signing_key.sign(b"wrong-auth-input").to_bytes())),
        };
        let invalid_responses = send_encrypted(
            &mut protocol,
            &mut invalid_connection,
            &mut invalid_device_session,
            envelope_value(MessageType::Auth, invalid_payload).unwrap(),
        );
        let error = decrypt_first(&mut invalid_device_session, invalid_responses);
        let error_payload: ErrorPayload = decode_payload(error.payload).unwrap();

        assert_eq!(error.kind, MessageType::Error);
        assert_eq!(error_payload.code, "auth_failed");
        assert!(!invalid_connection.is_authenticated());

        let (mut valid_connection, _) = protocol.start_connection();
        let (mut valid_device_session, valid_challenge) =
            open_auth_e2ee(&mut protocol, &mut valid_connection, device_id);
        let mut valid_payload = AuthPayload {
            device_id,
            challenge: valid_challenge.challenge,
            nonce: shared_nonce,
            timestamp_ms: current_unix_timestamp_millis(),
            signature: Signature("ed25519-v1:placeholder".to_owned()),
        };
        let signing_input =
            AuthSigningInput::from_payload(&valid_payload, protocol.daemon_public_identity())
                .to_bytes();
        valid_payload.signature = Signature(wire(&signing_key.sign(&signing_input).to_bytes()));

        let valid_responses = send_encrypted(
            &mut protocol,
            &mut valid_connection,
            &mut valid_device_session,
            envelope_value(MessageType::Auth, valid_payload).unwrap(),
        );

        assert!(valid_responses.is_empty());
        assert!(valid_connection.is_authenticated());
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
    fn startup_restores_live_session_cwd_watchers_from_sqlite() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-live-session-cwd-watchers.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();

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
        let mut protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();

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

        let attach_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut device_session, attach_responses);
        assert_eq!(attached.kind, MessageType::SessionAttached);

        assert!(
            connection
                .attached_cwd_signals(&protocol)
                .into_iter()
                .any(|(attached_session_id, _)| attached_session_id == session_id),
            "恢复后的 live supervisor session 仍必须注册 cwd watcher"
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
    fn startup_marks_obsolete_tmux_restore_info_records_closed() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("restore-tmux-session.json");
        let config = DaemonConfig::default_for_state_path(&state_path);
        let session_id = SessionId::new();
        let root_path = std::env::temp_dir();
        let size = TerminalSize {
            rows: 33,
            cols: 111,
            pixel_width: 800,
            pixel_height: 600,
        };
        let restore_info = PtyRestoreInfo::Tmux {
            socket_path: state_path
                .parent()
                .expect("temp state path has parent")
                .join("tmux.sock"),
            session_name: format!("termd-{}", session_id.0),
        };

        {
            let mut history = ClientHistoryStore::open(&state_path).unwrap();
            history
                .record_session_created(
                    session_id,
                    SessionState::Running,
                    size,
                    Some("tmux shell"),
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
                size,
                created_at_ms: UnixTimestampMillis(1_000),
                updated_at_ms: UnixTimestampMillis(1_002),
                restore_info: Some(restore_info.clone()),
            }],
        };
        StateStore::save(&state_path, &state).unwrap();
        let state = StateStore::load(&state_path).unwrap();

        let protocol =
            DaemonProtocol::from_state(config, backend.clone(), Ed25519SignatureVerifier, state)
                .unwrap();

        assert!(
            backend.reconnects().is_empty(),
            "生产 supervisor 启动路径不应再尝试接回旧 tmux restore 记录"
        );
        assert!(
            backend.reconnect_sizes().is_empty(),
            "旧 tmux restore 记录应该在恢复阶段直接降级为 closed"
        );
        assert!(
            !protocol.session_index.contains_key(&session_id),
            "tmux restore 记录不应继续出现在运行中的 session catalog 里"
        );
        assert!(
            protocol.snapshot_state().sessions.is_empty(),
            "被淘汰的 tmux restore 记录不应继续作为 live session 持久化回 state"
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
    fn startup_closes_stale_running_restore_records() {
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
            Vec::<SessionId>::new()
        );
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();
        assert!(
            payload.sessions.is_empty(),
            "死掉的 supervisor 不应继续作为 running session 展示并拖慢 list"
        );
        assert!(!protocol.session_index.contains_key(&stale_session_id));

        backend.allow_reconnects();
        let response = protocol
            .list_sessions(&connection, SessionListPayload {})
            .unwrap();
        let payload: SessionListResultPayload =
            decode_payload(response[0].payload.clone()).unwrap();
        assert!(payload.sessions.is_empty());
        assert!(
            !protocol.session_index.contains_key(&stale_session_id),
            "启动时已判定不可恢复的 session 不应在后续 list 中复活"
        );

        let reloaded_state = StateStore::load(&config.state_path).unwrap();
        assert!(
            reloaded_state.sessions.iter().all(|session| {
                session.state == SessionState::Closed || session.restore_info.is_none()
            }),
            "持久状态不能继续保存 running + restore_info 的死 supervisor"
        );
    }

    #[test]
    fn stale_restore_session_cannot_be_attached_after_reconnect_failure() {
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
        let reconnect_attempts_before = backend.reconnects().len();

        backend.allow_reconnects();
        let mut connection = ProtocolConnection::new(None);
        connection.authenticated_device_id = Some(DeviceId::new());

        let error = protocol
            .attach_session(
                &mut connection,
                SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap_err();

        assert!(matches!(error, ProtocolError::SessionNotFound));
        assert!(!protocol.session_index.contains_key(&session_id));
        let reconnect_attempts_after = backend.reconnects();
        assert_eq!(
            reconnect_attempts_after.len(),
            reconnect_attempts_before,
            "已判定 dead 的 session attach 时不能再同步重试旧 socket"
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
    fn attached_connection_can_resize_session_after_focus() {
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
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_crypto, attach_responses);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert_eq!(attached.kind, MessageType::SessionAttached);
        // 中文注释：终端尺寸以最后聚焦客户端为准；后 attach 的连接聚焦后也必须能接管 resize。
        assert!(attached_payload.resize_owner);

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
        let resized = decrypt_first(&mut second_crypto, resize_responses);
        let resized_payload: SessionResizedPayload = decode_payload(resized.payload).unwrap();

        assert_eq!(resized.kind, MessageType::SessionResized);
        assert_eq!(resized_payload.session_id, created_payload.session_id);
        assert_eq!(resized_payload.size.rows, 40);
        assert_eq!(resized_payload.size.cols, 120);
        assert!(resized_payload.resize_owner);
    }

    #[test]
    fn attached_connection_keeps_resize_capability_after_another_connection_disconnects() {
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
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_crypto, attach_responses);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();
        assert!(attached_payload.resize_owner);

        first_connection.close(&mut protocol);

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
                    watch_updates: true,
                    last_terminal_seq: None,
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
    fn attached_session_can_list_git_status_and_graph_from_session_cwd() {
        let base = temp_state_path("git-base");
        let root = base.join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        run_test_git(&root, &["init", "-b", "main"]);
        run_test_git(&root, &["config", "user.email", "test@example.com"]);
        run_test_git(&root, &["config", "user.name", "Termd Test"]);
        fs::write(root.join("README.md"), b"initial\n").unwrap();
        fs::write(root.join("src").join("lib.rs"), b"pub fn initial() {}\n").unwrap();
        run_test_git(&root, &["add", "README.md", "src/lib.rs"]);
        run_test_git(&root, &["commit", "-m", "main commit"]);
        fs::write(root.join("src").join("lib.rs"), b"pub fn staged() {}\n").unwrap();
        run_test_git(&root, &["add", "src/lib.rs"]);
        fs::write(root.join("README.md"), b"initial\nunstaged\n").unwrap();

        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path("git-state.json"));
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        backend.set_cwd_for_session(session_id, root.clone());

        let responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(MessageType::SessionGit, SessionGitPayload { session_id }).unwrap(),
        );
        let listed = decrypt_first(&mut device_session, responses);
        let payload: SessionGitResultPayload = decode_payload(listed.payload).unwrap();
        let current_worktree = payload
            .worktrees
            .iter()
            .find(|worktree| worktree.is_current)
            .expect("current worktree should be marked");

        assert_eq!(listed.kind, MessageType::SessionGitResult);
        assert_eq!(payload.session_id, session_id);
        assert_eq!(
            payload.repository_root,
            Some(root.to_string_lossy().to_string())
        );
        assert_eq!(payload.error, None);
        assert_eq!(current_worktree.branch.as_deref(), Some("main"));
        assert!(
            current_worktree
                .staged
                .iter()
                .any(|file| file.path == "src/lib.rs")
        );
        assert!(
            current_worktree
                .unstaged
                .iter()
                .any(|file| file.path == "README.md")
        );
        assert!(
            payload
                .graph
                .iter()
                .any(|line| line.contains("main commit"))
        );

        let unstage_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionGitAction,
                SessionGitActionPayload {
                    session_id,
                    worktree_path: root.to_string_lossy().to_string(),
                    file_path: "src/lib.rs".to_owned(),
                    action: SessionGitActionKind::Unstage,
                },
            )
            .unwrap(),
        );
        let unstage = decrypt_first(&mut device_session, unstage_responses);
        let unstage_payload: SessionGitActionResultPayload =
            decode_payload(unstage.payload).unwrap();
        assert_eq!(unstage.kind, MessageType::SessionGitActionResult);
        assert_eq!(unstage_payload.action, SessionGitActionKind::Unstage);

        let stage_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionGitAction,
                SessionGitActionPayload {
                    session_id,
                    worktree_path: root.to_string_lossy().to_string(),
                    file_path: "README.md".to_owned(),
                    action: SessionGitActionKind::Stage,
                },
            )
            .unwrap(),
        );
        let stage = decrypt_first(&mut device_session, stage_responses);
        let stage_payload: SessionGitActionResultPayload = decode_payload(stage.payload).unwrap();
        assert_eq!(stage.kind, MessageType::SessionGitActionResult);
        assert_eq!(stage_payload.action, SessionGitActionKind::Stage);

        let discard_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionGitAction,
                SessionGitActionPayload {
                    session_id,
                    worktree_path: root.to_string_lossy().to_string(),
                    file_path: "README.md".to_owned(),
                    action: SessionGitActionKind::Discard,
                },
            )
            .unwrap(),
        );
        let discard = decrypt_first(&mut device_session, discard_responses);
        let discard_payload: SessionGitActionResultPayload =
            decode_payload(discard.payload).unwrap();
        assert_eq!(discard.kind, MessageType::SessionGitActionResult);
        assert_eq!(discard_payload.action, SessionGitActionKind::Discard);

        fs::write(root.join("scratch.txt"), b"temporary\n").unwrap();
        let untracked_discard_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionGitAction,
                SessionGitActionPayload {
                    session_id,
                    worktree_path: root.to_string_lossy().to_string(),
                    file_path: "scratch.txt".to_owned(),
                    action: SessionGitActionKind::Discard,
                },
            )
            .unwrap(),
        );
        let untracked_discard = decrypt_first(&mut device_session, untracked_discard_responses);
        let untracked_discard_payload: SessionGitActionResultPayload =
            decode_payload(untracked_discard.payload).unwrap();
        assert_eq!(untracked_discard.kind, MessageType::SessionGitActionResult);
        assert_eq!(
            untracked_discard_payload.action,
            SessionGitActionKind::Discard
        );

        let status = run_test_git_stdout(&root, &["status", "--porcelain=v1"]);
        assert!(status.contains(" M src/lib.rs"));
        assert!(!status.contains("README.md"));
        assert!(!status.contains("scratch.txt"));

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_git_hides_active_http_upload_targets_and_rejects_actions() {
        let base = temp_state_path("git-active-upload-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        run_test_git(&root, &["init", "-b", "main"]);
        run_test_git(&root, &["config", "user.email", "test@example.com"]);
        run_test_git(&root, &["config", "user.name", "Termd Test"]);
        fs::write(root.join("README.md"), b"initial\n").unwrap();
        run_test_git(&root, &["add", "README.md"]);
        run_test_git(&root, &["commit", "-m", "initial"]);

        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("git-active-upload-state.json"));
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        backend.set_cwd_for_session(session_id, root.clone());

        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "partial.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let target = root.join("partial.bin");
        assert!(target.exists(), "init 必须先预分配最终目标文件");
        fs::hard_link(&target, root.join("hardlink.bin")).unwrap();
        fs::create_dir_all(root.join("aliases")).unwrap();
        fs::hard_link(&target, root.join("aliases").join("hardlink.bin")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("partial.bin", root.join("alias.bin")).unwrap();
        }

        let root_files = protocol
            .session_files_result(session_id, Some(root.to_string_lossy().to_string()), false)
            .unwrap();
        assert!(
            root_files
                .entries
                .iter()
                .all(|entry| entry.name != "partial.bin"
                    && entry.name != "hardlink.bin"
                    && entry.name != "alias.bin"),
            "文件列表不应暴露 active HTTP upload 目标及其 alias"
        );
        let alias_files = protocol
            .session_files_result(
                session_id,
                Some(root.join("aliases").to_string_lossy().to_string()),
                false,
            )
            .unwrap();
        assert!(
            alias_files
                .entries
                .iter()
                .all(|entry| entry.name != "hardlink.bin"),
            "子目录文件列表不应暴露 active HTTP upload hardlink alias"
        );
        let other_session_id =
            create_test_session(&mut protocol, &mut connection, &mut device_session);
        backend.set_cwd_for_session(other_session_id, root.clone());
        let other_root_files = protocol
            .session_files_result(
                other_session_id,
                Some(root.to_string_lossy().to_string()),
                false,
            )
            .unwrap();
        assert!(
            other_root_files
                .entries
                .iter()
                .all(|entry| entry.name != "partial.bin" && entry.name != "hardlink.bin"),
            "另一个 session 指向同一目录时也不能暴露 active HTTP upload 目标"
        );
        let other_read = protocol.read_session_file(
            &connection,
            SessionFileReadPayload {
                session_id: other_session_id,
                path: "partial.bin".to_owned(),
                max_bytes: Some(8),
            },
        );
        assert!(matches!(other_read, Err(ProtocolError::InvalidState)));

        let payload = protocol.session_git_result(session_id).unwrap();
        let current_worktree = payload
            .worktrees
            .iter()
            .find(|worktree| worktree.is_current)
            .expect("current worktree should be marked");
        assert!(
            current_worktree
                .staged
                .iter()
                .chain(current_worktree.unstaged.iter())
                .all(|change| change.path != "partial.bin"
                    && change.path != "hardlink.bin"
                    && change.path != "alias.bin"
                    && change.path != "aliases/hardlink.bin"),
            "Git 状态不应暴露 active HTTP upload 目标及其 alias"
        );

        let discard = protocol.apply_session_git_action(
            &connection,
            SessionGitActionPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: "partial.bin".to_owned(),
                action: SessionGitActionKind::Discard,
            },
        );
        assert!(matches!(discard, Err(ProtocolError::InvalidState)));
        assert!(
            target.exists(),
            "Git discard 不能删除 active HTTP upload 目标"
        );

        let diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: Some("partial.bin".to_owned()),
                staged: false,
            },
        );
        assert!(matches!(diff, Err(ProtocolError::InvalidState)));

        let full_diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: None,
                staged: false,
            },
        );
        assert!(matches!(full_diff, Err(ProtocolError::InvalidState)));

        let pathspec_discard = protocol.apply_session_git_action(
            &connection,
            SessionGitActionPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: ":(glob)*".to_owned(),
                action: SessionGitActionKind::Discard,
            },
        );
        assert!(matches!(
            pathspec_discard,
            Err(ProtocolError::InvalidEnvelope)
        ));

        let pathspec_diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: Some(":(glob)*".to_owned()),
                staged: false,
            },
        );
        assert!(matches!(pathspec_diff, Err(ProtocolError::InvalidEnvelope)));

        let wildcard_discard = protocol.apply_session_git_action(
            &connection,
            SessionGitActionPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: "*.bin".to_owned(),
                action: SessionGitActionKind::Discard,
            },
        );
        assert!(matches!(
            wildcard_discard,
            Err(ProtocolError::InvalidEnvelope)
        ));

        let wildcard_diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: Some("*.bin".to_owned()),
                staged: false,
            },
        );
        assert!(matches!(wildcard_diff, Err(ProtocolError::InvalidEnvelope)));

        let alias_dir_discard = protocol.apply_session_git_action(
            &connection,
            SessionGitActionPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: "aliases".to_owned(),
                action: SessionGitActionKind::Discard,
            },
        );
        assert!(matches!(
            alias_dir_discard,
            Err(ProtocolError::InvalidState)
        ));

        let alias_dir_diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: Some("aliases".to_owned()),
                staged: false,
            },
        );
        assert!(matches!(alias_dir_diff, Err(ProtocolError::InvalidState)));

        fs::create_dir_all(root.join("uploads")).unwrap();
        let nested_ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "uploads/partial.bin".to_owned(),
                    size_bytes: 4,
                },
                device_id,
            )
            .unwrap();
        let nested_target = root.join("uploads").join("partial.bin");
        let nested_dir_discard = protocol.apply_session_git_action(
            &connection,
            SessionGitActionPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: "uploads".to_owned(),
                action: SessionGitActionKind::Discard,
            },
        );
        assert!(matches!(
            nested_dir_discard,
            Err(ProtocolError::InvalidState)
        ));
        assert!(
            nested_target.exists(),
            "Git 目录级 discard 不能删除目录内的 active HTTP upload 目标"
        );
        let nested_dir_diff = protocol.read_session_git_diff(
            &connection,
            SessionGitDiffPayload {
                session_id,
                worktree_path: root.to_string_lossy().to_string(),
                file_path: Some("uploads".to_owned()),
                staged: false,
            },
        );
        assert!(matches!(nested_dir_diff, Err(ProtocolError::InvalidState)));
        protocol
            .abort_session_file_http_upload(
                &connection,
                &SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: nested_ready.path,
                    upload_id: nested_ready.upload_id,
                    size_bytes: nested_ready.size_bytes,
                    offset_bytes: 0,
                },
            )
            .unwrap();

        let read = protocol.read_session_file(
            &connection,
            SessionFileReadPayload {
                session_id,
                path: "partial.bin".to_owned(),
                max_bytes: Some(8),
            },
        );
        assert!(matches!(read, Err(ProtocolError::InvalidState)));

        let write = protocol.write_session_file(
            &connection,
            SessionFileWritePayload {
                session_id,
                path: "partial.bin".to_owned(),
                data_base64: general_purpose::STANDARD.encode(b"overwrite"),
            },
        );
        assert!(matches!(write, Err(ProtocolError::InvalidState)));

        let hardlink_write = protocol.write_session_file(
            &connection,
            SessionFileWritePayload {
                session_id,
                path: "hardlink.bin".to_owned(),
                data_base64: general_purpose::STANDARD.encode(b"hardlink"),
            },
        );
        assert!(matches!(hardlink_write, Err(ProtocolError::InvalidState)));

        OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_len(16)
            .unwrap();
        let resized_hardlink_write = protocol.write_session_file(
            &connection,
            SessionFileWritePayload {
                session_id,
                path: "hardlink.bin".to_owned(),
                data_base64: general_purpose::STANDARD.encode(b"resized"),
            },
        );
        assert!(matches!(
            resized_hardlink_write,
            Err(ProtocolError::InvalidState)
        ));

        #[cfg(unix)]
        {
            let symlink_write = protocol.write_session_file(
                &connection,
                SessionFileWritePayload {
                    session_id,
                    path: "alias.bin".to_owned(),
                    data_base64: general_purpose::STANDARD.encode(b"symlink"),
                },
            );
            assert!(matches!(symlink_write, Err(ProtocolError::InvalidState)));
        }

        let delete = protocol.delete_session_file(
            &connection,
            SessionFileDeletePayload {
                session_id,
                path: "partial.bin".to_owned(),
            },
        );
        assert!(matches!(delete, Err(ProtocolError::InvalidState)));
        assert!(
            target.exists(),
            "普通 file RPC 不能删除 active HTTP upload 目标"
        );

        let http_download = protocol.prepare_session_file_http_download(
            &connection,
            SessionFileHttpDownloadPayload {
                session_id,
                path: "partial.bin".to_owned(),
                offset_bytes: 0,
            },
        );
        assert!(matches!(http_download, Err(ProtocolError::InvalidState)));

        let stream_download = protocol.prepare_session_file_download_stream(
            &connection,
            SessionFileDownloadStreamPayload {
                session_id,
                path: "partial.bin".to_owned(),
            },
        );
        assert!(matches!(stream_download, Err(ProtocolError::InvalidState)));

        let token_download = protocol.prepare_session_file_download(
            &connection,
            SessionFileDownloadPreparePayload {
                session_id,
                path: "partial.bin".to_owned(),
            },
        );
        assert!(matches!(token_download, Err(ProtocolError::InvalidState)));

        let chunk_download = protocol.read_session_file_download_chunk(
            &connection,
            SessionFileDownloadChunkPayload {
                session_id,
                path: "partial.bin".to_owned(),
                offset_bytes: 0,
                max_bytes: 8,
            },
        );
        assert!(matches!(chunk_download, Err(ProtocolError::InvalidState)));

        let legacy_prepare = protocol.prepare_session_file_upload_stream(
            &connection,
            SessionFileUploadPayload {
                session_id,
                path: "partial.bin".to_owned(),
                size_bytes: 4,
            },
        );
        assert!(matches!(legacy_prepare, Err(ProtocolError::InvalidState)));

        let mutated_abort = protocol.abort_session_file_http_upload(&connection, &meta);
        assert!(matches!(mutated_abort, Err(ProtocolError::InvalidState)));
        assert!(
            protocol
                .session_file_http_uploads
                .contains_key(&ready.upload_id),
            "存在 hardlink alias 时 abort 失败后必须保留 active state"
        );

        fs::write(root.join("download-race.bin"), b"stable").unwrap();
        let (_stream_ready, mut download_stream) = protocol
            .prepare_session_file_download_stream(
                &connection,
                SessionFileDownloadStreamPayload {
                    session_id,
                    path: "download-race.bin".to_owned(),
                },
            )
            .unwrap();
        fs::remove_file(root.join("download-race.bin")).unwrap();
        let download_race_ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "download-race.bin".to_owned(),
                    size_bytes: 6,
                },
                device_id,
            )
            .unwrap();
        let (download_chunk, _) = protocol
            .read_session_file_download_stream_chunk(&mut download_stream, 16)
            .unwrap();
        assert_eq!(
            general_purpose::STANDARD
                .decode(download_chunk.data_base64)
                .unwrap(),
            b"stable"
        );
        protocol
            .abort_session_file_http_upload(
                &connection,
                &SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: download_race_ready.path,
                    upload_id: download_race_ready.upload_id,
                    size_bytes: download_race_ready.size_bytes,
                    offset_bytes: 0,
                },
            )
            .unwrap();

        fs::write(root.join("http-download-race.bin"), b"http-stable").unwrap();
        let (_http_ready, mut http_file, http_offset) = protocol
            .prepare_session_file_http_download(
                &connection,
                SessionFileHttpDownloadPayload {
                    session_id,
                    path: "http-download-race.bin".to_owned(),
                    offset_bytes: 0,
                },
            )
            .unwrap();
        fs::remove_file(root.join("http-download-race.bin")).unwrap();
        let http_download_race_ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "http-download-race.bin".to_owned(),
                    size_bytes: 11,
                },
                device_id,
            )
            .unwrap();
        http_file.seek(SeekFrom::Start(http_offset)).unwrap();
        let mut http_download_bytes = Vec::new();
        http_file.read_to_end(&mut http_download_bytes).unwrap();
        assert_eq!(http_download_bytes, b"http-stable");
        protocol
            .abort_session_file_http_upload(
                &connection,
                &SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: http_download_race_ready.path,
                    upload_id: http_download_race_ready.upload_id,
                    size_bytes: http_download_race_ready.size_bytes,
                    offset_bytes: 0,
                },
            )
            .unwrap();

        let (_legacy_ready, mut legacy_stream) = protocol
            .prepare_session_file_upload_stream(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "race.bin".to_owned(),
                    size_bytes: 4,
                },
            )
            .unwrap();
        let race_ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "race.bin".to_owned(),
                    size_bytes: 4,
                },
                device_id,
            )
            .unwrap();
        let legacy_commit = protocol.write_session_file_upload_stream_chunk(
            &mut legacy_stream,
            SessionFileTransferChunkPayload {
                session_id,
                offset_bytes: 0,
                data_base64: general_purpose::STANDARD.encode(b"race"),
                size_bytes: 4,
                eof: true,
            },
        );
        assert!(matches!(legacy_commit, Err(ProtocolError::InvalidState)));
        cleanup_upload_temp(&legacy_stream);
        protocol
            .abort_session_file_http_upload(
                &connection,
                &SessionFileHttpUploadStreamPayload {
                    session_id,
                    path: race_ready.path,
                    upload_id: race_ready.upload_id,
                    size_bytes: race_ready.size_bytes,
                    offset_bytes: 0,
                },
            )
            .unwrap();
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn attached_session_can_read_git_diff() {
        let base = temp_state_path("git-workflow-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        run_test_git(&root, &["init", "-b", "main"]);
        run_test_git(&root, &["config", "user.email", "test@example.com"]);
        run_test_git(&root, &["config", "user.name", "Termd Test"]);
        fs::write(root.join("README.md"), b"initial\n").unwrap();
        run_test_git(&root, &["add", "README.md"]);
        run_test_git(&root, &["commit", "-m", "initial"]);
        fs::write(root.join("README.md"), b"initial\nstaged\n").unwrap();
        run_test_git(&root, &["add", "README.md"]);

        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("git-workflow-state.json"));
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        backend.set_cwd_for_session(session_id, root.clone());

        let diff_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionGitDiff,
                SessionGitDiffPayload {
                    session_id,
                    worktree_path: root.to_string_lossy().to_string(),
                    file_path: Some("README.md".to_owned()),
                    staged: true,
                },
            )
            .unwrap(),
        );
        let diff = decrypt_first(&mut device_session, diff_responses);
        let diff_payload: SessionGitDiffResultPayload = decode_payload(diff.payload).unwrap();
        assert_eq!(diff.kind, MessageType::SessionGitDiffResult);
        assert!(diff_payload.diff.contains("+staged"));

        fs::remove_dir_all(base).ok();
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
    fn session_files_without_path_keeps_persisted_position_when_terminal_cwd_is_unreadable() {
        let base = temp_state_path("shared-files-unreadable-cwd-base");
        let root = base.join("project");
        let manual = root.join("manual");
        let terminal_cwd = root.join("terminal-cwd");
        let missing_terminal_cwd = base.join("deleted-cwd");
        fs::create_dir_all(&manual).unwrap();
        fs::create_dir_all(&terminal_cwd).unwrap();
        fs::write(manual.join("manual.txt"), b"manual\n").unwrap();
        fs::write(terminal_cwd.join("cwd.txt"), b"cwd\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "shared-files-unreadable-cwd-state.json",
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

        backend.set_cwd_for_session(created_payload.session_id, terminal_cwd.clone());
        let terminal_cwd_responses = send_encrypted(
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
        let terminal_cwd_listed = decrypt_first(&mut device_session, terminal_cwd_responses);
        let terminal_cwd_payload: SessionFilesResultPayload =
            decode_payload(terminal_cwd_listed.payload).unwrap();
        assert_eq!(terminal_cwd_listed.kind, MessageType::SessionFilesResult);
        assert_eq!(terminal_cwd_payload.path, terminal_cwd.to_string_lossy());

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

        backend.set_cwd_for_session(created_payload.session_id, missing_terminal_cwd);
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
        assert_eq!(cwd_payload.path, manual.to_string_lossy());
        assert!(
            cwd_payload
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
    fn manual_file_tree_browse_does_not_push_updates_to_other_connections() {
        let base = temp_state_path("shared-file-tree-browse-base");
        let root = base.join("project");
        let work = base.join("work");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("beta.log"), b"sync\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "shared-file-tree-browse-state.json",
        ));
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
                    watch_updates: true,
                    last_terminal_seq: None,
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

        let pushed =
            second_connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        assert!(pushed.is_empty());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn terminal_output_pushes_cwd_update_when_terminal_cwd_changes() {
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
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_crypto, attach_responses);
        assert_eq!(attached.kind, MessageType::SessionAttached);
        let mut cwd_signal = second_connection
            .attached_cwd_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to cwd changes");
        cwd_signal.borrow_and_update();

        backend.set_cwd_for_session(created_payload.session_id, work.clone());
        backend.push_output_for_session(created_payload.session_id, b"$ ");
        let output_responses =
            first_connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert!(!output_responses.is_empty());
        assert!(cwd_signal.has_changed().unwrap());

        let pushed =
            second_connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        let pushed = decrypt_first(&mut second_crypto, pushed);
        let pushed_payload: SessionCwdChangedPayload = decode_payload(pushed.payload).unwrap();

        assert_eq!(pushed.kind, MessageType::SessionCwdChanged);
        assert_eq!(pushed_payload.cwd, work.to_string_lossy());

        let files_responses = send_encrypted(
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
        let files = decrypt_first(&mut second_crypto, files_responses);
        let files_payload: SessionFilesResultPayload = decode_payload(files.payload).unwrap();
        assert_eq!(files.kind, MessageType::SessionFilesResult);
        assert_eq!(files_payload.path, work.to_string_lossy());
        assert!(
            files_payload
                .entries
                .iter()
                .any(|entry| entry.name == "from-cwd.txt")
        );

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn terminal_output_hot_path_only_notifies_cwd_probe() {
        let base = temp_state_path("terminal-cwd-hot-path-base");
        let root = base.join("project");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("from-cwd.txt"), b"cwd\n").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "terminal-cwd-hot-path-state.json",
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let mut cwd_signal = connection
            .attached_cwd_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to cwd changes");
        cwd_signal.borrow_and_update();

        backend.set_cwd_for_session(created_payload.session_id, work.clone());
        backend.push_output_for_session(created_payload.session_id, b"$ ");
        let output_responses =
            connection.read_session_output(&mut protocol, created_payload.session_id, 4096);

        assert!(!output_responses.is_empty());
        let _ = decrypt_first(&mut device_session, output_responses);
        assert_eq!(
            backend.cwd_read_count_for_session(created_payload.session_id),
            0
        );
        assert!(cwd_signal.has_changed().unwrap());

        let pushed = connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        let pushed = decrypt_first(&mut device_session, pushed);
        let pushed_payload: SessionCwdChangedPayload = decode_payload(pushed.payload).unwrap();

        assert_eq!(
            backend.cwd_read_count_for_session(created_payload.session_id),
            1
        );
        assert_eq!(pushed.kind, MessageType::SessionCwdChanged);
        assert_eq!(pushed_payload.cwd, work.to_string_lossy());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_cwd_update_survives_prior_session_files_refresh() {
        let base = temp_state_path("session-cwd-refresh-order-base");
        let root = base.join("project");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "session-cwd-refresh-order-state.json",
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let mut cwd_signal = connection
            .attached_cwd_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to cwd changes");
        cwd_signal.borrow_and_update();

        backend.set_cwd_for_session(created_payload.session_id, work.clone());
        backend.push_output_for_session(created_payload.session_id, b"$ ");
        let output_responses =
            connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert!(!output_responses.is_empty());
        let _ = decrypt_first(&mut device_session, output_responses);
        assert!(cwd_signal.has_changed().unwrap());

        let files_responses = send_encrypted(
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
        let files = decrypt_first(&mut device_session, files_responses);
        let files_payload: SessionFilesResultPayload = decode_payload(files.payload).unwrap();
        assert_eq!(files.kind, MessageType::SessionFilesResult);
        assert_eq!(files_payload.path, work.to_string_lossy());

        let cwd_update =
            connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        let cwd_update = decrypt_first(&mut device_session, cwd_update);
        let cwd_payload: SessionCwdChangedPayload = decode_payload(cwd_update.payload).unwrap();

        assert_eq!(cwd_update.kind, MessageType::SessionCwdChanged);
        assert_eq!(cwd_payload.cwd, work.to_string_lossy());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn terminal_cwd_probe_does_not_repeat_same_cwd_event() {
        let base = temp_state_path("terminal-cwd-repeat-suppressed-base");
        let root = base.join("project");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "terminal-cwd-repeat-suppressed-state.json",
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let mut cwd_signal = connection
            .attached_cwd_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to cwd changes");
        cwd_signal.borrow_and_update();

        backend.set_cwd_for_session(created_payload.session_id, work.clone());
        backend.push_output_for_session(created_payload.session_id, b"$ ");
        let output_responses =
            connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert!(!output_responses.is_empty());
        let _ = decrypt_first(&mut device_session, output_responses);
        assert!(cwd_signal.has_changed().unwrap());

        let first_update =
            connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        let first_update = decrypt_first(&mut device_session, first_update);
        let first_payload: SessionCwdChangedPayload = decode_payload(first_update.payload).unwrap();
        assert_eq!(first_update.kind, MessageType::SessionCwdChanged);
        assert_eq!(first_payload.cwd, work.to_string_lossy());

        backend.push_output_for_session(created_payload.session_id, b"echo same-cwd\n");
        let next_output =
            connection.read_session_output(&mut protocol, created_payload.session_id, 4096);
        assert!(!next_output.is_empty());
        let _ = decrypt_first(&mut device_session, next_output);
        assert!(cwd_signal.has_changed().unwrap());

        let second_update =
            connection.read_session_cwd_update(&mut protocol, created_payload.session_id);
        assert!(second_update.is_empty());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    #[ignore = "obsolete under supervisor-owned attach protocol"]
    fn packet_terminal_output_does_not_poll_cwd_per_frame() {
        let base = temp_state_path("packet-terminal-cwd-hot-path-base");
        let root = base.join("project");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "packet-terminal-cwd-hot-path-state.json",
        ));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root);
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
        let created_responses = send_encrypted(
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
        let created = decrypt_first(&mut device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        let mut cwd_signal = connection
            .attached_cwd_signals(&protocol)
            .into_iter()
            .find(|(session_id, _)| *session_id == created_payload.session_id)
            .map(|(_, signal)| signal)
            .expect("attached connection should subscribe to cwd changes");
        cwd_signal.borrow_and_update();

        connection.packet_mode = true;
        connection
            .register_packet_terminal_stream(PacketStreamId::new(), created_payload.session_id);
        backend.set_cwd_for_session(created_payload.session_id, work);
        for line in [
            b"one\n".as_slice(),
            b"two\n".as_slice(),
            b"three\n".as_slice(),
        ] {
            backend.push_output_for_session(created_payload.session_id, line.to_vec());
        }

        let messages = connection
            .try_drain_session_output_messages_for_push(
                &mut protocol,
                created_payload.session_id,
                4096,
            )
            .unwrap();

        assert!(!messages.is_empty());
        assert_eq!(
            backend.cwd_read_count_for_session(created_payload.session_id),
            0
        );
        assert!(cwd_signal.has_changed().unwrap());

        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_id_is_random_and_path_safe() {
        let first = session_file_http_upload_id();
        let second = session_file_http_upload_id();

        assert_ne!(first, second);
        assert!(session_file_http_upload_id_is_safe(&first));
        assert!(session_file_http_upload_id_is_safe(&second));
        assert!(!session_file_http_upload_id_is_safe("../escape"));
        assert!(!session_file_http_upload_id_is_safe("with/slash"));
    }

    #[test]
    fn session_file_http_upload_init_cleans_target_when_set_len_fails() {
        let base = temp_state_path("http-upload-set-len-fail-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("failed.bin");

        let result = create_session_file_http_upload_target_with_set_len(
            &target,
            8,
            |_file, _size_bytes| Err(std::io::Error::other("forced set_len failure")),
        );

        assert!(result.is_err());
        assert!(
            !target.exists(),
            "set_len 失败时不能留下未登记的最终目标文件"
        );
        fs::remove_dir_all(base).ok();
    }

    fn http_upload_temp_names(parent: &Path) -> Vec<String> {
        // 中文注释：HTTP upload 新模型必须直接 patch 目标文件；测试用这个 helper
        // 拦住旧的 .chunk/.part 临时文件路径回流。
        let mut names: Vec<String> = fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| {
                name.contains(".termd-http-upload-")
                    || name.ends_with(".part")
                    || name.ends_with(".chunk")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn http_e2ee_invalid_signature_does_not_consume_replay_nonce() {
        let backend = FakePtyBackend::default();
        let config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-e2ee-invalid-signature-state.json",
        ));
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

        let auth = signed_http_e2ee_auth(
            &protocol,
            device_id,
            &signing_key,
            Nonce("http-replay-nonce".to_owned()),
            "POST",
            "/api/files/download",
        );
        let mut invalid = auth.clone();
        invalid.signature = Signature("ed25519-v1:invalid".to_owned());

        assert!(matches!(
            protocol.open_http_e2ee_session(invalid),
            Err(ProtocolError::AuthFailed)
        ));
        assert!(
            protocol.open_http_e2ee_session(auth).is_ok(),
            "valid HTTP E2EE request must still be accepted after an invalid signature reused its nonce"
        );
    }

    #[test]
    fn session_file_http_upload_abort_requires_attached_connection() {
        let base = temp_state_path("http-upload-abort-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("http-upload-abort-state.json"));
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "partial.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let target = root.join("partial.bin");
        assert!(target.exists(), "init 必须直接创建最终目标文件");
        assert_eq!(
            fs::metadata(&target).unwrap().len(),
            8,
            "init 必须把最终目标文件长度设置为声明大小"
        );
        assert!(
            http_upload_temp_names(&root).is_empty(),
            "HTTP upload 不应再产生临时分片文件"
        );
        let active_files = protocol
            .session_files_result(
                created_payload.session_id,
                Some(absolute_path_string(&root)),
                false,
            )
            .unwrap();
        assert!(
            active_files
                .entries
                .iter()
                .all(|entry| entry.name != "partial.bin"),
            "active HTTP upload 目标文件在 commit 前不应出现在文件列表里"
        );

        protocol
            .write_session_file_http_upload(
                &connection,
                meta.clone(),
                device_id,
                vec![b"part".to_vec()],
            )
            .unwrap();
        assert_eq!(&fs::read(&target).unwrap()[..4], b"part");
        assert!(
            http_upload_temp_names(&root).is_empty(),
            "分片写入也只能 patch 目标文件，不能创建临时文件"
        );
        let partial_files = protocol
            .session_files_result(
                created_payload.session_id,
                Some(absolute_path_string(&root)),
                false,
            )
            .unwrap();
        assert!(
            partial_files
                .entries
                .iter()
                .all(|entry| entry.name != "partial.bin"),
            "未完成 HTTP upload 仍然必须从文件列表隐藏"
        );

        let http_connection = ProtocolConnection::authenticated_http(device_id);
        assert!(matches!(
            protocol.abort_session_file_http_upload(&http_connection, &meta),
            Err(ProtocolError::InvalidState)
        ));
        protocol
            .abort_session_file_http_upload(&connection, &meta)
            .unwrap();

        assert!(!target.exists());
        assert!(http_upload_temp_names(&root).is_empty());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_accepts_out_of_order_chunks_and_assembles() {
        let base = temp_state_path("http-upload-out-of-order-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-out-of-order-state.json",
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "joined.bin".to_owned(),
                    size_bytes: 10,
                },
                device_id,
            )
            .unwrap();

        let second_half = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 5,
        };
        let target = root.join("joined.bin");
        assert!(target.exists(), "init 后目标文件应立即存在");
        assert_eq!(fs::metadata(&target).unwrap().len(), 10);
        assert!(
            http_upload_temp_names(&root).is_empty(),
            "init 不应生成 .part/.chunk"
        );

        let progress = protocol
            .write_session_file_http_upload(
                &connection,
                second_half,
                device_id,
                vec![b"world".to_vec()],
            )
            .unwrap();
        assert_eq!(progress.offset_bytes, 5);
        assert!(!progress.eof);
        assert_eq!(&fs::read(&target).unwrap()[5..], b"world");
        assert!(
            http_upload_temp_names(&root).is_empty(),
            "乱序分片也不应落临时文件"
        );

        let first_half = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path,
            upload_id: ready.upload_id,
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let progress = protocol
            .write_session_file_http_upload(
                &connection,
                first_half,
                device_id,
                vec![b"hello".to_vec()],
            )
            .unwrap();
        assert_eq!(progress.offset_bytes, 10);
        assert!(progress.eof);
        assert_eq!(fs::read(&target).unwrap(), b"helloworld");
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_concurrent_non_overlapping_writes_keep_offsets() {
        let base = temp_state_path("http-upload-concurrent-offset-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-concurrent-offset-state.json",
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let chunk_len = 512 * 1024;
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "concurrent.bin".to_owned(),
                    size_bytes: (chunk_len * 2) as u64,
                },
                device_id,
            )
            .unwrap();
        let first_meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let second_meta = SessionFileHttpUploadStreamPayload {
            offset_bytes: chunk_len as u64,
            ..first_meta.clone()
        };
        let first_plan = match protocol
            .begin_session_file_http_upload_write(
                &connection,
                first_meta.clone(),
                device_id,
                chunk_len as u64,
            )
            .unwrap()
        {
            SessionFileHttpUploadBegin::Write(plan) => plan,
            SessionFileHttpUploadBegin::Complete(_) => panic!("upload should not be complete"),
        };
        let second_plan = match protocol
            .begin_session_file_http_upload_write(
                &connection,
                second_meta.clone(),
                device_id,
                chunk_len as u64,
            )
            .unwrap()
        {
            SessionFileHttpUploadBegin::Write(plan) => plan,
            SessionFileHttpUploadBegin::Complete(_) => panic!("upload should not be complete"),
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let first_barrier = barrier.clone();
        let first_handle = std::thread::spawn(move || {
            first_barrier.wait();
            write_session_file_http_upload_files(first_plan, vec![vec![b'a'; chunk_len]])
        });
        let second_barrier = barrier.clone();
        let second_handle = std::thread::spawn(move || {
            second_barrier.wait();
            write_session_file_http_upload_files(second_plan, vec![vec![b'b'; chunk_len]])
        });
        barrier.wait();
        let first_result = first_handle.join().unwrap().unwrap();
        let second_result = second_handle.join().unwrap().unwrap();

        protocol
            .commit_session_file_http_upload_write(&first_meta, &first_result)
            .unwrap();
        let progress = match protocol
            .commit_session_file_http_upload_write(&second_meta, &second_result)
            .unwrap()
        {
            SessionFileHttpUploadCommit::Progress(progress)
            | SessionFileHttpUploadCommit::Complete(progress) => progress,
        };

        assert!(progress.eof);
        let bytes = fs::read(root.join("concurrent.bin")).unwrap();
        assert_eq!(&bytes[..chunk_len], vec![b'a'; chunk_len].as_slice());
        assert_eq!(&bytes[chunk_len..], vec![b'b'; chunk_len].as_slice());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_retries_same_offset_idempotently() {
        let base = temp_state_path("http-upload-idempotent-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-idempotent-state.json",
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "retry.bin".to_owned(),
                    size_bytes: 5,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        protocol
            .write_session_file_http_upload(
                &connection,
                meta.clone(),
                device_id,
                vec![b"hello".to_vec()],
            )
            .unwrap();
        let retry = protocol
            .write_session_file_http_upload(&connection, meta, device_id, vec![b"hello".to_vec()])
            .unwrap();

        assert!(retry.eof);
        assert_eq!(fs::read(root.join("retry.bin")).unwrap(), b"hello");
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_retry_can_cover_adjacent_written_ranges() {
        let base = temp_state_path("http-upload-adjacent-retry-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-adjacent-retry-state.json",
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "adjacent-retry.bin".to_owned(),
                    size_bytes: 10,
                },
                device_id,
            )
            .unwrap();
        let first_meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        protocol
            .write_session_file_http_upload(
                &connection,
                first_meta.clone(),
                device_id,
                vec![b"he".to_vec(), b"llo".to_vec()],
            )
            .unwrap();
        let retry = protocol
            .write_session_file_http_upload(
                &connection,
                first_meta,
                device_id,
                vec![b"hello".to_vec()],
            )
            .unwrap();

        assert_eq!(retry.offset_bytes, 5);
        assert!(!retry.eof);
        assert_eq!(
            &fs::read(root.join("adjacent-retry.bin")).unwrap()[..5],
            b"hello"
        );
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_rejects_partial_duplicate_overwrite() {
        let base = temp_state_path("http-upload-partial-duplicate-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-partial-duplicate-state.json",
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "partial-duplicate.bin".to_owned(),
                    size_bytes: 10,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        protocol
            .write_session_file_http_upload(
                &connection,
                meta.clone(),
                device_id,
                vec![b"hello".to_vec()],
            )
            .unwrap();
        let overwrite = protocol.write_session_file_http_upload(
            &connection,
            meta,
            device_id,
            vec![b"xxxxx".to_vec()],
        );

        assert!(
            overwrite.is_err(),
            "未完成上传的已写区间不能被不同内容的旧请求覆盖"
        );
        assert_eq!(
            &fs::read(root.join("partial-duplicate.bin")).unwrap()[..5],
            b"hello"
        );
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_reserves_inflight_ranges_before_file_io() {
        let base = temp_state_path("http-upload-inflight-range-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-inflight-range-state.json",
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "reserved.bin".to_owned(),
                    size_bytes: 10,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        protocol
            .begin_session_file_http_upload_write(&connection, meta.clone(), device_id, 5)
            .unwrap();
        let duplicate_begin =
            protocol.begin_session_file_http_upload_write(&connection, meta, device_id, 5);

        assert!(
            matches!(duplicate_begin, Err(ProtocolError::InvalidState)),
            "锁外文件 I/O 开始前必须预留区间，阻止并发旧请求覆盖同一 offset"
        );
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_abort_clears_inflight_before_late_commit() {
        let base = temp_state_path("http-upload-abort-inflight-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-abort-inflight-state.json",
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "late-commit.bin".to_owned(),
                    size_bytes: 5,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let plan = match protocol
            .begin_session_file_http_upload_write(&connection, meta.clone(), device_id, 5)
            .unwrap()
        {
            SessionFileHttpUploadBegin::Write(plan) => plan,
            SessionFileHttpUploadBegin::Complete(_) => panic!("upload should not be complete"),
        };
        let file_result = write_session_file_http_upload_files(plan, vec![b"hello".to_vec()])
            .expect("file write should finish before abort wins commit race");

        protocol
            .abort_session_file_http_upload(&connection, &meta)
            .unwrap();
        let late_commit = protocol.commit_session_file_http_upload_write(&meta, &file_result);

        assert!(matches!(late_commit, Err(ProtocolError::InvalidState)));
        assert!(
            protocol
                .session_file_http_uploads
                .get(&ready.upload_id)
                .unwrap()
                .inflight_ranges
                .is_empty(),
            "abort 与 late commit 竞态后不能残留 in-flight range"
        );
        assert!(!root.join("late-commit.bin").exists());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_http_upload_startup_cleanup_removes_matching_target_and_drops_record() {
        let base = temp_state_path("http-upload-startup-cleanup-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-startup-cleanup-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let target = root.join("stale-after-restart.bin");
        let upload_id = {
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
            let session_id =
                create_test_session(&mut protocol, &mut connection, &mut device_session);
            let ready = protocol
                .prepare_session_file_http_upload(
                    &connection,
                    SessionFileUploadPayload {
                        session_id,
                        path: "stale-after-restart.bin".to_owned(),
                        size_bytes: 8,
                    },
                    device_id,
                )
                .unwrap();
            assert!(target.exists());
            assert_eq!(StateStore::list_http_uploads(&state_path).unwrap().len(), 1);
            ready.upload_id
        };

        cleanup_persisted_session_file_http_uploads(&state_path).unwrap();

        assert!(
            !target.exists(),
            "daemon 重启后 recovery record 仍存在，说明 upload 未 commit，必须删除预分配目标"
        );
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .all(|record| record.upload_id != upload_id)
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_cleanup_removes_same_object_after_length_change() {
        let base = temp_state_path("http-upload-cleanup-mutated-length-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("mutated-length.bin");
        let (_file, identity) = create_session_file_http_upload_target(&target, 8).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_len(4)
            .unwrap();

        let outcome = remove_session_file_http_upload_target(&target, identity).unwrap();

        assert_eq!(outcome, SessionFileHttpUploadCleanupOutcome::Removed);
        assert!(
            !target.exists(),
            "同一个文件对象只改了长度，cleanup 仍应删除 active upload 目标"
        );
        fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_startup_cleanup_keeps_record_for_ambiguous_same_object_change() {
        let base = temp_state_path("http-upload-startup-ambiguous-same-object-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-startup-ambiguous-same-object-state.json");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("ambiguous.bin");
        let (_file, identity) = create_session_file_http_upload_target(&target, 8).unwrap();
        let upload_id = session_file_http_upload_id();
        StateStore::record_http_upload(
            &state_path,
            &HttpUploadRecoveryRecord {
                upload_id: upload_id.clone(),
                target_path: target.clone(),
                size_bytes: 8,
                dev: identity.dev(),
                ino: identity.ino(),
                updated_at_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_len(4)
            .unwrap();

        let cleanup = cleanup_persisted_session_file_http_uploads(&state_path);

        assert!(
            cleanup.is_err(),
            "启动 recovery 没有 open file handle，same inode 但长度变化必须安全失败"
        );
        assert!(target.exists());
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == upload_id),
            "歧义 cleanup 失败时必须保留 recovery record，避免丢 guard"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_startup_cleanup_keeps_record_when_target_missing() {
        let base = temp_state_path("http-upload-startup-missing-target-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-startup-missing-target-state.json");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("missing-target.bin");
        let alias = root.join("missing-target-hardlink.bin");
        let (_file, identity) = create_session_file_http_upload_target(&target, 8).unwrap();
        let upload_id = session_file_http_upload_id();
        StateStore::record_http_upload(
            &state_path,
            &HttpUploadRecoveryRecord {
                upload_id: upload_id.clone(),
                target_path: target.clone(),
                size_bytes: 8,
                dev: identity.dev(),
                ino: identity.ino(),
                updated_at_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        fs::hard_link(&target, &alias).unwrap();
        fs::remove_file(&target).unwrap();

        let cleanup = cleanup_persisted_session_file_http_uploads(&state_path);

        assert!(
            cleanup.is_err(),
            "启动 recovery 没有 open file handle，target missing 也必须安全失败"
        );
        assert!(alias.exists());
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == upload_id),
            "target missing 不能删除 recovery record，避免 hardlink alias 暴露"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[test]
    fn session_file_http_upload_init_rejects_existing_target_without_truncating() {
        let base = temp_state_path("http-upload-existing-target-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let existing = root.join("existing.bin");
        fs::write(&existing, b"keep-existing").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-existing-target-state.json",
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
        let result = protocol.prepare_session_file_http_upload(
            &connection,
            SessionFileUploadPayload {
                session_id: created_payload.session_id,
                path: "existing.bin".to_owned(),
                size_bytes: 4,
            },
            device_id,
        );

        assert!(result.is_err(), "直接目标写模型不能静默覆盖已有文件");
        assert_eq!(fs::read(&existing).unwrap(), b"keep-existing");
        assert!(http_upload_temp_names(&root).is_empty());
        fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_startup_cleanup_keeps_record_for_replaced_target() {
        let base = temp_state_path("http-upload-startup-replaced-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-startup-replaced-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let target = root.join("startup-replacement.bin");
        let upload_id = {
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
            let session_id =
                create_test_session(&mut protocol, &mut connection, &mut device_session);
            let ready = protocol
                .prepare_session_file_http_upload(
                    &connection,
                    SessionFileUploadPayload {
                        session_id,
                        path: "startup-replacement.bin".to_owned(),
                        size_bytes: 8,
                    },
                    device_id,
                )
                .unwrap();
            assert_eq!(StateStore::list_http_uploads(&state_path).unwrap().len(), 1);
            ready.upload_id
        };
        let replacement = root.join("startup-replacement-source.bin");
        fs::write(&replacement, b"user-file").unwrap();
        fs::rename(&replacement, &target).unwrap();

        let cleanup = cleanup_persisted_session_file_http_uploads(&state_path);

        assert!(
            cleanup.is_err(),
            "启动 recovery 遇到 replacement 时没有原文件句柄，必须保留 record 并安全失败"
        );
        assert_eq!(fs::read(&target).unwrap(), b"user-file");
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == upload_id),
            "replacement cleanup 不能删除 recovery record，避免原 active 对象仍有 alias"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_abort_does_not_delete_replaced_target() {
        let base = temp_state_path("http-upload-replaced-target-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-replaced-target-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "replacement.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let target = root.join("replacement.bin");
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"user-file").unwrap();
        let upload_id = ready.upload_id.clone();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path,
            upload_id: upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };
        let abort = protocol.abort_session_file_http_upload(&connection, &meta);

        assert!(
            matches!(abort, Err(ProtocolError::InvalidState)),
            "旧 upload_id 不能删除 init 后被外部替换的同名目标文件"
        );
        assert_eq!(fs::read(&target).unwrap(), b"user-file");
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .all(|record| record.upload_id != upload_id),
            "replacement abort 后旧 recovery record 必须收敛删除"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_abort_keeps_guard_when_replaced_target_has_hardlink_alias() {
        let base = temp_state_path("http-upload-replaced-hardlink-alias-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-replaced-hardlink-alias-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "replacement-with-alias.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let target = root.join("replacement-with-alias.bin");
        let alias = root.join("replacement-with-alias-hardlink.bin");
        fs::hard_link(&target, &alias).unwrap();
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"user-file").unwrap();
        let upload_id = ready.upload_id.clone();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path,
            upload_id: upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        let abort = protocol.abort_session_file_http_upload(&connection, &meta);

        assert!(matches!(abort, Err(ProtocolError::InvalidState)));
        assert!(
            protocol.session_file_http_uploads.contains_key(&upload_id),
            "target path 被替换但原 active 对象仍有 hardlink alias 时必须保留 guard"
        );
        assert_eq!(fs::read(&target).unwrap(), b"user-file");
        assert!(alias.exists());
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == upload_id),
            "hardlink alias 仍可访问未完成对象时 recovery record 不能被删除"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_abort_keeps_guard_when_missing_target_has_hardlink_alias() {
        let base = temp_state_path("http-upload-missing-hardlink-alias-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-missing-hardlink-alias-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "missing-with-alias.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let target = root.join("missing-with-alias.bin");
        let alias = root.join("missing-with-alias-hardlink.bin");
        fs::hard_link(&target, &alias).unwrap();
        fs::remove_file(&target).unwrap();
        let upload_id = ready.upload_id.clone();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id,
            path: ready.path,
            upload_id: upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        let abort = protocol.abort_session_file_http_upload(&connection, &meta);

        assert!(matches!(abort, Err(ProtocolError::InvalidState)));
        assert!(
            protocol.session_file_http_uploads.contains_key(&upload_id),
            "target path 缺失但原 active 对象仍有 hardlink alias 时必须保留 guard"
        );
        assert!(alias.exists());
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == upload_id),
            "hardlink alias 仍可访问未完成对象时 recovery record 不能被删除"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_idle_prune_keeps_replaced_target() {
        let base = temp_state_path("http-upload-replaced-prune-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-replaced-prune-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "prune-replacement.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let target = root.join("prune-replacement.bin");
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"user-file").unwrap();
        protocol
            .session_file_http_uploads
            .get_mut(&ready.upload_id)
            .unwrap()
            .updated_at_ms = 0;

        protocol.prune_session_file_http_uploads();

        assert_eq!(fs::read(&target).unwrap(), b"user-file");
        assert!(
            !protocol
                .session_file_http_uploads
                .contains_key(&ready.upload_id)
        );
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .all(|record| record.upload_id != ready.upload_id),
            "replacement prune 后旧 recovery record 必须收敛删除"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[test]
    fn session_file_http_upload_prunes_idle_active_target() {
        let base = temp_state_path("http-upload-idle-active-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-idle-active-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "stale.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let target = root.join("stale.bin");
        protocol
            .session_file_http_uploads
            .get_mut(&ready.upload_id)
            .unwrap()
            .updated_at_ms = 0;

        protocol.prune_session_file_http_uploads();

        assert!(
            !target.exists(),
            "idle Active upload 需要清理预分配目标文件"
        );
        assert!(
            !protocol
                .session_file_http_uploads
                .contains_key(&ready.upload_id)
        );
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .all(|record| record.upload_id != ready.upload_id)
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_file_http_upload_idle_prune_keeps_state_when_cleanup_fails() {
        let base = temp_state_path("http-upload-idle-active-failed-cleanup-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-idle-active-failed-cleanup-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id,
                    path: "stale-failed-cleanup.bin".to_owned(),
                    size_bytes: 8,
                },
                device_id,
            )
            .unwrap();
        let protected_target = PathBuf::from("/");
        let protected_metadata = fs::symlink_metadata(&protected_target).unwrap();
        {
            let state = protocol
                .session_file_http_uploads
                .get_mut(&ready.upload_id)
                .unwrap();
            state.target = protected_target;
            state.file_identity =
                SessionFileHttpUploadFileIdentity::from_metadata(&protected_metadata);
            state.updated_at_ms = 0;
        }

        protocol.prune_session_file_http_uploads();

        assert!(
            protocol
                .session_file_http_uploads
                .contains_key(&ready.upload_id),
            "cleanup 失败时必须保留 active state，继续隔离未完成目标"
        );
        assert!(
            protocol
                .session_file_http_uploads
                .get(&ready.upload_id)
                .unwrap()
                .updated_at_ms
                > 0,
            "cleanup 失败后要刷新 updated_at，避免每次请求都重复失败清理"
        );
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .any(|record| record.upload_id == ready.upload_id),
            "cleanup 失败时 recovery record 必须保留，供后续启动重试"
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[test]
    fn session_file_http_upload_complete_tombstone_rejects_late_overwrite() {
        let base = temp_state_path("http-upload-complete-tombstone-base");
        let root = base.join("project");
        let state_path = temp_state_path("http-upload-complete-tombstone-state.json");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(&state_path);
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
        let ready = protocol
            .prepare_session_file_http_upload(
                &connection,
                SessionFileUploadPayload {
                    session_id: created_payload.session_id,
                    path: "late.bin".to_owned(),
                    size_bytes: 5,
                },
                device_id,
            )
            .unwrap();
        let meta = SessionFileHttpUploadStreamPayload {
            session_id: created_payload.session_id,
            path: ready.path.clone(),
            upload_id: ready.upload_id.clone(),
            size_bytes: ready.size_bytes,
            offset_bytes: 0,
        };

        protocol
            .write_session_file_http_upload(
                &connection,
                meta.clone(),
                device_id,
                vec![b"hello".to_vec()],
            )
            .unwrap();
        let late = protocol
            .write_session_file_http_upload(&connection, meta, device_id, vec![b"xxxxx".to_vec()])
            .unwrap();

        assert!(late.eof);
        assert_eq!(fs::read(root.join("late.bin")).unwrap(), b"hello");
        assert!(
            StateStore::list_http_uploads(&state_path)
                .unwrap()
                .into_iter()
                .all(|record| record.upload_id != ready.upload_id)
        );
        fs::remove_dir_all(base).ok();
        fs::remove_file(state_path.with_extension("sqlite")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_file_http_upload_rejects_symlink_target_escape() {
        let base = temp_state_path("http-upload-target-symlink-base");
        let root = base.join("project");
        let outside_target = base.join("outside.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside_target, b"outside-original").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "http-upload-target-symlink-state.json",
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
        let target = root.join("escape.bin");
        std::os::unix::fs::symlink(&outside_target, &target).unwrap();
        let result = protocol.prepare_session_file_http_upload(
            &connection,
            SessionFileUploadPayload {
                session_id: created_payload.session_id,
                path: "escape.bin".to_owned(),
                size_bytes: 4,
            },
            device_id,
        );

        assert!(
            result.is_err(),
            "HTTP upload init 不能跟随 root 内 symlink 写到 root 外"
        );
        assert_eq!(fs::read(&outside_target).unwrap(), b"outside-original");
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(http_upload_temp_names(&root).is_empty());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_download_stream_stops_at_declared_size_when_file_grows() {
        let base = temp_state_path("stream-download-grow-base");
        let root = base.join("project");
        let target = root.join("artifact.bin");
        fs::create_dir_all(&root).unwrap();
        fs::write(&target, b"abc").unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "stream-download-grow-state.json",
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
        let (ready, mut stream) = protocol
            .prepare_session_file_download_stream(
                &connection,
                SessionFileDownloadStreamPayload {
                    session_id: created_payload.session_id,
                    path: "artifact.bin".to_owned(),
                },
            )
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .unwrap()
            .write_all(b"extra")
            .unwrap();

        let (chunk, eof) = protocol
            .read_session_file_download_stream_chunk(
                &mut stream,
                SESSION_FILE_TRANSFER_CHUNK_MAX_BYTES,
            )
            .unwrap();

        assert_eq!(ready.size_bytes, 3);
        assert_eq!(chunk.size_bytes, 3);
        assert_eq!(
            general_purpose::STANDARD.decode(chunk.data_base64).unwrap(),
            b"abc"
        );
        assert!(eof);
        assert!(chunk.eof);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn session_file_read_without_max_bytes_uses_daemon_memory_cap() {
        let base = temp_state_path("file-read-default-cap-base");
        let root = base.join("project");
        let target = root.join("large.txt");
        fs::create_dir_all(&root).unwrap();
        fs::File::create(&target)
            .unwrap()
            .set_len(SESSION_FILE_READ_MAX_BYTES + 1)
            .unwrap();
        let backend = FakePtyBackend::default();
        let mut config = DaemonConfig::default_for_state_path(temp_state_path(
            "file-read-default-cap-state.json",
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

        let result = protocol.read_session_file(
            &connection,
            SessionFileReadPayload {
                session_id: created_payload.session_id,
                path: "large.txt".to_owned(),
                max_bytes: None,
            },
        );

        assert!(matches!(result, Err(ProtocolError::InvalidEnvelope)));
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

    #[test]
    fn packet_file_transfer_stream_uploads_and_downloads_binary_chunks() {
        let base = temp_state_path("file-stream-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("file-stream-state.json"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session, _) =
            open_binary_packet_e2ee(&mut protocol, &mut connection, device_id);
        let token = protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .clone();
        let pair_responses = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::request(
                PacketRequestId::new(),
                METHOD_PAIR_REQUEST,
                serde_json::to_value(PairRequestPayload {
                    device_id,
                    device_public_key: public_key,
                    token,
                    nonce: nonce(),
                    timestamp_ms: UnixTimestampMillis(1_000),
                })
                .unwrap(),
            ),
        );
        let _ = decrypt_binary_packets(&mut device_session, pair_responses);
        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        let upload_stream_id = PacketStreamId::new();
        let upload_open = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                upload_stream_id,
                METHOD_SESSION_FILE_UPLOAD_STREAM,
                0,
                serde_json::to_value(SessionFileUploadPayload {
                    session_id,
                    path: "uploaded.bin".to_owned(),
                    size_bytes: 11,
                })
                .unwrap(),
            ),
        );
        let upload_ready = decrypt_binary_packets(&mut device_session, upload_open);
        assert_eq!(upload_ready[0].1.kind, PacketKind::Response);

        let upload_chunk = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_chunk(
                upload_stream_id,
                1,
                serde_json::to_value(SessionFileTransferChunkPayload {
                    session_id,
                    offset_bytes: 0,
                    data_base64: general_purpose::STANDARD.encode(b"hello file\n"),
                    size_bytes: 11,
                    eof: true,
                })
                .unwrap(),
            ),
        );
        let upload_progress = decrypt_binary_packets(&mut device_session, upload_chunk);
        assert!(
            matches!(
                upload_progress[0].0.payload,
                Some(binary_protocol_packet::Payload::Json(_))
            ),
            "upload progress is metadata only; file bytes must not be returned as base64"
        );
        assert_eq!(
            fs::read(root.join("uploaded.bin")).unwrap(),
            b"hello file\n"
        );

        let download_stream_id = PacketStreamId::new();
        let download_open = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                download_stream_id,
                METHOD_SESSION_FILE_DOWNLOAD_STREAM,
                0,
                serde_json::to_value(SessionFileDownloadStreamPayload {
                    session_id,
                    path: "uploaded.bin".to_owned(),
                })
                .unwrap(),
            ),
        );
        let download_ready = decrypt_binary_packets(&mut device_session, download_open);
        assert_eq!(download_ready[0].1.kind, PacketKind::Response);

        let download_chunk = send_binary_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::flow(download_stream_id, 0, 256 * 1024),
        );
        let download_packets = decrypt_binary_packets(&mut device_session, download_chunk);
        let Some(binary_protocol_packet::Payload::FileChunk(chunk)) =
            download_packets[0].0.payload.clone()
        else {
            panic!("download stream must return a binary file_chunk payload");
        };
        assert_eq!(chunk.data, b"hello file\n");
        assert_eq!(chunk.offset_bytes, 0);
        assert_eq!(chunk.size_bytes, 11);
        assert!(chunk.eof);

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
    fn session_file_write_rejects_payload_above_rpc_editor_cap() {
        let base = temp_state_path("write-large-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("write-large-state.json"));
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        let target = root.join("too-large.txt");
        let oversized = vec![b'x'; SESSION_FILE_WRITE_MAX_BYTES + 1];

        let write_responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionFileWrite,
                SessionFileWritePayload {
                    session_id,
                    path: target.to_string_lossy().to_string(),
                    data_base64: general_purpose::STANDARD.encode(oversized),
                },
            )
            .unwrap(),
        );
        let written = decrypt_first(&mut device_session, write_responses);

        assert_eq!(written.kind, MessageType::Error);
        assert!(!target.exists());
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
                    max_bytes: None,
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
                watch_updates: true,
                last_terminal_seq: None,
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
    fn raw_session_output_limits_each_flush_and_wakes_for_remaining_history() {
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

        for index in 0..(RAW_OUTPUT_BATCH_MAX_CHUNKS + 3) {
            backend.push_output(format!("chunk-{index:02}\n").into_bytes());
        }

        let first_flush =
            connection.read_session_output(&mut protocol, created_payload.session_id, 16);
        let first_chunks = first_flush
            .into_iter()
            .map(|response| {
                let inner = decrypt_first(&mut device_session, vec![response]);
                assert_eq!(inner.kind, MessageType::SessionData);
                let payload: SessionDataPayload = decode_payload(inner.payload).unwrap();
                general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            first_chunks.len(),
            RAW_OUTPUT_BATCH_MAX_CHUNKS,
            "raw/legacy 输出单轮不能把完整 backlog 一次性塞进 writer"
        );
        assert_eq!(
            connection.take_deferred_output_wakeups(),
            vec![created_payload.session_id],
            "仍有 raw history 或 pending chunk 时必须显式排下一轮 flush"
        );

        let second_flush =
            connection.read_session_output(&mut protocol, created_payload.session_id, 16);
        let mut all_output = Vec::new();
        for chunk in first_chunks {
            all_output.extend(chunk);
        }
        for response in second_flush {
            let inner = decrypt_first(&mut device_session, vec![response]);
            assert_eq!(inner.kind, MessageType::SessionData);
            let payload: SessionDataPayload = decode_payload(inner.payload).unwrap();
            all_output.extend(
                general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap(),
            );
        }

        let output = String::from_utf8(all_output).unwrap();
        assert!(output.contains("chunk-00"));
        assert!(output.contains("chunk-10"));
    }

    #[test]
    fn raw_pending_snapshot_is_split_by_max_bytes() {
        let (mut protocol, _) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let session_id = SessionId::new();
        let internal_session_id = session_id.0.to_string();
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

        protocol
            .session_index
            .insert(session_id, internal_session_id.clone());
        protocol.session_output_history_mut(session_id, TerminalSize::new(24, 80));
        connection.attach(
            session_id,
            0,
            b"abcdefghijklmnopqrstuvwxyz0123456789".to_vec(),
            true,
        );

        let messages = connection
            .try_drain_session_output_messages_for_push(&mut protocol, session_id, 4)
            .unwrap();
        let data_chunks = messages
            .into_iter()
            .map(|message| {
                assert_eq!(message.kind, MessageType::SessionData);
                let payload: SessionDataPayload = decode_payload(message.payload).unwrap();
                general_purpose::STANDARD
                    .decode(payload.data_base64)
                    .unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            data_chunks.first().map(Vec::as_slice),
            Some(b"abcd".as_slice())
        );
        assert!(
            data_chunks.iter().all(|chunk| chunk.len() <= 4),
            "pending snapshot chunk 必须按本轮 max_bytes 拆分"
        );
        assert_eq!(connection.take_deferred_output_wakeups(), vec![session_id]);
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
    fn session_output_history_drops_pre_clear_text_but_keeps_visible_background_rows() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(3, 8));

        // 中文注释：`clear` 之后的 snapshot/history 不应再把 pre-clear 内容带回来，
        // 但当前样式刷出来的空白行仍要被保留。
        history.append(b"alpha\nbeta\n\x1b[48;5;22m\x1b[H\x1b[2J");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(!snapshot.contains("alpha"));
        assert!(!snapshot.contains("beta"));
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
    fn session_output_history_resize_reflows_retained_output_to_new_width() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(4, 10));

        history.append(b"1234567890abcdefghij\r\n");
        let narrow_snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();
        assert!(narrow_snapshot.contains("1234567890\r\nabcdefghij"));

        history.resize(TerminalSize::new(4, 20));
        let wide_snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();
        assert!(wide_snapshot.contains("1234567890abcdefghij"));
        assert!(!wide_snapshot.contains("1234567890\r\nabcdefghij"));
    }

    #[test]
    fn session_output_history_snapshot_does_not_render_charset_designation_bytes_as_text() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(4, 20));

        history.append(b"\x1b(Bhello\r\n\x1b)0world");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();
        assert!(snapshot.contains("hello\r\nworld"));
        assert!(!snapshot.contains("Bhello"));
        assert!(!snapshot.contains("0world"));
    }

    #[test]
    fn session_output_history_drops_status_block_before_screen_redraw() {
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
        // 中文注释：`clear` 之后重新 attach/scrollback 时，不应再看到 pre-clear 状态块。
        history.append(b"\n\n\n\n\n\n\x1b[1;1H\x1b[2Jvisible after redraw");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(!snapshot.contains("当前状态"));
        assert!(!snapshot.contains("工作区干净"));
        assert!(!snapshot.contains("4b70e91a"));
        assert!(!snapshot.contains("go test"));
        assert!(snapshot.contains("visible after redraw"));
    }

    #[test]
    fn session_output_history_drops_visible_status_block_when_plain_screen_clears() {
        let mut history = SessionOutputHistory::new(TerminalSize::new(8, 100));

        // 中文注释：即使状态块还在 clear 前的可见 viewport 内，clear 也应该把它切掉，
        // 避免用户后续 scroll/full snapshot 时又看到旧屏内容。
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
        history.append(b"\x1b[H\x1b[2J\x1b[48;5;236mSummarize recent commits");

        let snapshot = String::from_utf8(history.snapshot_bytes()).unwrap();

        assert!(!snapshot.contains("当前状态"));
        assert!(!snapshot.contains("工作区干净"));
        assert!(!snapshot.contains("4b70e91a"));
        assert!(!snapshot.contains("go test"));
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
    fn permission_only_attach_can_use_session_rpc_without_subscribing_to_output() {
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
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        backend.push_output_for_session(created_payload.session_id, b"busy output\n".to_vec());

        let (mut rpc_connection, _) = protocol.start_connection();
        let mut rpc_device_session = authenticate_paired_connection(
            &mut protocol,
            &mut rpc_connection,
            device_id,
            &signing_key,
        );
        let attached = send_encrypted(
            &mut protocol,
            &mut rpc_connection,
            &mut rpc_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut rpc_device_session, attached);
        let attached_payload: SessionAttachedPayload = decode_payload(attached.payload).unwrap();

        assert_eq!(attached.kind, MessageType::SessionAttached);
        assert!(!attached_payload.resize_owner);
        assert!(rpc_connection.attached_output_signals(&protocol).is_empty());
        assert!(rpc_connection.attached_resize_signals(&protocol).is_empty());
        assert!(
            rpc_connection
                .session_activity_signals(&protocol)
                .is_empty()
        );
        assert!(
            rpc_connection
                .read_attached_outputs(&mut protocol, 4096)
                .is_empty()
        );

        // 短连接仍然拥有 session 级权限，可以查询文件；只是不会接收终端输出洪峰。
        let files_responses = send_encrypted(
            &mut protocol,
            &mut rpc_connection,
            &mut rpc_device_session,
            envelope_value(
                MessageType::SessionFiles,
                SessionFilesPayload {
                    session_id: created_payload.session_id,
                    path: None,
                },
            )
            .unwrap(),
        );
        let files = decrypt_first(&mut rpc_device_session, files_responses);
        assert_eq!(files.kind, MessageType::SessionFilesResult);
    }

    #[test]
    fn permission_only_attach_does_not_start_watched_attachment_handle() {
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
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();
        assert_eq!(backend.attachment_starts().len(), 1);

        let (mut rpc_connection, _) = protocol.start_connection();
        let mut rpc_device_session = authenticate_paired_connection(
            &mut protocol,
            &mut rpc_connection,
            device_id,
            &signing_key,
        );
        let attached = send_encrypted(
            &mut protocol,
            &mut rpc_connection,
            &mut rpc_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: created_payload.session_id,
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut rpc_device_session, attached);

        assert_eq!(attached.kind, MessageType::SessionAttached);
        assert_eq!(
            backend.attachment_starts().len(),
            1,
            "permission-only attach 不能创建 terminal watcher 的独立 attach handle"
        );
        assert!(backend.attachment_drops().is_empty());
    }

    #[test]
    fn permission_only_attach_keeps_device_operator_until_last_same_device_connection_closes() {
        let (mut protocol, backend) = protocol();
        let (mut creator_connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut creator_device_session) =
            open_e2ee(&mut protocol, &mut creator_connection, device_id);
        pair_device(
            &mut protocol,
            &mut creator_connection,
            &mut creator_device_session,
            device_id,
            public_key,
        );
        let session_id = create_test_session(
            &mut protocol,
            &mut creator_connection,
            &mut creator_device_session,
        );
        creator_connection.close(&mut protocol);

        let (mut rpc_connection, _) = protocol.start_connection();
        let mut rpc_device_session = authenticate_paired_connection(
            &mut protocol,
            &mut rpc_connection,
            device_id,
            &signing_key,
        );
        let attached = send_encrypted(
            &mut protocol,
            &mut rpc_connection,
            &mut rpc_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id,
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        assert_eq!(
            decrypt_first(&mut rpc_device_session, attached).kind,
            MessageType::SessionAttached
        );

        // 中文注释：这里模拟同一设备的另一条短连接也曾 attach 同一 session。
        // 关闭它时不能因为 runtime 是设备级 operator，就把仍在线的 permission-only
        // WebSocket 连接权限一起撤掉。
        let mut scoped_http = ProtocolConnection::authenticated_http(device_id);
        protocol
            .attach_session_permission(
                &mut scoped_http,
                SessionAttachPayload {
                    session_id,
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap();
        protocol.detach_connection(&mut scoped_http);

        let write = send_encrypted(
            &mut protocol,
            &mut rpc_connection,
            &mut rpc_device_session,
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id,
                    data_base64: general_purpose::STANDARD.encode(b"rpc-still-attached\n"),
                },
            )
            .unwrap(),
        );

        assert!(
            write.is_empty(),
            "permission-only WebSocket 连接仍 attached 时，session_data 不应返回错误"
        );
        assert_eq!(backend.writes(), vec![b"rpc-still-attached\n".to_vec()]);
    }

    #[test]
    fn watched_connection_close_drops_only_its_attachment_handle() {
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
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap(),
        );
        let created = decrypt_first(&mut first_device_session, created_responses);
        let created_payload: SessionCreatedPayload = decode_payload(created.payload).unwrap();

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
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let attached = decrypt_first(&mut second_device_session, attached);
        assert_eq!(attached.kind, MessageType::SessionAttached);
        assert_eq!(backend.attachment_starts().len(), 2);

        protocol.detach_connection(&mut second_connection);

        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "关闭第二条 watched 连接只能释放它自己的 attachment"
        );
        assert_eq!(backend.terminate_count(), 0);
        send_encrypted(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            envelope_value(
                MessageType::SessionData,
                SessionDataPayload {
                    session_id: created_payload.session_id,
                    data_base64: general_purpose::STANDARD.encode(b"first-still-attached\n"),
                },
            )
            .unwrap(),
        );
        assert_eq!(backend.writes(), vec![b"first-still-attached\n".to_vec()]);
    }

    #[test]
    fn packet_terminal_stream_replacement_drops_previous_watched_attachment() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let first_stream = PacketStreamId::new();
        let first = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                first_stream,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, first).kind,
            PacketKind::Response
        );
        assert_eq!(backend.attachment_starts().len(), 1);
        assert!(backend.attachment_drops().is_empty());

        let second_stream = PacketStreamId::new();
        let second = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                second_stream,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, second).kind,
            PacketKind::Response
        );

        assert_eq!(backend.attachment_starts().len(), 2);
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "terminal stream 替换成功后必须释放旧 watched attachment"
        );
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn packet_terminal_attach_replaces_session_create_watched_attachment() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let session_id =
            create_test_packet_session(&mut protocol, &mut connection, &mut device_session);
        assert_eq!(backend.attachment_starts().len(), 1);
        assert!(backend.attachment_drops().is_empty());

        let stream_id = PacketStreamId::new();
        let attached = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_ATTACH,
                128 * 1024,
                serde_json::to_value(SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, attached).kind,
            PacketKind::Response
        );

        assert_eq!(backend.attachment_starts().len(), 2);
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "terminal stream attach 必须替换 packet session.create 留下的旧 watched handle"
        );
    }

    #[test]
    fn packet_terminal_stream_cancel_drops_watched_attachment() {
        let (mut protocol, backend) = protocol();
        let (mut connection, _) = protocol.start_connection();
        let device_id = DeviceId::new();
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = PublicKey(wire(signing_key.verifying_key().as_bytes()));
        let (_, mut device_session, _) =
            open_packet_e2ee(&mut protocol, &mut connection, device_id);
        pair_packet_device(
            &mut protocol,
            &mut connection,
            &mut device_session,
            device_id,
            public_key,
        );

        let stream_id = PacketStreamId::new();
        let opened = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::stream_open(
                PacketRequestId::new(),
                stream_id,
                METHOD_TERMINAL_CREATE,
                128 * 1024,
                serde_json::to_value(SessionCreatePayload {
                    command: vec!["sh".to_owned()],
                    size: TerminalSize::new(24, 80),
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            decrypt_first_packet(&mut device_session, opened).kind,
            PacketKind::Response
        );
        assert_eq!(backend.attachment_starts().len(), 1);

        let canceled = send_encrypted_packet(
            &mut protocol,
            &mut connection,
            &mut device_session,
            ProtocolPacket::cancel_stream(stream_id, serde_json::json!({"reason": "test"})),
        );

        assert!(canceled.is_empty());
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "terminal stream cancel 必须释放当前 watched attachment"
        );
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn attach_session_rolls_back_runtime_attach_when_late_step_fails() {
        let (mut protocol, backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let first_device_id = DeviceId::new();
        let first_signing_key = SigningKey::generate(&mut OsRng);
        let first_public_key = PublicKey(wire(first_signing_key.verifying_key().as_bytes()));
        let (_, mut first_device_session) =
            open_e2ee(&mut protocol, &mut first_connection, first_device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            first_device_id,
            first_public_key,
        );
        let session_id = create_test_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        let internal_session_id = protocol.session_index.get(&session_id).cloned().unwrap();

        let (mut second_connection, _) = protocol.start_connection();
        let second_device_id = DeviceId::new();
        let second_signing_key = SigningKey::generate(&mut OsRng);
        let second_public_key = PublicKey(wire(second_signing_key.verifying_key().as_bytes()));
        let (_, mut second_device_session) =
            open_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            second_public_key,
        );

        backend.fail_reads("forced attach drain failure");
        let responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let response = decrypt_first(&mut second_device_session, responses);
        backend.allow_reads();

        assert_eq!(response.kind, MessageType::Error);
        assert!(
            protocol
                .runtime
                .role(&internal_session_id, &device_key(second_device_id))
                .unwrap()
                .is_none()
        );
        assert_eq!(second_connection.debug_snapshot().attached_sessions, 0);
    }

    #[test]
    fn permission_attach_rolls_back_runtime_attach_when_history_write_fails() {
        let (mut protocol, _backend) = protocol();
        let (mut first_connection, _) = protocol.start_connection();
        let first_device_id = DeviceId::new();
        let first_signing_key = SigningKey::generate(&mut OsRng);
        let first_public_key = PublicKey(wire(first_signing_key.verifying_key().as_bytes()));
        let (_, mut first_device_session) =
            open_e2ee(&mut protocol, &mut first_connection, first_device_id);
        pair_device(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
            first_device_id,
            first_public_key,
        );
        let session_id = create_test_session(
            &mut protocol,
            &mut first_connection,
            &mut first_device_session,
        );
        let internal_session_id = protocol.session_index.get(&session_id).cloned().unwrap();

        let (mut second_connection, _) = protocol.start_connection();
        let second_device_id = DeviceId::new();
        let second_signing_key = SigningKey::generate(&mut OsRng);
        let second_public_key = PublicKey(wire(second_signing_key.verifying_key().as_bytes()));
        let (_, mut second_device_session) =
            open_e2ee(&mut protocol, &mut second_connection, second_device_id);
        pair_device(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            second_device_id,
            second_public_key,
        );

        protocol
            .client_history
            .set_query_only_for_test(true)
            .unwrap();
        let responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id,
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let response = decrypt_first(&mut second_device_session, responses);
        protocol
            .client_history
            .set_query_only_for_test(false)
            .unwrap();

        assert_eq!(response.kind, MessageType::Error);
        assert!(
            protocol
                .runtime
                .role(&internal_session_id, &device_key(second_device_id))
                .unwrap()
                .is_none(),
            "permission attach 的后置历史写入失败不能泄漏 runtime operator 角色"
        );
        assert_eq!(second_connection.debug_snapshot().attached_sessions, 0);
    }

    #[test]
    fn permission_attach_unknown_session_does_not_issue_scope_grant() {
        let (mut protocol, _backend) = protocol();
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
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: SessionId::new(),
                    watch_updates: false,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let response = decrypt_first(&mut device_session, responses);

        assert_eq!(response.kind, MessageType::Error);
        assert!(
            format!("{:?}", protocol.session_scope_manager).contains("len: 0"),
            "不存在的 session attach 失败时不应留下内部 scope token"
        );
    }

    #[test]
    fn terminal_attach_unknown_session_does_not_issue_scope_grant() {
        let (mut protocol, _backend) = protocol();
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
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id: SessionId::new(),
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let response = decrypt_first(&mut device_session, responses);

        assert_eq!(response.kind, MessageType::Error);
        assert!(
            format!("{:?}", protocol.session_scope_manager).contains("len: 0"),
            "不存在的 terminal attach 失败时不应留下内部 scope token"
        );
    }

    #[test]
    fn session_create_scope_grant_failure_rolls_back_connection_attach_state() {
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

        // 中文注释：pairing 已完成后再把 TTL 置零，专门模拟 create 最后的 scope grant
        // 签发失败；此时 runtime session 和 watched attachment 都已经创建过。
        protocol.config.pairing_token_ttl_ms = 0;
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
        let response = decrypt_first(&mut device_session, responses);
        let snapshot = connection.debug_snapshot();

        assert_eq!(response.kind, MessageType::Error);
        assert_eq!(snapshot.attached_sessions, 0);
        assert_eq!(snapshot.watched_sessions, 0);
        assert_eq!(backend.terminate_count(), 1);
        assert_eq!(backend.attachment_drops().len(), 1);
    }

    #[test]
    fn terminal_reattach_scope_grant_failure_keeps_previous_watched_attachment() {
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
        let session_id = create_test_session(&mut protocol, &mut connection, &mut device_session);
        assert_eq!(backend.attachment_starts().len(), 1);
        assert_eq!(connection.attached_output_signals(&protocol).len(), 1);

        // 中文注释：同一连接再次 terminal attach 会先启动新 watcher。这里让最后的
        // scope grant 签发失败，验证失败路径不能把旧 watcher 一起丢掉。
        protocol.config.pairing_token_ttl_ms = 0;
        let responses = send_encrypted(
            &mut protocol,
            &mut connection,
            &mut device_session,
            envelope_value(
                MessageType::SessionAttach,
                SessionAttachPayload {
                    session_id,
                    watch_updates: true,
                    last_terminal_seq: None,
                },
            )
            .unwrap(),
        );
        let response = decrypt_first(&mut device_session, responses);
        let snapshot = connection.debug_snapshot();

        assert_eq!(response.kind, MessageType::Error);
        assert_eq!(backend.attachment_starts().len(), 2);
        assert_eq!(
            backend.attachment_drops().len(),
            1,
            "失败的新 watcher 应被释放，但旧 watcher 必须保留"
        );
        assert_eq!(snapshot.attached_sessions, 1);
        assert_eq!(snapshot.watched_sessions, 1);
        assert_eq!(connection.attached_output_signals(&protocol).len(), 1);
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
                    watch_updates: true,
                    last_terminal_seq: None,
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
    fn authenticated_but_unattached_connection_cannot_close_attached_session() {
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

        // close 是 session 级破坏性操作，不能只凭同设备认证状态绕过当前连接的 attach 关系。
        let close_responses = send_encrypted(
            &mut protocol,
            &mut second_connection,
            &mut second_crypto,
            envelope_value(
                MessageType::SessionClose,
                SessionClosePayload {
                    session_id: created_payload.session_id,
                },
            )
            .unwrap(),
        );
        let close_error = decrypt_first(&mut second_crypto, close_responses);
        assert_eq!(close_error.kind, MessageType::Error);
        let close_payload: ErrorPayload = decode_payload(close_error.payload).unwrap();
        assert_eq!(close_payload.code, "invalid_state");

        let state = StateStore::load(&protocol.config.state_path).unwrap();
        let persisted_session = state
            .sessions
            .iter()
            .find(|session| session.session_id == created_payload.session_id)
            .expect("unattached close must not remove the backing session");
        assert_eq!(persisted_session.state, SessionState::Running);
        assert_eq!(backend.terminate_count(), 0);
    }

    #[derive(Debug, Clone, Copy)]
    enum SessionScopedConnectionCase {
        Unauthenticated,
        AuthenticatedUnattached,
        AttachedWrongSession,
        AttachedCorrectSession,
    }

    impl SessionScopedConnectionCase {
        fn expected_error_code(self) -> Option<&'static str> {
            match self {
                Self::Unauthenticated => Some("unauthenticated"),
                Self::AuthenticatedUnattached | Self::AttachedWrongSession => Some("invalid_state"),
                Self::AttachedCorrectSession => None,
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum SessionScopedMutatingHandler {
        WriteData,
        Resize,
        Cursor,
        Rename,
        Control,
        FileWrite,
        FileDelete,
        FileUploadPrepare,
        FileHttpUploadPrepare,
        FileHttpUploadBegin,
        FileHttpUploadAbort,
        FileHttpDownloadPrepare,
        FileDownloadStreamPrepare,
        FileDownloadPrepare,
        FileDownloadChunk,
        GitAction,
        Close,
    }

    impl SessionScopedMutatingHandler {
        fn all() -> &'static [Self] {
            &[
                Self::WriteData,
                Self::Resize,
                Self::Cursor,
                Self::Rename,
                Self::Control,
                Self::FileWrite,
                Self::FileDelete,
                Self::FileUploadPrepare,
                Self::FileHttpUploadPrepare,
                Self::FileHttpUploadBegin,
                Self::FileHttpUploadAbort,
                Self::FileHttpDownloadPrepare,
                Self::FileDownloadStreamPrepare,
                Self::FileDownloadPrepare,
                Self::FileDownloadChunk,
                Self::GitAction,
                Self::Close,
            ]
        }

        fn needs_git(self) -> bool {
            matches!(self, Self::GitAction)
        }

        fn needs_existing_http_upload(self) -> bool {
            matches!(self, Self::FileHttpUploadBegin | Self::FileHttpUploadAbort)
        }

        fn invoke(
            self,
            fixture: &mut SessionScopedMutatingFixture,
            connection: &ProtocolConnection,
        ) -> Result<(), ProtocolError> {
            match self {
                Self::WriteData => fixture
                    .protocol
                    .write_session_data(
                        connection,
                        SessionDataPayload {
                            session_id: fixture.target_session_id,
                            data_base64: general_purpose::STANDARD.encode(b"scoped input\n"),
                        },
                    )
                    .map(drop),
                Self::Resize => fixture
                    .protocol
                    .resize_session(
                        connection,
                        SessionResizePayload {
                            session_id: fixture.target_session_id,
                            size: TerminalSize::new(40, 120),
                        },
                    )
                    .map(drop),
                Self::Cursor => fixture
                    .protocol
                    .record_session_cursor(
                        connection,
                        SessionCursorPayload {
                            session_id: fixture.target_session_id,
                            row: 2,
                            col: 3,
                            focused: true,
                        },
                    )
                    .map(drop),
                Self::Rename => fixture
                    .protocol
                    .rename_session(
                        connection,
                        SessionRenamePayload {
                            session_id: fixture.target_session_id,
                            name: "scoped rename".to_owned(),
                        },
                    )
                    .map(drop),
                Self::Control => fixture
                    .protocol
                    .request_control(
                        connection,
                        ControlRequestPayload {
                            session_id: fixture.target_session_id,
                            device_id: fixture.device_id,
                        },
                    )
                    .map(drop),
                Self::FileWrite => fixture
                    .protocol
                    .write_session_file(
                        connection,
                        SessionFileWritePayload {
                            session_id: fixture.target_session_id,
                            path: "write.txt".to_owned(),
                            data_base64: general_purpose::STANDARD.encode(b"scoped file\n"),
                        },
                    )
                    .map(drop),
                Self::FileDelete => fixture
                    .protocol
                    .delete_session_file(
                        connection,
                        SessionFileDeletePayload {
                            session_id: fixture.target_session_id,
                            path: "delete-me.txt".to_owned(),
                        },
                    )
                    .map(drop),
                Self::FileUploadPrepare => {
                    let result = fixture.protocol.prepare_session_file_upload_stream(
                        connection,
                        SessionFileUploadPayload {
                            session_id: fixture.target_session_id,
                            path: "stream-upload.bin".to_owned(),
                            size_bytes: 6,
                        },
                    );
                    match result {
                        Ok((_, stream)) => {
                            cleanup_upload_temp(&stream);
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                Self::FileHttpUploadPrepare => fixture
                    .protocol
                    .prepare_session_file_http_upload(
                        connection,
                        SessionFileUploadPayload {
                            session_id: fixture.target_session_id,
                            path: "http-upload-prepare.bin".to_owned(),
                            size_bytes: 6,
                        },
                        fixture.device_id,
                    )
                    .map(drop),
                Self::FileHttpUploadBegin => {
                    let payload = fixture
                        .http_upload_payload
                        .clone()
                        .expect("HTTP upload begin fixture should create upload state");
                    fixture
                        .protocol
                        .begin_session_file_http_upload_write(
                            connection,
                            payload,
                            fixture.device_id,
                            3,
                        )
                        .map(drop)
                }
                Self::FileHttpUploadAbort => {
                    let payload = fixture
                        .http_upload_payload
                        .clone()
                        .expect("HTTP upload abort fixture should create upload state");
                    fixture
                        .protocol
                        .abort_session_file_http_upload(connection, &payload)
                }
                Self::FileHttpDownloadPrepare => fixture
                    .protocol
                    .prepare_session_file_http_download(
                        connection,
                        SessionFileHttpDownloadPayload {
                            session_id: fixture.target_session_id,
                            path: "download.txt".to_owned(),
                            offset_bytes: 0,
                        },
                    )
                    .map(drop),
                Self::FileDownloadStreamPrepare => fixture
                    .protocol
                    .prepare_session_file_download_stream(
                        connection,
                        SessionFileDownloadStreamPayload {
                            session_id: fixture.target_session_id,
                            path: "download.txt".to_owned(),
                        },
                    )
                    .map(drop),
                Self::FileDownloadPrepare => fixture
                    .protocol
                    .prepare_session_file_download(
                        connection,
                        SessionFileDownloadPreparePayload {
                            session_id: fixture.target_session_id,
                            path: "download.txt".to_owned(),
                        },
                    )
                    .map(drop),
                Self::FileDownloadChunk => fixture
                    .protocol
                    .read_session_file_download_chunk(
                        connection,
                        SessionFileDownloadChunkPayload {
                            session_id: fixture.target_session_id,
                            path: "download.txt".to_owned(),
                            offset_bytes: 0,
                            max_bytes: 8,
                        },
                    )
                    .map(drop),
                Self::GitAction => fixture
                    .protocol
                    .apply_session_git_action(
                        connection,
                        SessionGitActionPayload {
                            session_id: fixture.target_session_id,
                            worktree_path: fixture.root.to_string_lossy().to_string(),
                            file_path: "tracked.txt".to_owned(),
                            action: SessionGitActionKind::Stage,
                        },
                    )
                    .map(drop),
                Self::Close => fixture
                    .protocol
                    .close_session(
                        connection,
                        SessionClosePayload {
                            session_id: fixture.target_session_id,
                        },
                    )
                    .map(drop),
            }
        }
    }

    struct SessionScopedMutatingFixture {
        protocol: DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        backend: FakePtyBackend,
        device_id: DeviceId,
        target_session_id: SessionId,
        other_session_id: SessionId,
        root: PathBuf,
        http_upload_payload: Option<SessionFileHttpUploadStreamPayload>,
    }

    impl SessionScopedMutatingFixture {
        fn connection_for(&self, case: SessionScopedConnectionCase) -> ProtocolConnection {
            match case {
                SessionScopedConnectionCase::Unauthenticated => ProtocolConnection::new(None),
                SessionScopedConnectionCase::AuthenticatedUnattached => {
                    ProtocolConnection::authenticated_http(self.device_id)
                }
                SessionScopedConnectionCase::AttachedWrongSession => {
                    let mut connection = ProtocolConnection::authenticated_http(self.device_id);
                    connection.attach(self.other_session_id, 0, Vec::new(), false);
                    connection
                }
                SessionScopedConnectionCase::AttachedCorrectSession => {
                    let mut connection = ProtocolConnection::authenticated_http(self.device_id);
                    connection.attach(self.target_session_id, 0, Vec::new(), false);
                    connection
                }
            }
        }

        fn assert_rejected_without_mutation(&self, handler: SessionScopedMutatingHandler) {
            match handler {
                SessionScopedMutatingHandler::WriteData => {
                    assert!(
                        self.backend.writes().is_empty(),
                        "rejected session data must not reach PTY"
                    );
                }
                SessionScopedMutatingHandler::FileWrite => {
                    assert!(
                        !self.root.join("write.txt").exists(),
                        "rejected file write must not create the target"
                    );
                }
                SessionScopedMutatingHandler::FileDelete => {
                    assert!(
                        self.root.join("delete-me.txt").exists(),
                        "rejected file delete must leave the target in place"
                    );
                }
                SessionScopedMutatingHandler::FileHttpUploadPrepare => {
                    assert!(
                        !self.root.join("http-upload-prepare.bin").exists(),
                        "rejected HTTP upload prepare must not create the target"
                    );
                }
                SessionScopedMutatingHandler::Close => {
                    assert_eq!(
                        self.backend.terminate_count(),
                        0,
                        "rejected close must not terminate the PTY"
                    );
                }
                SessionScopedMutatingHandler::Resize
                | SessionScopedMutatingHandler::Cursor
                | SessionScopedMutatingHandler::Rename
                | SessionScopedMutatingHandler::Control
                | SessionScopedMutatingHandler::FileUploadPrepare
                | SessionScopedMutatingHandler::FileHttpUploadBegin
                | SessionScopedMutatingHandler::FileHttpUploadAbort
                | SessionScopedMutatingHandler::FileHttpDownloadPrepare
                | SessionScopedMutatingHandler::FileDownloadStreamPrepare
                | SessionScopedMutatingHandler::FileDownloadPrepare
                | SessionScopedMutatingHandler::FileDownloadChunk
                | SessionScopedMutatingHandler::GitAction => {}
            }
        }
    }

    fn session_scoped_mutating_fixture(
        handler: SessionScopedMutatingHandler,
    ) -> SessionScopedMutatingFixture {
        let base = temp_state_path("session-scoped-mutating-base");
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("delete-me.txt"), b"delete target\n").unwrap();
        fs::write(root.join("download.txt"), b"download target\n").unwrap();
        fs::write(root.join("tracked.txt"), b"initial\n").unwrap();
        if handler.needs_git() {
            run_test_git(&root, &["init", "-b", "main"]);
            run_test_git(&root, &["config", "user.email", "test@example.com"]);
            run_test_git(&root, &["config", "user.name", "Termd Test"]);
            run_test_git(&root, &["add", "tracked.txt"]);
            run_test_git(&root, &["commit", "-m", "initial"]);
            fs::write(root.join("tracked.txt"), b"initial\nchanged\n").unwrap();
        }

        let backend = FakePtyBackend::default();
        let mut config =
            DaemonConfig::default_for_state_path(temp_state_path("session-scoped-mutating-state"));
        config.default_command = vec!["sh".to_owned()];
        config.default_working_directory = Some(root.clone());
        let mut protocol =
            DaemonProtocol::new(config, backend.clone(), Ed25519SignatureVerifier).unwrap();
        let device_id = DeviceId::new();
        let mut controller = ProtocolConnection::authenticated_http(device_id);

        // 中文注释：两个 session 共享同一 device runtime 角色，测试连接只能通过
        // ProtocolConnection.attached_sessions 声明自己的 session 作用域。
        let target_session_id = create_test_session_direct(&mut protocol, &mut controller);
        let other_session_id = create_test_session_direct(&mut protocol, &mut controller);
        let http_upload_payload = if handler.needs_existing_http_upload() {
            let ready = protocol
                .prepare_session_file_http_upload(
                    &controller,
                    SessionFileUploadPayload {
                        session_id: target_session_id,
                        path: "http-upload-active.bin".to_owned(),
                        size_bytes: 6,
                    },
                    device_id,
                )
                .unwrap();
            Some(SessionFileHttpUploadStreamPayload {
                session_id: target_session_id,
                path: ready.path,
                upload_id: ready.upload_id,
                size_bytes: ready.size_bytes,
                offset_bytes: 0,
            })
        } else {
            None
        };

        SessionScopedMutatingFixture {
            protocol,
            backend,
            device_id,
            target_session_id,
            other_session_id,
            root,
            http_upload_payload,
        }
    }

    fn create_test_session_direct(
        protocol: &mut DaemonProtocol<FakePtyBackend, Ed25519SignatureVerifier>,
        connection: &mut ProtocolConnection,
    ) -> SessionId {
        let responses = protocol
            .create_session(
                connection,
                SessionCreatePayload {
                    command: Vec::new(),
                    size: TerminalSize::new(24, 80),
                },
            )
            .unwrap();
        let created: SessionCreatedPayload =
            decode_payload(responses.into_iter().next().unwrap().payload).unwrap();
        created.session_id
    }

    #[test]
    fn session_scoped_mutating_handlers_enforce_connection_scope_table() {
        let cases = [
            SessionScopedConnectionCase::Unauthenticated,
            SessionScopedConnectionCase::AuthenticatedUnattached,
            SessionScopedConnectionCase::AttachedWrongSession,
            SessionScopedConnectionCase::AttachedCorrectSession,
        ];

        for handler in SessionScopedMutatingHandler::all() {
            for case in cases {
                let mut fixture = session_scoped_mutating_fixture(*handler);
                let connection = fixture.connection_for(case);
                let result = handler.invoke(&mut fixture, &connection);

                match case.expected_error_code() {
                    Some(expected_code) => {
                        let Err(error) = result else {
                            panic!("{handler:?} unexpectedly accepted {case:?}");
                        };
                        assert_eq!(
                            error.code(),
                            expected_code,
                            "{handler:?} returned wrong error for {case:?}"
                        );
                        fixture.assert_rejected_without_mutation(*handler);
                    }
                    None => {
                        result.unwrap_or_else(|error| {
                            panic!(
                                "{handler:?} rejected attached correct session with {}",
                                error.code()
                            )
                        });
                    }
                }
            }
        }
    }

    #[test]
    fn session_scoped_unknown_session_preserves_handler_error_order() {
        let mut fixture = session_scoped_mutating_fixture(SessionScopedMutatingHandler::Rename);
        let connection = ProtocolConnection::authenticated_http(fixture.device_id);
        let unknown_session_id = SessionId::new();

        let rename = fixture.protocol.rename_session(
            &connection,
            SessionRenamePayload {
                session_id: unknown_session_id,
                name: "unknown".to_owned(),
            },
        );
        assert_eq!(rename.unwrap_err().code(), "invalid_state");

        let files = fixture.protocol.list_session_files(
            &connection,
            SessionFilesPayload {
                session_id: unknown_session_id,
                path: None,
            },
        );
        assert_eq!(files.unwrap_err().code(), "invalid_state");

        let git = fixture.protocol.list_session_git(
            &connection,
            SessionGitPayload {
                session_id: unknown_session_id,
            },
        );
        assert_eq!(git.unwrap_err().code(), "invalid_state");

        let close = fixture.protocol.close_session(
            &connection,
            SessionClosePayload {
                session_id: unknown_session_id,
            },
        );
        assert_eq!(close.unwrap_err().code(), "invalid_state");

        let write = fixture.protocol.write_session_data(
            &connection,
            SessionDataPayload {
                session_id: unknown_session_id,
                data_base64: general_purpose::STANDARD.encode(b"unknown\n"),
            },
        );
        assert_eq!(write.unwrap_err().code(), "session_not_found");

        let resize = fixture.protocol.resize_session(
            &connection,
            SessionResizePayload {
                session_id: unknown_session_id,
                size: TerminalSize::new(40, 120),
            },
        );
        assert_eq!(resize.unwrap_err().code(), "session_not_found");
    }

    #[test]
    fn session_scoped_control_request_rejects_wrong_payload_device_before_session_scope() {
        let mut fixture = session_scoped_mutating_fixture(SessionScopedMutatingHandler::Control);
        let connection = ProtocolConnection::authenticated_http(fixture.device_id);
        let wrong_device_id = DeviceId::new();
        let unknown_session_id = SessionId::new();

        let control = fixture.protocol.request_control(
            &connection,
            ControlRequestPayload {
                session_id: unknown_session_id,
                device_id: wrong_device_id,
            },
        );
        assert_eq!(control.unwrap_err().code(), "invalid_envelope");
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
        assert!(closed_state.sessions.is_empty());

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
    fn explicit_close_prunes_successfully_closed_session_rows() {
        let backend = FakePtyBackend::default();
        let state_path = temp_state_path("explicit-close-prunes-closed-rows.json");
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

        let state = StateStore::load(&state_path).unwrap();
        assert!(state.sessions.is_empty());
        let history = ClientHistoryStore::open(&state_path).unwrap();
        assert!(
            history
                .session_record_including_closed(created_payload.session_id)
                .unwrap()
                .is_none()
        );
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
