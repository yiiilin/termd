//! 每个 session 一个独立 supervisor 的 PTY backend。
//!
//! daemon 主进程不再直接持有真实 PTY；它只通过 Unix socket 和 session supervisor 通信。
//! supervisor 进程继续使用 termd 当前二进制启动，并在自己的进程空间里托管 PTY、
//! 保留最近输出快照，以及在 daemon 重启后接受新的 attach。

#[cfg(not(unix))]
compile_error!("the persistent session supervisor backend is supported only on Unix platforms");

mod terminal_journal;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc as tokio_mpsc, oneshot, watch};

use crate::net::pty_bridge::NonBlockingPortablePtyBackend;

use self::terminal_journal::{SupervisorTerminalCache, SupervisorTerminalMirror};
#[cfg(test)]
use self::terminal_journal::{TERMINAL_ATTACH_TAIL_MAX_BYTES, TERMINAL_JOURNAL_MAX_EVENTS};
use super::{
    CommandSpec, PtyAttachment, PtyAttachmentBootstrap, PtyBackend, PtyError, PtyRestoreInfo,
    PtyResult, PtySession, PtySize, PtySnapshot, PtyStartupGrant, PtySupervisorStatus,
    PtyTerminalFrame,
};

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RECONCILE_IPC_TIMEOUT: Duration = Duration::from_millis(200);
const SUPERVISOR_SOCKET_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_SIGNAL_INIT: u64 = 0;
// Supervisor frame 必须为 16 MiB WebSocket transport 保留外层编码空间。
pub(crate) const MAX_SUPERVISOR_FRAME_BYTES: usize = 8 * 1024 * 1024;
const SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES: usize = 128;
const SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES: usize = MAX_SUPERVISOR_FRAME_BYTES;
const SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES: usize = 64 * 1024;
const SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK: usize = 64;
const SUPERVISOR_OUTPUT_PUMP_MAX_BYTES_PER_TICK: usize = 4 * 1024 * 1024;
const SUPERVISOR_TERMINAL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const SUPERVISOR_TERMINAL_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;
const SUPERVISOR_RUNTIME_DIR_MODE: u32 = 0o700;
const SUPERVISOR_SOCKET_MODE: u32 = 0o600;
const SUPERVISOR_SOCKET_TEMP_RANDOM_BYTES: usize = 12;
const SUPERVISOR_SOCKET_TEMP_BIND_ATTEMPTS: usize = 8;
const CLEANUP_CAPABILITY_BYTES: usize = 32;
const STARTUP_GRANT_MAGIC: &[u8; 8] = b"TMDGRT01";
const STARTUP_GRANT_BYTES: usize = 8 + 16 + CLEANUP_CAPABILITY_BYTES + 32 + 32 + 4;
const CLEANUP_AUTH_NONCE_BYTES: usize = 32;
struct ValidatedStartupGrant {
    cleanup_capability: [u8; CLEANUP_CAPABILITY_BYTES],
}

fn startup_binding_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn encode_startup_grant(
    grant: &PtyStartupGrant,
    session_id: &str,
    socket_path: &Path,
    supervisor_pid: u32,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(STARTUP_GRANT_BYTES);
    frame.extend_from_slice(STARTUP_GRANT_MAGIC);
    frame.extend_from_slice(grant.create_operation_id());
    frame.extend_from_slice(grant.capability());
    frame.extend_from_slice(&startup_binding_hash(session_id.as_bytes()));
    frame.extend_from_slice(&startup_binding_hash(socket_path.as_os_str().as_bytes()));
    frame.extend_from_slice(&supervisor_pid.to_be_bytes());
    frame
}

fn read_and_validate_startup_grant(
    reader: &mut dyn Read,
    session_id: &str,
    socket_path: &Path,
    supervisor_pid: u32,
) -> PtyResult<ValidatedStartupGrant> {
    let mut frame = Vec::with_capacity(STARTUP_GRANT_BYTES + 1);
    reader
        .take((STARTUP_GRANT_BYTES + 1) as u64)
        .read_to_end(&mut frame)
        .map_err(PtyError::from)?;
    if frame.len() != STARTUP_GRANT_BYTES || &frame[..8] != STARTUP_GRANT_MAGIC {
        return Err(PtyError::Backend(
            "session supervisor startup grant is invalid".to_owned(),
        ));
    }
    let expected_session_hash = startup_binding_hash(session_id.as_bytes());
    let expected_socket_hash = startup_binding_hash(socket_path.as_os_str().as_bytes());
    if frame[56..88] != expected_session_hash
        || frame[88..120] != expected_socket_hash
        || frame[120..124] != supervisor_pid.to_be_bytes()
    {
        return Err(PtyError::Backend(
            "session supervisor startup grant binding is invalid".to_owned(),
        ));
    }
    let cleanup_capability = frame[24..56]
        .try_into()
        .expect("validated startup grant capability length");
    Ok(ValidatedStartupGrant { cleanup_capability })
}

fn random_cleanup_capability() -> [u8; CLEANUP_CAPABILITY_BYTES] {
    let mut capability = [0_u8; CLEANUP_CAPABILITY_BYTES];
    OsRng.fill_bytes(&mut capability);
    capability
}

fn random_16_bytes() -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn decode_cleanup_capability(encoded: &str) -> PtyResult<[u8; CLEANUP_CAPABILITY_BYTES]> {
    let decoded = general_purpose::STANDARD
        .decode(encoded)
        .map_err(PtyError::backend)?;
    decoded.try_into().map_err(|_| {
        PtyError::Backend("invalid session supervisor cleanup capability length".to_owned())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupervisorPathIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct SecureRuntimeDir {
    file: fs::File,
    identity: SupervisorPathIdentity,
    path: PathBuf,
    effective_uid: u32,
}

struct BoundSupervisorListener {
    listener: UnixListener,
    runtime_dir: SecureRuntimeDir,
    socket_file: fs::File,
    socket_name: CString,
    socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisorProcess {
    pid: u32,
    session_id: String,
    socket_path: PathBuf,
    size: Option<PtySize>,
}

/// 从仍存活的 session supervisor 进程中提取出的可恢复记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorRestoreCandidate {
    pub session_id: String,
    pub socket_path: PathBuf,
    pub supervisor_pid: u32,
    pub size: PtySize,
}

/// 生产 daemon 使用的 supervisor PTY backend。
#[derive(Debug, Clone)]
pub struct SupervisorPtyBackend {
    binary_path: PathBuf,
    runtime_dir: Result<PathBuf, String>,
}

impl SupervisorPtyBackend {
    /// 使用显式 `termd` 二进制路径和 state path 创建 backend。
    ///
    /// 集成测试会传 `CARGO_BIN_EXE_termd`；生产默认路径见 `for_state_path`。
    pub fn with_binary_and_state_path(
        binary_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
    ) -> Self {
        Self {
            binary_path: binary_path.as_ref().to_path_buf(),
            runtime_dir: supervisor_runtime_dir(state_path.as_ref()),
        }
    }

    /// 基于当前运行中的 `termd` 二进制构造 supervisor backend。
    pub fn for_state_path(state_path: impl AsRef<Path>) -> Self {
        let binary_path = discover_termd_binary_path();
        Self::with_binary_and_state_path(binary_path, state_path)
    }

    fn runtime_dir(&self) -> PtyResult<&Path> {
        self.runtime_dir
            .as_ref()
            .map(PathBuf::as_path)
            .map_err(|message| PtyError::Backend(message.clone()))
    }

    fn socket_path_for_session(&self, session_id: &str) -> PtyResult<PathBuf> {
        Ok(self
            .runtime_dir()?
            .join(short_supervisor_socket_file_name(session_id)))
    }

    /// 统计当前 supervisor 目录中已经不属于有效 runtime session 的孤儿进程。
    ///
    /// 启动恢复阶段只能告警，不能主动杀进程；否则 socket 文件短暂缺失或状态迁移异常时，
    /// 会把仍在运行的用户 shell 当成垃圾回收掉。
    pub fn orphaned_supervisor_count<I, S>(&self, valid_session_ids: I) -> PtyResult<usize>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let valid_session_ids = valid_session_ids
            .into_iter()
            .map(|session_id| session_id.as_ref().to_owned())
            .collect::<HashSet<_>>();
        let supervisors = supervisor_processes_from_proc()?;
        let runtime_dir = self.runtime_dir()?;
        let orphan_pids = orphaned_supervisor_pids(runtime_dir, &valid_session_ids, &supervisors);

        Ok(orphan_pids.len())
    }

    /// 列出当前 state 目录下仍存活、且命令行足够完整的 session supervisor。
    pub fn live_supervisor_restore_candidates(&self) -> PtyResult<Vec<SupervisorRestoreCandidate>> {
        let supervisors = supervisor_processes_from_proc()?;
        let runtime_dir = self.runtime_dir()?;
        Ok(supervisors
            .into_iter()
            .filter(|supervisor| supervisor.socket_path.parent() == Some(runtime_dir))
            .filter_map(|supervisor| {
                let size = supervisor.size?;
                Some(SupervisorRestoreCandidate {
                    session_id: supervisor.session_id,
                    socket_path: supervisor.socket_path,
                    supervisor_pid: supervisor.pid,
                    size,
                })
            })
            .collect())
    }

    fn launch_supervisor(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        let grant = PtyStartupGrant::new(random_16_bytes(), random_cleanup_capability());
        let mut evidence_committed = |_restore_info: &PtyRestoreInfo| Ok(());
        self.launch_supervisor_with_grant(
            session_id,
            command,
            size,
            &grant,
            &mut evidence_committed,
        )
    }

    fn launch_supervisor_with_grant(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
        grant: &PtyStartupGrant,
        evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
    ) -> PtyResult<Box<dyn PtySession>> {
        let runtime_dir = self.runtime_dir()?;
        let runtime_dir = SecureRuntimeDir::open(runtime_dir)?;
        let socket_path = self.socket_path_for_session(session_id)?;

        let socket_name = c_string_from_os_str(
            socket_path.file_name().ok_or_else(|| {
                PtyError::Backend(format!(
                    "supervisor socket path has no file name: {}",
                    socket_path.display()
                ))
            })?,
            &socket_path,
        )?;
        remove_existing_supervisor_socket(&runtime_dir, &socket_name, &socket_path)?;

        let command_base64 = general_purpose::STANDARD
            .encode(serde_json::to_vec(command).map_err(PtyError::backend)?);
        let size_base64 =
            general_purpose::STANDARD.encode(serde_json::to_vec(&size).map_err(PtyError::backend)?);
        let socket_path_arg = socket_path.to_string_lossy().to_string();

        let mut child = ProcessCommand::new(&self.binary_path)
            .args([
                "__session-supervisor",
                "--session-id",
                session_id,
                "--socket-path",
                &socket_path_arg,
                "--command-base64",
                &command_base64,
                "--size-base64",
                &size_base64,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(PtyError::from)?;
        let supervisor_pid = child.id();
        let restore_info = PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid,
            supervisor_status: PtySupervisorStatus::Running,
        };
        if let Err(error) = evidence_committed(&restore_info) {
            drop(child.stdin.take());
            let _ = child.wait();
            return Err(error);
        }
        let startup_frame = encode_startup_grant(grant, session_id, &socket_path, supervisor_pid);
        let mut startup_pipe = child.stdin.take().ok_or_else(|| {
            PtyError::Backend("session supervisor startup grant pipe is unavailable".to_owned())
        })?;
        startup_pipe
            .write_all(&startup_frame)
            .map_err(PtyError::from)?;
        drop(startup_pipe);
        let cleanup_capability: [u8; CLEANUP_CAPABILITY_BYTES] = grant
            .capability()
            .try_into()
            .expect("startup grant capability length");

        let supervisor_child = Arc::new(StdMutex::new(Some(child)));
        match self.connect_session(
            session_id,
            &socket_path,
            supervisor_pid,
            Arc::clone(&supervisor_child),
            Some(cleanup_capability),
        ) {
            Ok(session) => Ok(session),
            Err(connect_error) => Ok(Box::new(DisconnectedSupervisorPtySession {
                session_id: session_id.to_owned(),
                restore_info,
                supervisor_child,
                cleanup_capability: Some(cleanup_capability),
                close_operation_id: None,
                connect_error: connect_error.to_string(),
            })),
        }
    }

    fn connect_session(
        &self,
        session_id: &str,
        socket_path: &Path,
        supervisor_pid: u32,
        supervisor_child: Arc<StdMutex<Option<Child>>>,
        cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    ) -> PtyResult<Box<dyn PtySession>> {
        let session = SupervisorPtySession::connect_with_child_slot(
            session_id,
            socket_path.to_path_buf(),
            supervisor_pid,
            supervisor_child,
            cleanup_capability,
        )?;
        Ok(Box::new(session))
    }
}

impl Default for SupervisorPtyBackend {
    fn default() -> Self {
        Self::for_state_path("daemon-state.json")
    }
}

impl PtyBackend for SupervisorPtyBackend {
    fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let session_id = format!("session-supervisor-{}-{nanos}", std::process::id());
        self.launch_supervisor(&session_id, command, size)
    }

    fn spawn_named(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        self.launch_supervisor(session_id, command, size)
    }

    fn expected_socket_path(&self, session_id: &str) -> PtyResult<Option<PathBuf>> {
        self.socket_path_for_session(session_id).map(Some)
    }

    fn spawn_named_gated(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
        grant: &PtyStartupGrant,
        evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
    ) -> PtyResult<Box<dyn PtySession>> {
        self.launch_supervisor_with_grant(session_id, command, size, grant, evidence_committed)
    }

    fn reconcile_owned_cleanup(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        capability: &[u8],
        operation_id: u64,
    ) -> PtyResult<bool> {
        let capability: [u8; CLEANUP_CAPABILITY_BYTES] = capability.try_into().map_err(|_| {
            PtyError::Backend("invalid durable supervisor cleanup capability".to_owned())
        })?;
        finalize_supervisor_close_with_capability(
            session_id,
            restore_info,
            operation_id,
            &capability,
        )
    }

    fn owned_natural_exit_status(&self, restore_info: &PtyRestoreInfo) -> PtyResult<bool> {
        supervisor_natural_exit_status(restore_info)
    }

    fn install_legacy_cleanup_capability(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        capability: &[u8],
    ) -> PtyResult<bool> {
        let PtyRestoreInfo::UnixSocket { socket_path, .. } = restore_info else {
            return Err(PtyError::Backend(
                "legacy capability migration requires Unix restore metadata".to_owned(),
            ));
        };
        let capability: [u8; CLEANUP_CAPABILITY_BYTES] = capability.try_into().map_err(|_| {
            PtyError::Backend("invalid migrated supervisor cleanup capability".to_owned())
        })?;
        let mut stream = connect_supervisor_socket(socket_path)?;
        supervisor_candidate_request(
            &mut stream,
            1,
            SupervisorRequest::AttachSync {
                session_id: session_id.to_owned(),
                last_terminal_seq: None,
                resume_controller_id: None,
            },
        )?
        .into_attach_sync()?;
        match supervisor_candidate_request(
            &mut stream,
            2,
            SupervisorRequest::InstallCleanupCapability {
                session_id: session_id.to_owned(),
                capability_base64: general_purpose::STANDARD.encode(capability),
                migration_operation_id: None,
            },
        ) {
            Ok(response) => {
                response.expect_empty()?;
                Ok(true)
            }
            Err(PtyError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn reconcile_legacy_owned_cleanup(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
    ) -> PtyResult<bool> {
        let PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            ..
        } = restore_info
        else {
            return Ok(false);
        };
        let mut session = self.connect_session(
            session_id,
            socket_path,
            *supervisor_pid,
            Arc::new(StdMutex::new(None)),
            None,
        )?;
        session.terminate()?;
        Ok(true)
    }

    fn reconnect(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        _size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        match restore_info {
            PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                supervisor_status,
            } => {
                if *supervisor_status != PtySupervisorStatus::Running {
                    return Err(PtyError::Backend(format!(
                        "session supervisor is not reconnectable in {supervisor_status:?} state"
                    )));
                }
                let mut session = self.connect_session(
                    session_id,
                    socket_path,
                    *supervisor_pid,
                    Arc::new(StdMutex::new(None)),
                    None,
                )?;
                // attach 已返回一次 snapshot；这里再走 ping/snapshot，让重连入口明确校验
                // supervisor 仍能响应控制帧，而不是只依赖 socket connect 成功。
                session.ping()?;
                let _ = session.snapshot()?;
                Ok(session)
            }
            PtyRestoreInfo::Tmux { .. } => Err(PtyError::Backend(
                "supervisor backend cannot reconnect obsolete tmux restore metadata".to_owned(),
            )),
        }
    }

    fn attach_client(
        &self,
        session_id: &str,
        restore_info: Option<&PtyRestoreInfo>,
        _size: PtySize,
        attachment_id: &str,
        bootstrap: PtyAttachmentBootstrap,
    ) -> PtyResult<Box<dyn PtyAttachment>> {
        let control_socket_path = match restore_info {
            Some(PtyRestoreInfo::UnixSocket { socket_path, .. }) => socket_path.clone(),
            Some(PtyRestoreInfo::Tmux { .. }) => {
                return Err(PtyError::Backend(
                    "supervisor backend cannot attach obsolete tmux restore metadata".to_owned(),
                ));
            }
            None => {
                return Err(PtyError::Backend(
                    "supervisor attach requires reconnect metadata".to_owned(),
                ));
            }
        };
        let attach_socket_path =
            attach_socket_path_for_control_socket(&control_socket_path, session_id);
        let attachment = SupervisorAttachProxy::connect(
            session_id,
            attach_socket_path,
            attachment_id,
            bootstrap,
        )?;
        Ok(Box::new(attachment))
    }
}

/// daemon 侧持有的 supervisor IPC 客户端。
struct SupervisorPtySession {
    session_id: String,
    restore_info: PtyRestoreInfo,
    supervisor_child: Arc<StdMutex<Option<Child>>>,
    writer: StdMutex<StdUnixStream>,
    pending_requests: Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
    bootstrap_terminal_frames: Arc<StdMutex<Option<VecDeque<PtyTerminalFrame>>>>,
    output_signal_tx: watch::Sender<u64>,
    output_signal_rx: watch::Receiver<u64>,
    next_request_id: AtomicU64,
    controller_identity: StdMutex<Option<ControllerIdentity>>,
    cached_size: StdMutex<PtySize>,
    cached_process_id: StdMutex<Option<u32>>,
    close_operation_id: StdMutex<Option<u64>>,
    cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    exited: Arc<AtomicBool>,
}

impl SupervisorPtySession {
    #[cfg(test)]
    fn connect(
        session_id: &str,
        socket_path: PathBuf,
        supervisor_pid: u32,
        child: Option<Child>,
    ) -> PtyResult<Self> {
        Self::connect_with_child_slot(
            session_id,
            socket_path,
            supervisor_pid,
            Arc::new(StdMutex::new(child)),
            Some(random_cleanup_capability()),
        )
    }

    fn connect_with_child_slot(
        session_id: &str,
        socket_path: PathBuf,
        supervisor_pid: u32,
        supervisor_child: Arc<StdMutex<Option<Child>>>,
        cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    ) -> PtyResult<Self> {
        let stream = connect_supervisor_socket(&socket_path)?;
        let writer = stream.try_clone().map_err(PtyError::from)?;
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_output = Arc::new(StdMutex::new(VecDeque::new()));
        let pending_terminal_frames = Arc::new(StdMutex::new(VecDeque::new()));
        let terminal_mirror = Arc::new(StdMutex::new(SupervisorTerminalMirror::new(
            PtySize::default(),
        )));
        let bootstrap_terminal_frames = Arc::new(StdMutex::new(Some(VecDeque::new())));
        let exited = Arc::new(AtomicBool::new(false));
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        spawn_supervisor_reader_thread(
            session_id,
            stream,
            SupervisorReaderShared {
                pending_requests: Arc::clone(&pending_requests),
                pending_terminal_frames: Arc::clone(&pending_terminal_frames),
                terminal_mirror: Arc::clone(&terminal_mirror),
                bootstrap_terminal_frames: Arc::clone(&bootstrap_terminal_frames),
                exited: Arc::clone(&exited),
            },
            output_signal_tx.clone(),
        )?;

        let restore_info = PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            supervisor_status: PtySupervisorStatus::Running,
        };
        let session = Self {
            session_id: session_id.to_owned(),
            restore_info,
            supervisor_child,
            writer: StdMutex::new(writer),
            pending_requests,
            pending_output,
            pending_terminal_frames,
            terminal_mirror,
            bootstrap_terminal_frames,
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::default()),
            cached_process_id: StdMutex::new(None),
            close_operation_id: StdMutex::new(None),
            cleanup_capability,
            exited,
        };

        let sync = session.attach_sync(None)?;
        session.seed_attach_sync(sync);
        // 中文注释：新的 daemon controller 接管 session 时，要把旧 daemon 遗留的
        // attached-device authority 清空；否则 daemon 重启后，已经掉线的设备仍会被
        // supervisor 当作已 attach。探测使用独立连接，因此 0.6.4 supervisor 遇到未知
        // request 后关闭探测连接时，不会破坏已经建立的兼容 controller 连接。
        let controller = session.current_controller_id().ok_or_else(|| {
            PtyError::Backend("session supervisor controller identity is missing".to_owned())
        })?;
        reset_attached_devices_if_supported(&session.restore_info, controller)?;

        Ok(session)
    }

    fn request(&self, request: SupervisorRequest) -> PtyResult<SupervisorResponsePayload> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self.request_once(request.clone()) {
                Ok(payload) => return Ok(payload),
                Err(SupervisorRequestFailure::Response(error)) => return Err(error),
                Err(SupervisorRequestFailure::Transport(error)) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        // supervisor 进程仍存活但 Unix IPC 连接可能因为 daemon 侧读写竞态、
                        // 旧连接 EOF 或热升级残留而断开；运行中请求失败时重连同一个 socket
                        // 再做有限重试，避免把活 session 误报为 runtime_failed。
                        //
                        // 中文注释：这里只能重试传输层故障。supervisor 明确返回的业务拒绝
                        // 必须原样上抛；否则 stale controller 会借自动重连重新 attach_sync，
                        // 再次篡夺 active controller 身份。
                        match self.reconnect_ipc() {
                            Ok(()) => continue,
                            Err(reconnect_error) => {
                                last_error = Some(reconnect_error);
                            }
                        }
                    }
                }
            }
            break;
        }
        Err(last_error
            .unwrap_or_else(|| PtyError::Backend("session supervisor request failed".to_owned())))
    }

    fn request_once(
        &self,
        request: SupervisorRequest,
    ) -> Result<SupervisorResponsePayload, SupervisorRequestFailure> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request_kind = request.kind_label();
        let envelope = SupervisorRequestEnvelope {
            request_id,
            request,
        };
        let (tx, rx) = mpsc::channel();
        self.pending_requests
            .lock()
            .expect("supervisor pending request mutex poisoned")
            .insert(request_id, tx);

        let write_result = {
            let mut writer = self
                .writer
                .lock()
                .expect("supervisor writer mutex poisoned");
            write_frame_sync(&mut writer, &envelope)
        };
        if let Err(error) = write_result {
            self.pending_requests
                .lock()
                .expect("supervisor pending request mutex poisoned")
                .remove(&request_id);
            return Err(SupervisorRequestFailure::Transport(PtyError::from(error)));
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result.into_result(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending_requests
                    .lock()
                    .expect("supervisor pending request mutex poisoned")
                    .remove(&request_id);
                let (socket_path, supervisor_pid) = match &self.restore_info {
                    PtyRestoreInfo::UnixSocket {
                        socket_path,
                        supervisor_pid,
                        ..
                    } => (
                        Some(socket_path.display().to_string()),
                        Some(*supervisor_pid),
                    ),
                    PtyRestoreInfo::Tmux { .. } => (None, None),
                };
                tracing::warn!(
                    layer = "supervisor",
                    phase = "ipc_request",
                    timeout_code = "supervisor_request_timeout",
                    timeout_ms = REQUEST_TIMEOUT.as_millis() as u64,
                    session_id = %self.session_id,
                    request_id,
                    request_kind,
                    supervisor_pid,
                    socket_path,
                    "session supervisor request timed out"
                );
                Err(SupervisorRequestFailure::Transport(PtyError::Backend(
                    "session supervisor request timed out".to_owned(),
                )))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(SupervisorRequestFailure::Transport(
                PtyError::Backend("session supervisor response channel disconnected".to_owned()),
            )),
        }
    }

    fn reconnect_ipc(&self) -> PtyResult<()> {
        self.reconnect_ipc_with_validation(|_| Ok(()))
    }

    fn reconnect_ipc_with_validation(
        &self,
        mut validate: impl FnMut(ResumeValidationPoint) -> PtyResult<()>,
    ) -> PtyResult<()> {
        let (socket_path, supervisor_pid, supervisor_status) = match &self.restore_info {
            PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                supervisor_status,
            } => (socket_path.clone(), *supervisor_pid, *supervisor_status),
            PtyRestoreInfo::Tmux { .. } => {
                return Err(PtyError::Backend(
                    "supervisor IPC cannot reconnect obsolete tmux restore metadata".to_owned(),
                ));
            }
        };
        if supervisor_status != PtySupervisorStatus::Running {
            return Err(PtyError::Backend(format!(
                "session supervisor is not reconnectable in {supervisor_status:?} state"
            )));
        }

        let controller_id = self
            .current_controller_id()
            .map(|identity| identity.id)
            .ok_or_else(|| {
                PtyError::Backend("session supervisor controller lease is missing".to_owned())
            })?;
        let transaction_id = random_nonzero_u64();
        let mut last_error = None;
        for _ in 0..3 {
            let attempt = (|| -> PtyResult<_> {
                let mut stream = connect_supervisor_socket(&socket_path)?;
                let sync = supervisor_candidate_request(
                    &mut stream,
                    1,
                    SupervisorRequest::AttachSync {
                        session_id: self.session_id.clone(),
                        last_terminal_seq: None,
                        resume_controller_id: Some(controller_id),
                    },
                )?
                .into_attach_sync()?;
                validate(ResumeValidationPoint::AttachSync)?;
                supervisor_candidate_request(&mut stream, 2, SupervisorRequest::Ping)?
                    .expect_empty()?;
                validate(ResumeValidationPoint::Ping)?;
                supervisor_candidate_request(&mut stream, 3, SupervisorRequest::Snapshot)?
                    .into_snapshot()?;
                validate(ResumeValidationPoint::Snapshot)?;
                let writer = stream.try_clone().map_err(PtyError::from)?;
                validate(ResumeValidationPoint::WriterClone)?;
                let reader = prepare_supervisor_reader_thread(
                    &self.session_id,
                    SupervisorReaderShared {
                        pending_requests: Arc::clone(&self.pending_requests),
                        pending_terminal_frames: Arc::clone(&self.pending_terminal_frames),
                        terminal_mirror: Arc::clone(&self.terminal_mirror),
                        bootstrap_terminal_frames: Arc::clone(&self.bootstrap_terminal_frames),
                        exited: Arc::clone(&self.exited),
                    },
                    self.output_signal_tx.clone(),
                )?;
                validate(ResumeValidationPoint::ReaderThread)?;
                let committed = supervisor_candidate_request(
                    &mut stream,
                    4,
                    SupervisorRequest::CommitControllerResume { transaction_id },
                )?
                .into_controller_resume_status()?
                .ok_or_else(|| {
                    PtyError::Backend(
                        "session supervisor resume commit was not recorded".to_owned(),
                    )
                })?;
                validate(ResumeValidationPoint::CommitResponse)?;
                validate(ResumeValidationPoint::ReaderStart)?;
                reader.start(stream)?;
                Ok((writer, sync, committed))
            })();
            let (writer, sync, committed) = match attempt {
                Ok(values) => values,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            *self
                .bootstrap_terminal_frames
                .lock()
                .expect("supervisor bootstrap frame mutex poisoned") = Some(VecDeque::new());
            *self
                .writer
                .lock()
                .expect("supervisor writer mutex poisoned") = writer;
            self.seed_attach_sync(sync);
            *self
                .controller_identity
                .lock()
                .expect("controller identity mutex poisoned") = Some(committed);
            let _ = supervisor_pid;
            return Ok(());
        }
        Err(last_error.unwrap_or_else(|| {
            PtyError::Backend("session supervisor resume transaction failed".to_owned())
        }))
    }

    fn current_controller_id(&self) -> Option<ControllerIdentity> {
        *self
            .controller_identity
            .lock()
            .expect("controller identity mutex poisoned")
    }

    fn attach_sync(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<SupervisorAttachSyncPayload> {
        self.request(SupervisorRequest::AttachSync {
            session_id: self.session_id.clone(),
            last_terminal_seq,
            resume_controller_id: None,
        })?
        .into_attach_sync()
    }

    fn seed_attach_sync(&self, sync: SupervisorAttachSyncPayload) {
        let mut bootstrap = self
            .bootstrap_terminal_frames
            .lock()
            .expect("supervisor bootstrap frame mutex poisoned");
        *self
            .controller_identity
            .lock()
            .expect("controller identity mutex poisoned") = Some(ControllerIdentity {
            id: sync.controller_id,
            connection_id: sync.controller_connection_id,
        });
        let snapshot = sync.snapshot;
        self.exited.store(snapshot.exited, Ordering::Release);
        *self.cached_size.lock().expect("cached size mutex poisoned") = snapshot.size;
        *self
            .cached_process_id
            .lock()
            .expect("cached pid mutex poisoned") = snapshot.process_id;
        let buffered_live = bootstrap.as_mut().map(std::mem::take).unwrap_or_default();
        let mut pending_live = Vec::new();
        {
            let mut pending = self
                .pending_terminal_frames
                .lock()
                .expect("pending terminal frames mutex poisoned");
            while let Some(frame) = pending.pop_front() {
                if frame.terminal_seq().is_some_and(|seq| seq > sync.base_seq) {
                    pending_live.push(frame);
                }
            }
        }
        if sync.frames.is_empty() && !snapshot.retained_output.is_empty() {
            self.terminal_mirror
                .lock()
                .expect("terminal mirror mutex poisoned")
                .apply_snapshot_and_tail(
                    snapshot.size,
                    sync.base_seq,
                    &snapshot.retained_output,
                    &[],
                );
        }
        let mut published = false;
        for frame in sync.frames {
            published |= apply_and_enqueue_daemon_terminal_frame(
                &self.pending_terminal_frames,
                &self.terminal_mirror,
                frame,
            );
        }
        pending_live.extend(buffered_live);
        pending_live.sort_by_key(|frame| frame.terminal_seq().unwrap_or_default());
        pending_live.dedup_by_key(|frame| frame.terminal_seq());
        for frame in pending_live {
            published |= apply_and_enqueue_daemon_terminal_frame(
                &self.pending_terminal_frames,
                &self.terminal_mirror,
                frame,
            );
        }
        *bootstrap = None;
        if published {
            // 中文注释：`read()` 是 termctl/测试仍在使用的 legacy byte-stream 兼容面。
            // 现在 attach 首屏只通过 sequenced frames 传输，因此 daemon 侧也要把
            // attach_sync 的 snapshot/tail 注入本地 terminal frame 队列；否则重连后
            // legacy reader 会看不到首屏输出。但不要恢复 retained_output，Web watched
            // attach 仍以 frames 为唯一权威，避免同一屏内容双播。
            let next = self.output_signal_tx.borrow().wrapping_add(1);
            let _ = self.output_signal_tx.send(next);
        }
        if !snapshot.retained_output.is_empty() {
            enqueue_daemon_legacy_output(&self.pending_output, snapshot.retained_output, false);
            let next = self.output_signal_tx.borrow().wrapping_add(1);
            let _ = self.output_signal_tx.send(next);
        }
    }

    fn signal_pending_output(&self) {
        let next = self.output_signal_tx.borrow().wrapping_add(1);
        // daemon 侧的 supervisor 客户端也使用 watch 做输出通知；如果本地缓存
        // 还没读空，需要再次唤醒协议推送层，避免大输出停在缓存里。
        let _ = self.output_signal_tx.send(next);
    }

    fn has_pending_output_or_terminal_frames(&self) -> bool {
        !self
            .pending_output
            .lock()
            .expect("pending output mutex poisoned")
            .is_empty()
            || !self
                .pending_terminal_frames
                .lock()
                .expect("pending terminal frames mutex poisoned")
                .is_empty()
    }

    fn has_pending_terminal_frames(&self) -> bool {
        !self
            .pending_terminal_frames
            .lock()
            .expect("pending terminal frames mutex poisoned")
            .is_empty()
    }

    fn attach_device(&self, device_id: &str) -> PtyResult<()> {
        self.request(SupervisorRequest::AttachDevice {
            device_id: device_id.to_owned(),
        })?
        .expect_empty()
    }

    fn detach_device(&self, device_id: &str) -> PtyResult<()> {
        self.request(SupervisorRequest::DetachDevice {
            device_id: device_id.to_owned(),
        })?
        .expect_empty()
    }

    fn has_attached_device(&self, device_id: &str) -> PtyResult<bool> {
        self.request(SupervisorRequest::DeviceAttached {
            device_id: device_id.to_owned(),
        })?
        .into_device_attached()
    }

    fn confirm_and_finalize_close(
        &self,
        operation_id: u64,
        deadline: Instant,
        close_error: PtyError,
    ) -> PtyResult<()> {
        let socket_path = match &self.restore_info {
            PtyRestoreInfo::UnixSocket { socket_path, .. } => socket_path,
            PtyRestoreInfo::Tmux { .. } => return Err(close_error),
        };
        let cleanup_capability = self.cleanup_capability.ok_or_else(|| {
            PtyError::Backend("session supervisor cleanup capability is unavailable".to_owned())
        })?;
        let mut confirmed_dead = false;
        while Instant::now() < deadline {
            if let Some(child) = self
                .supervisor_child
                .lock()
                .expect("supervisor child mutex poisoned")
                .as_mut()
                && child.try_wait().map_err(PtyError::from)?.is_some()
            {
                return Ok(());
            }
            if confirmed_dead
                && probe_reconnected_supervisor_exit(&self.session_id, &self.restore_info)?
                    .is_some()
            {
                return Ok(());
            }
            if let Ok(mut stream) = connect_supervisor_socket(socket_path) {
                if authenticate_restricted_supervisor_close(
                    &self.session_id,
                    &mut stream,
                    &cleanup_capability,
                )
                .is_err()
                {
                    thread::sleep(CLOSE_CONFIRM_POLL_INTERVAL);
                    continue;
                }
                let status = supervisor_candidate_request(
                    &mut stream,
                    3,
                    SupervisorRequest::CloseStatus { operation_id },
                )
                .and_then(SupervisorResponsePayload::into_close_status)
                .unwrap_or(false);
                if status {
                    confirmed_dead = true;
                    if supervisor_candidate_request(
                        &mut stream,
                        4,
                        SupervisorRequest::FinalizeClose { operation_id },
                    )
                    .and_then(SupervisorResponsePayload::expect_empty)
                    .is_ok()
                    {
                        return Ok(());
                    }
                }
            }
            thread::sleep(CLOSE_CONFIRM_POLL_INTERVAL);
        }
        Err(close_error)
    }
}

fn reset_attached_devices_if_supported(
    restore_info: &PtyRestoreInfo,
    controller: ControllerIdentity,
) -> PtyResult<()> {
    let PtyRestoreInfo::UnixSocket { socket_path, .. } = restore_info else {
        return Ok(());
    };
    let mut stream = connect_supervisor_socket(socket_path)?;
    write_frame_sync(
        &mut stream,
        &SupervisorRequestEnvelope {
            request_id: 1,
            request: SupervisorRequest::ResetAttachedDevicesForController {
                controller_id: controller.id,
                controller_connection_id: controller.connection_id,
            },
        },
    )
    .map_err(PtyError::from)?;
    loop {
        match read_frame_sync::<SupervisorFrame>(&mut stream) {
            Ok(SupervisorFrame::Response {
                request_id: 1,
                response,
            }) => return response.into_result()?.expect_empty(),
            Ok(SupervisorFrame::TerminalFrame { .. }) => {}
            Ok(SupervisorFrame::Response { .. }) => {
                return Err(PtyError::Backend(
                    "session supervisor returned a mismatched reset response".to_owned(),
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(PtyError::from(error)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResumeValidationPoint {
    AttachSync,
    Ping,
    Snapshot,
    WriterClone,
    ReaderThread,
    CommitResponse,
    ReaderStart,
}

fn random_nonzero_u64() -> u64 {
    loop {
        let value = OsRng.next_u64();
        if value != 0 {
            return value;
        }
    }
}

fn cleanup_auth_proof(
    capability: &[u8; CLEANUP_CAPABILITY_BYTES],
    session_id: &str,
    client_nonce: &[u8; CLEANUP_AUTH_NONCE_BYTES],
    server_nonce: &[u8; CLEANUP_AUTH_NONCE_BYTES],
) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut inner_key = [0x36_u8; BLOCK_BYTES];
    let mut outer_key = [0x5c_u8; BLOCK_BYTES];
    for (index, byte) in capability.iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(b"termd-supervisor-cleanup-v1\0");
    inner.update(session_id.as_bytes());
    inner.update([0]);
    inner.update(client_nonce);
    inner.update(server_nonce);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn decode_cleanup_nonce(encoded: &str) -> PtyResult<[u8; CLEANUP_AUTH_NONCE_BYTES]> {
    let decoded = general_purpose::STANDARD
        .decode(encoded)
        .map_err(PtyError::backend)?;
    decoded.try_into().map_err(|_| {
        PtyError::Backend("invalid session supervisor cleanup authentication nonce".to_owned())
    })
}

fn supervisor_candidate_request(
    stream: &mut StdUnixStream,
    request_id: u64,
    request: SupervisorRequest,
) -> PtyResult<SupervisorResponsePayload> {
    write_frame_sync(
        stream,
        &SupervisorRequestEnvelope {
            request_id,
            request,
        },
    )
    .map_err(PtyError::from)?;
    loop {
        match read_frame_sync::<SupervisorFrame>(stream).map_err(PtyError::from)? {
            SupervisorFrame::Response {
                request_id: response_id,
                response,
            } if response_id == request_id => return response.into_result(),
            SupervisorFrame::TerminalFrame { .. } => {}
            SupervisorFrame::Response { .. } => {
                return Err(PtyError::Backend(
                    "session supervisor returned a mismatched candidate response".to_owned(),
                ));
            }
        }
    }
}

/// daemon watched attachment 对应的 supervisor attach 代理。
///
/// 中文注释：这个代理只搬运 opaque frame，不再把 terminal output/input/heartbeat
/// 解释回 daemon 业务对象。
struct SupervisorAttachProxy {
    writer: StdMutex<StdUnixStream>,
    pending_frames: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    reader_error: Arc<StdMutex<Option<String>>>,
    output_signal_tx: watch::Sender<u64>,
    output_signal_rx: watch::Receiver<u64>,
}

impl SupervisorAttachProxy {
    fn connect(
        session_id: &str,
        socket_path: PathBuf,
        attachment_id: &str,
        bootstrap: PtyAttachmentBootstrap,
    ) -> PtyResult<Self> {
        let mut stream = connect_supervisor_socket(&socket_path)?;
        write_frame_sync(
            &mut stream,
            &SupervisorTerminalClientFrame::BootstrapAttach {
                session_id: session_id.to_owned(),
                last_terminal_seq: bootstrap.last_terminal_seq,
            },
        )
        .map_err(PtyError::from)?;
        let writer = stream.try_clone().map_err(PtyError::from)?;
        let pending_frames = Arc::new(StdMutex::new(VecDeque::new()));
        let reader_error = Arc::new(StdMutex::new(None));
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        spawn_terminal_attach_reader_thread(
            attachment_id,
            stream,
            Arc::clone(&pending_frames),
            Arc::clone(&reader_error),
            output_signal_tx.clone(),
        )?;
        Ok(Self {
            writer: StdMutex::new(writer),
            pending_frames,
            reader_error,
            output_signal_tx,
            output_signal_rx,
        })
    }

    fn has_pending_frames(&self) -> bool {
        !self
            .pending_frames
            .lock()
            .expect("terminal attach pending frame mutex poisoned")
            .is_empty()
    }

    fn signal_pending_output(&self) {
        let next = self.output_signal_tx.borrow().wrapping_add(1);
        let _ = self.output_signal_tx.send(next);
    }
}

impl PtyAttachment for SupervisorAttachProxy {
    fn output_signal(&self) -> Option<watch::Receiver<u64>> {
        Some(self.output_signal_rx.clone())
    }

    fn read_frame(&mut self) -> PtyResult<Option<Vec<u8>>> {
        if let Some(message) = self
            .reader_error
            .lock()
            .expect("terminal attach reader error mutex poisoned")
            .take()
        {
            return Err(PtyError::Backend(message));
        }
        let frame = self
            .pending_frames
            .lock()
            .expect("terminal attach pending frame mutex poisoned")
            .pop_front();
        if self.has_pending_frames() {
            self.signal_pending_output();
        }
        Ok(frame)
    }

    fn write_frame(&mut self, bytes: &[u8]) -> PtyResult<()> {
        validate_length_prefixed_supervisor_frame(bytes).map_err(PtyError::from)?;
        let mut writer = self
            .writer
            .lock()
            .expect("terminal attach writer mutex poisoned");
        writer.write_all(bytes).map_err(PtyError::from)?;
        writer.flush().map_err(PtyError::from)?;
        Ok(())
    }
}

impl Drop for SupervisorPtySession {
    fn drop(&mut self) {
        let Ok(mut child_slot) = self.supervisor_child.lock() else {
            return;
        };
        let Some(mut child) = child_slot.take() else {
            return;
        };
        let supervisor_pid = child.id();

        // 普通 daemon 重启时进程会退出，supervisor 会被系统收养；但如果运行中的
        // daemon 仅丢弃 runtime/session 对象，必须保留一个 reaper 等待子进程，避免它
        // 后续退出时在当前 daemon 下变成 zombie。
        let _ = thread::Builder::new()
            .name(format!("termd-supervisor-reaper-{supervisor_pid}"))
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

fn connect_supervisor_socket(socket_path: &Path) -> PtyResult<StdUnixStream> {
    let deadline = Instant::now() + SOCKET_CONNECT_TIMEOUT;
    let stream = loop {
        match StdUnixStream::connect(socket_path) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                if error.kind() != io::ErrorKind::NotFound
                    && error.kind() != io::ErrorKind::ConnectionRefused
                {
                    return Err(PtyError::Io(error));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(PtyError::Io(error)),
        }
    };
    stream.set_nonblocking(false).map_err(PtyError::from)?;
    Ok(stream)
}

fn spawn_terminal_attach_reader_thread(
    attachment_id: &str,
    mut reader: StdUnixStream,
    pending_frames: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    reader_error: Arc<StdMutex<Option<String>>>,
    output_signal_tx: watch::Sender<u64>,
) -> PtyResult<()> {
    let thread_name = format!("termd-supervisor-attach-{attachment_id}");
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            loop {
                match read_raw_frame_sync(&mut reader) {
                    Ok(frame) => {
                        let mut pending = pending_frames
                            .lock()
                            .expect("terminal attach pending frame mutex poisoned");
                        let retained_bytes = pending.iter().map(Vec::len).sum::<usize>();
                        if pending.len() >= SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES
                            || retained_bytes.saturating_add(frame.len())
                                > SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
                        {
                            pending.clear();
                            *reader_error
                                .lock()
                                .expect("terminal attach reader error mutex poisoned") = Some(
                                "session supervisor attach output queue overflow; reattach to resynchronize"
                                    .to_owned(),
                            );
                            let next = output_signal_tx.borrow().wrapping_add(1);
                            let _ = output_signal_tx.send(next);
                            break;
                        }
                        pending.push_back(frame);
                        drop(pending);
                        let next = output_signal_tx.borrow().wrapping_add(1);
                        let _ = output_signal_tx.send(next);
                    }
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => {
                        pending_frames
                            .lock()
                            .expect("terminal attach pending frame mutex poisoned")
                            .clear();
                        *reader_error
                            .lock()
                            .expect("terminal attach reader error mutex poisoned") = Some(format!(
                            "session supervisor attach transport failed; reattach to resynchronize: {error}"
                        ));
                        let next = output_signal_tx.borrow().wrapping_add(1);
                        let _ = output_signal_tx.send(next);
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(PtyError::backend)
}

struct SupervisorReaderShared {
    pending_requests: Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
    bootstrap_terminal_frames: Arc<StdMutex<Option<VecDeque<PtyTerminalFrame>>>>,
    exited: Arc<AtomicBool>,
}

fn spawn_supervisor_reader_thread(
    session_id: &str,
    reader: StdUnixStream,
    shared: SupervisorReaderShared,
    output_signal_tx: watch::Sender<u64>,
) -> PtyResult<()> {
    prepare_supervisor_reader_thread(session_id, shared, output_signal_tx)?.start(reader)
}

struct PreparedSupervisorReaderThread {
    start_tx: mpsc::SyncSender<StdUnixStream>,
}

impl PreparedSupervisorReaderThread {
    fn start(self, reader: StdUnixStream) -> PtyResult<()> {
        self.start_tx.send(reader).map_err(PtyError::backend)
    }
}

fn prepare_supervisor_reader_thread(
    session_id: &str,
    shared: SupervisorReaderShared,
    output_signal_tx: watch::Sender<u64>,
) -> PtyResult<PreparedSupervisorReaderThread> {
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("termd-supervisor-ipc-{session_id}"))
        .spawn(move || {
            if let Ok(reader) = start_rx.recv() {
                supervisor_reader_loop(
                    reader,
                    shared.pending_requests,
                    shared.pending_terminal_frames,
                    shared.terminal_mirror,
                    shared.bootstrap_terminal_frames,
                    output_signal_tx,
                    shared.exited,
                );
            }
        })
        .map_err(PtyError::backend)?;
    Ok(PreparedSupervisorReaderThread { start_tx })
}

struct DisconnectedSupervisorPtySession {
    session_id: String,
    restore_info: PtyRestoreInfo,
    supervisor_child: Arc<StdMutex<Option<Child>>>,
    connect_error: String,
    cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    close_operation_id: Option<u64>,
}

impl DisconnectedSupervisorPtySession {
    fn unavailable<T>(&self) -> PtyResult<T> {
        Err(PtyError::Backend(self.connect_error.clone()))
    }
}

impl PtySession for DisconnectedSupervisorPtySession {
    fn read(&mut self, _buffer: &mut [u8]) -> PtyResult<usize> {
        self.unavailable()
    }

    fn write_all(&mut self, _bytes: &[u8]) -> PtyResult<()> {
        self.unavailable()
    }

    fn authority_attach_device(&mut self, _device_id: &str) -> PtyResult<Option<()>> {
        self.unavailable()
    }

    fn authority_detach_device(&mut self, _device_id: &str) -> PtyResult<Option<()>> {
        self.unavailable()
    }

    fn authority_has_device(&mut self, _device_id: &str) -> PtyResult<Option<bool>> {
        self.unavailable()
    }

    fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
        self.unavailable()
    }

    fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
        self.unavailable()
    }

    fn ping(&mut self) -> PtyResult<()> {
        self.unavailable()
    }

    fn restore_info(&self) -> Option<PtyRestoreInfo> {
        Some(self.restore_info.clone())
    }

    fn terminate(&mut self) -> PtyResult<()> {
        self.restore_info =
            restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closing);
        let operation_id = self.close_operation_id.ok_or_else(|| {
            PtyError::Backend(
                "disconnected supervisor cleanup requires a durable close operation".to_owned(),
            )
        })?;
        let capability = self.cleanup_capability.ok_or_else(|| {
            PtyError::Backend(
                "disconnected supervisor cleanup capability is unavailable".to_owned(),
            )
        })?;
        if !finalize_supervisor_close_with_capability(
            &self.session_id,
            &self.restore_info,
            operation_id,
            &capability,
        )? {
            return Err(PtyError::Backend(
                "disconnected supervisor cleanup was not confirmed".to_owned(),
            ));
        }
        reap_supervisor_child_pid(&self.restore_info)?;
        self.supervisor_child
            .lock()
            .expect("supervisor child mutex poisoned")
            .take();
        self.restore_info =
            restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closed);
        Ok(())
    }

    fn try_wait(&mut self) -> PtyResult<Option<super::PtyExitStatus>> {
        if let Some(child) = self
            .supervisor_child
            .lock()
            .expect("supervisor child mutex poisoned")
            .as_mut()
        {
            return child
                .try_wait()
                .map(|status| status.map(|_| super::PtyExitStatus::exited(0)))
                .map_err(PtyError::from);
        }
        probe_reconnected_supervisor_exit(&self.session_id, &self.restore_info)
    }

    fn wait(&mut self) -> PtyResult<super::PtyExitStatus> {
        self.unavailable()
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

impl PtySession for SupervisorPtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let mut chunk = if let Some(chunk) = self
            .pending_output
            .lock()
            .expect("pending output mutex poisoned")
            .pop_front()
        {
            chunk
        } else {
            loop {
                let Some(frame) = self
                    .pending_terminal_frames
                    .lock()
                    .expect("pending terminal frames mutex poisoned")
                    .pop_front()
                else {
                    return Ok(0);
                };
                if let Some(bytes) = frame.bytes_for_legacy_read() {
                    break bytes.to_vec();
                }
            }
        };

        let read = buffer.len().min(chunk.len());
        buffer[..read].copy_from_slice(&chunk[..read]);
        if read < chunk.len() {
            let remaining = chunk.split_off(read);
            enqueue_daemon_legacy_output(&self.pending_output, remaining, true);
        }
        if self.has_pending_output_or_terminal_frames() {
            self.signal_pending_output();
        }
        Ok(read)
    }

    fn output_signal(&self) -> Option<watch::Receiver<u64>> {
        Some(self.output_signal_rx.clone())
    }

    fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
        self.request(SupervisorRequest::Input {
            data_base64: general_purpose::STANDARD.encode(bytes),
        })?
        .expect_empty()
    }

    fn authority_attach_device(&mut self, device_id: &str) -> PtyResult<Option<()>> {
        self.attach_device(device_id)?;
        Ok(Some(()))
    }

    fn authority_detach_device(&mut self, device_id: &str) -> PtyResult<Option<()>> {
        self.detach_device(device_id)?;
        Ok(Some(()))
    }

    fn authority_has_device(&mut self, device_id: &str) -> PtyResult<Option<bool>> {
        Ok(Some(self.has_attached_device(device_id)?))
    }

    fn resize(&mut self, size: PtySize) -> PtyResult<()> {
        self.request(SupervisorRequest::Resize { size })?
            .expect_empty()?;
        *self.cached_size.lock().expect("cached size mutex poisoned") = size;
        Ok(())
    }

    fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
        let snapshot = self.request(SupervisorRequest::Snapshot)?.into_snapshot()?;
        self.exited.store(snapshot.exited, Ordering::Release);
        *self.cached_size.lock().expect("cached size mutex poisoned") = snapshot.size;
        *self
            .cached_process_id
            .lock()
            .expect("cached pid mutex poisoned") = snapshot.process_id;
        Ok(PtySnapshot {
            size: snapshot.size,
            process_id: snapshot.process_id,
            retained_output: snapshot.retained_output,
        })
    }

    fn terminal_snapshot(
        &mut self,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<Vec<PtyTerminalFrame>> {
        let (_base_seq, frames) = self
            .terminal_mirror
            .lock()
            .expect("terminal mirror mutex poisoned")
            .terminal_snapshot_or_tail(last_terminal_seq);
        if let Some(PtyTerminalFrame::Snapshot { size, .. }) = frames.first() {
            *self.cached_size.lock().expect("cached size mutex poisoned") = *size;
        }
        Ok(frames)
    }

    fn read_terminal_frame(&mut self) -> PtyResult<Option<PtyTerminalFrame>> {
        let frame = self
            .pending_terminal_frames
            .lock()
            .expect("pending terminal frames mutex poisoned")
            .pop_front();
        // terminal_frame 新协议只消费结构化 frame；legacy raw output 缓存不能在这里 rearm，
        // 否则 packet client 会在没有新 frame 时空转唤醒 output watcher。
        if self.has_pending_terminal_frames() {
            self.signal_pending_output();
        }
        Ok(frame)
    }

    fn ping(&mut self) -> PtyResult<()> {
        self.request(SupervisorRequest::Ping)?.expect_empty()
    }

    fn restore_info(&self) -> Option<PtyRestoreInfo> {
        Some(self.restore_info.clone())
    }

    fn terminate(&mut self) -> PtyResult<()> {
        self.restore_info =
            restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closing);
        if self.cleanup_capability.is_none() {
            self.request_once(SupervisorRequest::Close)
                .map_err(|failure| match failure {
                    SupervisorRequestFailure::Response(error)
                    | SupervisorRequestFailure::Transport(error) => error,
                })?
                .expect_empty()?;
            self.restore_info =
                restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closed);
            return Ok(());
        }
        let deadline = Instant::now() + CLOSE_CONFIRM_TIMEOUT;
        let operation_id = {
            let mut operation_id = self
                .close_operation_id
                .lock()
                .expect("close operation mutex poisoned");
            *operation_id.get_or_insert_with(random_nonzero_u64)
        };
        let close_result = self
            .request_once(SupervisorRequest::CloseIdempotent { operation_id })
            .map_err(|failure| match failure {
                SupervisorRequestFailure::Response(error)
                | SupervisorRequestFailure::Transport(error) => error,
            })
            .and_then(SupervisorResponsePayload::into_close_status);
        let close_error = match close_result {
            Ok(true) => None,
            Ok(false) => Some(PtyError::Backend(
                "session supervisor close was not confirmed".to_owned(),
            )),
            Err(error) => Some(error),
        };
        if let Some(close_error) = close_error {
            self.confirm_and_finalize_close(operation_id, deadline, close_error)?;
        } else {
            self.confirm_and_finalize_close(
                operation_id,
                deadline,
                PtyError::Backend("session supervisor finalize was not confirmed".to_owned()),
            )?;
        }

        // 只有直接 spawn supervisor 的 daemon 持有 Child 句柄；重连 daemon 没有父子关系，
        // 不能 wait 已经被其他父进程收养的 supervisor。
        let mut child_guard = self
            .supervisor_child
            .lock()
            .expect("supervisor child mutex poisoned");
        while let Some(child) = child_guard.as_mut() {
            if child.try_wait().map_err(PtyError::from)?.is_some() {
                child_guard.take();
                break;
            }
            if Instant::now() >= deadline {
                return Err(PtyError::Backend(
                    "timed out reaping session supervisor child".to_owned(),
                ));
            }
            thread::sleep(CLOSE_CONFIRM_POLL_INTERVAL);
        }
        self.restore_info =
            restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closed);
        Ok(())
    }

    fn try_wait(&mut self) -> PtyResult<Option<super::PtyExitStatus>> {
        if self.exited.load(Ordering::Acquire) {
            return Ok(Some(super::PtyExitStatus::exited(0)));
        }
        if let Some(child) = self
            .supervisor_child
            .lock()
            .expect("supervisor child mutex poisoned")
            .as_mut()
        {
            return child
                .try_wait()
                .map(|status| status.map(|_| super::PtyExitStatus::exited(0)))
                .map_err(PtyError::from);
        }
        let snapshot = self.request(SupervisorRequest::Snapshot)?.into_snapshot()?;
        self.exited.store(snapshot.exited, Ordering::Release);
        if snapshot.exited {
            return Ok(Some(super::PtyExitStatus::exited(0)));
        }
        probe_reconnected_supervisor_exit(&self.session_id, &self.restore_info)
    }

    fn wait(&mut self) -> PtyResult<super::PtyExitStatus> {
        Err(PtyError::Backend(
            "session supervisor wait is not supported".to_owned(),
        ))
    }

    fn process_id(&self) -> Option<u32> {
        *self
            .cached_process_id
            .lock()
            .expect("cached pid mutex poisoned")
    }

    fn current_working_directory(&self) -> Option<PathBuf> {
        self.process_id()
            .and_then(super::current_working_directory_for_pid)
    }
}

fn restore_info_with_status(
    restore_info: &PtyRestoreInfo,
    supervisor_status: PtySupervisorStatus,
) -> PtyRestoreInfo {
    match restore_info {
        PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            ..
        } => PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid: *supervisor_pid,
            supervisor_status,
        },
        PtyRestoreInfo::Tmux { .. } => restore_info.clone(),
    }
}

fn supervisor_reader_loop(
    mut reader: StdUnixStream,
    pending_requests: Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
    bootstrap_terminal_frames: Arc<StdMutex<Option<VecDeque<PtyTerminalFrame>>>>,
    output_signal_tx: watch::Sender<u64>,
    exited: Arc<AtomicBool>,
) {
    loop {
        let frame: SupervisorFrame = match read_frame_sync(&mut reader) {
            Ok(frame) => frame,
            Err(error) => {
                let message = format!("session supervisor reader stopped: {error}");
                fail_all_pending_requests(&pending_requests, message);
                return;
            }
        };

        match frame {
            SupervisorFrame::Response {
                request_id,
                response,
            } => {
                if let Some(sender) = pending_requests
                    .lock()
                    .expect("supervisor pending request mutex poisoned")
                    .remove(&request_id)
                {
                    let _ = sender.send(SupervisorRequestCompletion::Response(response));
                }
            }
            SupervisorFrame::TerminalFrame { frame } => {
                if matches!(frame, PtyTerminalFrame::Exit { .. }) {
                    exited.store(true, Ordering::Release);
                }
                let mut bootstrap = bootstrap_terminal_frames
                    .lock()
                    .expect("supervisor bootstrap frame mutex poisoned");
                if let Some(buffered) = bootstrap.as_mut() {
                    let retained_bytes = buffered
                        .iter()
                        .map(terminal_frame_retained_bytes)
                        .sum::<usize>();
                    if buffered.len() >= SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES
                        || retained_bytes.saturating_add(terminal_frame_retained_bytes(&frame))
                            > SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
                    {
                        buffered.clear();
                        drop(bootstrap);
                        fail_all_pending_requests(
                            &pending_requests,
                            "session supervisor bootstrap output queue overflow; reconnect to resynchronize"
                                .to_owned(),
                        );
                        return;
                    }
                    buffered.push_back(frame);
                    continue;
                }
                drop(bootstrap);
                if apply_and_enqueue_daemon_terminal_frame(
                    &pending_terminal_frames,
                    &terminal_mirror,
                    frame,
                ) {
                    let next = output_signal_tx.borrow().wrapping_add(1);
                    let _ = output_signal_tx.send(next);
                }
            }
        }
    }
}

fn fail_all_pending_requests(
    pending_requests: &Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    message: String,
) {
    let pending = std::mem::take(
        &mut *pending_requests
            .lock()
            .expect("supervisor pending request mutex poisoned"),
    );
    for (_, sender) in pending {
        let _ = sender.send(SupervisorRequestCompletion::TransportError(
            PtyError::Backend(message.clone()),
        ));
    }
}

fn terminal_frame_retained_bytes(frame: &PtyTerminalFrame) -> usize {
    frame.bytes_for_legacy_read().map_or(1, <[u8]>::len)
}

fn apply_and_enqueue_daemon_terminal_frame(
    pending_terminal_frames: &Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: &Arc<StdMutex<SupervisorTerminalMirror>>,
    frame: PtyTerminalFrame,
) -> bool {
    let applied = terminal_mirror
        .lock()
        .expect("terminal mirror mutex poisoned")
        .apply_frame(&frame);
    if applied {
        enqueue_daemon_terminal_frame(pending_terminal_frames, terminal_mirror, frame);
    }
    applied
}

fn enqueue_daemon_terminal_frame(
    pending_terminal_frames: &Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: &Arc<StdMutex<SupervisorTerminalMirror>>,
    frame: PtyTerminalFrame,
) {
    let frame_bytes = terminal_frame_retained_bytes(&frame);
    let mut pending = pending_terminal_frames
        .lock()
        .expect("pending terminal frames mutex poisoned");
    let retained_bytes = pending
        .iter()
        .map(terminal_frame_retained_bytes)
        .sum::<usize>();
    if pending.len() < SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES
        && retained_bytes.saturating_add(frame_bytes) <= SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
    {
        pending.push_back(frame);
        return;
    }

    pending.clear();
    let snapshot = terminal_mirror
        .lock()
        .expect("terminal mirror mutex poisoned")
        .terminal_snapshot_or_tail(None)
        .1;
    pending.extend(snapshot.into_iter().filter(|snapshot_frame| {
        terminal_frame_retained_bytes(snapshot_frame) <= SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
    }));
}

fn enqueue_daemon_legacy_output(
    pending_output: &Arc<StdMutex<VecDeque<Vec<u8>>>>,
    mut chunk: Vec<u8>,
    at_front: bool,
) {
    if chunk.len() > SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES {
        if at_front {
            chunk.truncate(SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES);
        } else {
            chunk = chunk.split_off(chunk.len() - SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES);
        }
    }
    let mut pending = pending_output
        .lock()
        .expect("pending output mutex poisoned");
    let mut retained_bytes = pending.iter().map(Vec::len).sum::<usize>();
    while pending.len() >= SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES
        || retained_bytes.saturating_add(chunk.len()) > SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
    {
        let removed = if at_front {
            pending.pop_back()
        } else {
            pending.pop_front()
        };
        let Some(removed) = removed else {
            break;
        };
        retained_bytes = retained_bytes.saturating_sub(removed.len());
    }
    if at_front {
        pending.push_front(chunk);
    } else {
        pending.push_back(chunk);
    }
}

fn supervisor_runtime_dir(state_path: &Path) -> Result<PathBuf, String> {
    // 不能放在 /tmp：systemd 的 PrivateTmp 会让 daemon 重启后进入新的 /tmp 视图，
    // 老 session supervisor 还活着，但新 daemon 看不到它的 socket 文件。
    //
    // 目录也不能带 state_path 哈希后缀：安装后的 systemd 服务升级时如果工作目录曾发生过漂移，
    // 同一台机器会留下多套 supervisor 目录，排查时容易出现 “systemctl 看得到进程，Web 看不到
    // session” 的错觉。固定目录名让安装目录下的 supervisor 文件位置稳定、可预期。
    let base_dir = state_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir);

    let preferred = base_dir.join("termd-supervisors");
    let longest_socket_name_len = short_supervisor_socket_file_name("probe")
        .len()
        .max(short_supervisor_attach_socket_file_name("probe").len());
    if preferred.as_os_str().as_bytes().len() + 1 + longest_socket_name_len
        <= UNIX_SOCKET_PATH_MAX_BYTES
    {
        Ok(preferred)
    } else if base_dir.as_os_str().as_bytes().len() + 1 + "ts".len() + 1 + longest_socket_name_len
        <= UNIX_SOCKET_PATH_MAX_BYTES
    {
        // 中文注释：优先保留一个专用 runtime 子目录，避免把 socket 直接混在 state
        // 父目录根下；但这个短目录本身也要重新做长度预算。
        Ok(base_dir.join("ts"))
    } else if base_dir.as_os_str().as_bytes().len() + 1 + "t".len() + 1 + longest_socket_name_len
        <= UNIX_SOCKET_PATH_MAX_BYTES
    {
        Ok(base_dir.join("t"))
    } else {
        // 中文注释：不能把 socket 直接放到 state 父目录，否则后续收紧 runtime 目录权限
        // 会错误地 chmod 共享父目录。没有安全的专用目录时必须在创建 session 前失败。
        Err(format!(
            "cannot construct a safe supervisor runtime directory for state path {}: Unix socket path exceeds {UNIX_SOCKET_PATH_MAX_BYTES} bytes",
            state_path.display()
        ))
    }
}

fn short_supervisor_socket_file_name(session_id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    // 中文注释：Unix domain socket 路径受平台 `sun_path` 长度限制。state 目录本身
    // 可能已经很长，所以文件名必须固定且紧凑，不能直接把完整 session id 拼进去。
    let token = general_purpose::URL_SAFE_NO_PAD.encode(hasher.finish().to_be_bytes());
    format!("{token}.sock")
}

fn short_supervisor_attach_socket_file_name(session_id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    let token = general_purpose::URL_SAFE_NO_PAD.encode(hasher.finish().to_be_bytes());
    format!("{token}.attach.sock")
}

fn attach_socket_path_for_control_socket(control_socket_path: &Path, session_id: &str) -> PathBuf {
    let Some(parent) = control_socket_path.parent() else {
        return control_socket_path.to_path_buf();
    };
    let Some(file_name) = control_socket_path
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return control_socket_path.to_path_buf();
    };
    let attach_name = file_name.replacen(".sock", ".attach.sock", 1);
    let preferred = parent.join(attach_name);
    if preferred.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX_BYTES {
        return preferred;
    }

    // 中文注释：attach socket 文件名比 control socket 更长。长 state 目录下如果继续
    // 直接拼 `.attach.sock`，路径会超过 Unix `sun_path` 限制，首次 watched attach
    // 就会因为 socket 根本没绑定成功而超时。
    parent.join(short_supervisor_attach_socket_file_name(session_id))
}

fn supervisor_processes_from_proc() -> PtyResult<Vec<SupervisorProcess>> {
    let mut supervisors = Vec::new();
    let proc_entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        // 非 Linux 或受限测试环境没有 /proc 时，不应阻断 daemon 启动。
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(supervisors),
        Err(error) => return Err(PtyError::from(error)),
    };

    for entry in proc_entries.flatten() {
        let raw_pid = entry.file_name().to_string_lossy().into_owned();
        if !raw_pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = raw_pid.parse::<u32>() else {
            continue;
        };
        let Ok(raw_cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args = raw_cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).to_string())
            .collect::<Vec<_>>();

        if let Some(supervisor) = parse_supervisor_cmdline(pid, &args) {
            supervisors.push(supervisor);
        }
    }

    Ok(supervisors)
}

fn parse_supervisor_cmdline(pid: u32, args: &[String]) -> Option<SupervisorProcess> {
    if !args.iter().any(|arg| arg == "__session-supervisor") {
        return None;
    }

    let mut session_id = None;
    let mut socket_path = None;
    let mut size = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session-id" => {
                session_id = args.get(index + 1).cloned();
                index += 2;
            }
            "--socket-path" => {
                socket_path = args.get(index + 1).map(PathBuf::from);
                index += 2;
            }
            "--size-base64" => {
                size = args
                    .get(index + 1)
                    .and_then(|raw| general_purpose::STANDARD.decode(raw).ok())
                    .and_then(|bytes| serde_json::from_slice::<PtySize>(&bytes).ok());
                index += 2;
            }
            _ => index += 1,
        }
    }

    Some(SupervisorProcess {
        pid,
        session_id: session_id?,
        socket_path: socket_path?,
        size,
    })
}

#[cfg(target_os = "linux")]
fn linux_supervisor_process(pid: u32) -> PtyResult<Option<SupervisorProcess>> {
    let raw_cmdline = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw_cmdline) => raw_cmdline,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PtyError::from(error)),
    };
    let args = raw_cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).to_string())
        .collect::<Vec<_>>();
    Ok(parse_supervisor_cmdline(pid, &args))
}

#[cfg(target_os = "linux")]
fn probe_reconnected_supervisor_exit(
    session_id: &str,
    restore_info: &PtyRestoreInfo,
) -> PtyResult<Option<super::PtyExitStatus>> {
    let PtyRestoreInfo::UnixSocket {
        socket_path,
        supervisor_pid,
        ..
    } = restore_info
    else {
        return Ok(None);
    };
    let same_supervisor = linux_supervisor_process(*supervisor_pid)?.is_some_and(|process| {
        process.session_id == session_id && process.socket_path == *socket_path
    });
    if same_supervisor {
        Ok(None)
    } else {
        // PID 已复用或 cmdline 已不再属于原 supervisor，原进程事实已经死亡。
        Ok(Some(super::PtyExitStatus::exited(0)))
    }
}

pub(crate) fn supervisor_natural_exit_status(restore_info: &PtyRestoreInfo) -> PtyResult<bool> {
    let PtyRestoreInfo::UnixSocket {
        socket_path,
        supervisor_status: PtySupervisorStatus::Running,
        ..
    } = restore_info
    else {
        return Ok(false);
    };
    let mut stream = StdUnixStream::connect(socket_path).map_err(PtyError::from)?;
    stream
        .set_read_timeout(Some(RECONCILE_IPC_TIMEOUT))
        .map_err(PtyError::from)?;
    stream
        .set_write_timeout(Some(RECONCILE_IPC_TIMEOUT))
        .map_err(PtyError::from)?;
    let exited =
        supervisor_candidate_request(&mut stream, 1, SupervisorRequest::NaturalExitStatus)?
            .into_close_status()?;
    if exited {
        supervisor_candidate_request(&mut stream, 2, SupervisorRequest::FinalizeNaturalExit)?
            .expect_empty()?;
    }
    Ok(exited)
}

fn finalize_supervisor_close_with_capability(
    session_id: &str,
    restore_info: &PtyRestoreInfo,
    operation_id: u64,
    cleanup_capability: &[u8; CLEANUP_CAPABILITY_BYTES],
) -> PtyResult<bool> {
    let PtyRestoreInfo::UnixSocket { socket_path, .. } = restore_info else {
        return Ok(false);
    };
    let mut stream = StdUnixStream::connect(socket_path).map_err(PtyError::from)?;
    stream
        .set_read_timeout(Some(RECONCILE_IPC_TIMEOUT))
        .map_err(PtyError::from)?;
    stream
        .set_write_timeout(Some(RECONCILE_IPC_TIMEOUT))
        .map_err(PtyError::from)?;
    validate_restricted_supervisor_close_target(session_id, restore_info, &stream)?;
    authenticate_restricted_supervisor_close(session_id, &mut stream, cleanup_capability)?;
    let mut confirmed_dead = supervisor_candidate_request(
        &mut stream,
        3,
        SupervisorRequest::CloseStatus { operation_id },
    )?
    .into_close_status()?;
    if !confirmed_dead {
        confirmed_dead = supervisor_candidate_request(
            &mut stream,
            4,
            SupervisorRequest::CleanupClose {
                session_id: session_id.to_owned(),
                operation_id,
            },
        )?
        .into_close_status()?;
    }
    if !confirmed_dead {
        return Ok(false);
    }
    supervisor_candidate_request(
        &mut stream,
        5,
        SupervisorRequest::FinalizeClose { operation_id },
    )?
    .expect_empty()?;
    let deadline = Instant::now() + CLOSE_CONFIRM_TIMEOUT;
    while Instant::now() < deadline {
        if probe_reconnected_supervisor_exit(session_id, restore_info)?.is_some() {
            return Ok(true);
        }
        thread::sleep(CLOSE_CONFIRM_POLL_INTERVAL);
    }
    Err(PtyError::Backend(
        "timed out waiting for finalized session supervisor to exit".to_owned(),
    ))
}

fn authenticate_restricted_supervisor_close(
    session_id: &str,
    stream: &mut StdUnixStream,
    capability: &[u8; CLEANUP_CAPABILITY_BYTES],
) -> PtyResult<()> {
    let mut client_nonce = [0_u8; CLEANUP_AUTH_NONCE_BYTES];
    OsRng.fill_bytes(&mut client_nonce);
    let (server_nonce_base64, server_proof_base64) = supervisor_candidate_request(
        stream,
        1,
        SupervisorRequest::CleanupAuthChallenge {
            session_id: session_id.to_owned(),
            client_nonce_base64: general_purpose::STANDARD.encode(client_nonce),
        },
    )?
    .into_cleanup_auth_challenge()?;
    let server_nonce = decode_cleanup_nonce(&server_nonce_base64)?;
    let supplied_proof = general_purpose::STANDARD
        .decode(&server_proof_base64)
        .map_err(PtyError::backend)?;
    let expected_proof = cleanup_auth_proof(capability, session_id, &client_nonce, &server_nonce);
    if !constant_time_eq(&supplied_proof, &expected_proof) {
        return Err(PtyError::Backend(
            "restricted supervisor cleanup server authentication failed".to_owned(),
        ));
    }
    supervisor_candidate_request(
        stream,
        2,
        SupervisorRequest::CleanupAuthenticate {
            session_id: session_id.to_owned(),
            client_nonce_base64: general_purpose::STANDARD.encode(client_nonce),
            server_nonce_base64,
            capability_base64: general_purpose::STANDARD.encode(capability),
        },
    )?
    .expect_empty()
}

#[cfg(unix)]
fn validate_restricted_supervisor_close_target(
    session_id: &str,
    restore_info: &PtyRestoreInfo,
    stream: &StdUnixStream,
) -> PtyResult<()> {
    let PtyRestoreInfo::UnixSocket {
        socket_path,
        supervisor_pid,
        supervisor_status: PtySupervisorStatus::Closing,
    } = restore_info
    else {
        return Err(PtyError::Backend(
            "restricted supervisor close requires Closing Unix restore metadata".to_owned(),
        ));
    };
    #[cfg(target_os = "linux")]
    {
        let expected_pid = i32::try_from(*supervisor_pid).map_err(PtyError::backend)?;
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut credentials_len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut credentials_len,
            )
        };
        if result != 0 {
            return Err(PtyError::from(io::Error::last_os_error()));
        }
        if credentials_len as usize != std::mem::size_of::<libc::ucred>()
            || credentials.pid != expected_pid
            || credentials.uid != unsafe { libc::geteuid() }
        {
            return Err(PtyError::Backend(
                "restricted supervisor close peer credentials do not match restore metadata"
                    .to_owned(),
            ));
        }
        let process = linux_supervisor_process(*supervisor_pid)?.ok_or_else(|| {
            PtyError::Backend(
                "restricted supervisor close process identity is unavailable".to_owned(),
            )
        })?;
        if process.session_id != session_id || process.socket_path != *socket_path {
            return Err(PtyError::Backend(
                "restricted supervisor close process identity does not match restore metadata"
                    .to_owned(),
            ));
        }
    }

    let parent = socket_path.parent().ok_or_else(|| {
        PtyError::Backend("restricted supervisor socket has no parent directory".to_owned())
    })?;
    let socket_name = socket_path.file_name().ok_or_else(|| {
        PtyError::Backend("restricted supervisor socket has no file name".to_owned())
    })?;
    let runtime_dir = open_secure_runtime_dir(parent, false, false)?;
    runtime_dir.ensure_private()?;
    let socket_name = c_string_from_os_str(socket_name, socket_path)?;
    let socket_file = runtime_dir.open_socket(&socket_name, socket_path)?;
    let metadata = socket_file.metadata().map_err(PtyError::from)?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != SUPERVISOR_SOCKET_MODE
    {
        return Err(PtyError::Backend(
            "restricted supervisor socket ownership or permissions are unsafe".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_restricted_supervisor_close_target(
    _session_id: &str,
    _restore_info: &PtyRestoreInfo,
    _stream: &StdUnixStream,
) -> PtyResult<()> {
    Err(PtyError::Backend(
        "restricted supervisor close peer identity is unsupported on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn unix_pid_is_missing(supervisor_pid: u32) -> PtyResult<bool> {
    let result =
        unsafe { libc::kill(i32::try_from(supervisor_pid).map_err(PtyError::backend)?, 0) };
    if result == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else if error.raw_os_error() == Some(libc::EPERM) {
        Ok(false)
    } else {
        Err(PtyError::from(error))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn supervisor_pid_confirmed_dead(supervisor_pid: u32) -> PtyResult<bool> {
    unix_pid_is_missing(supervisor_pid)
}

#[cfg(unix)]
fn reap_supervisor_child_pid(restore_info: &PtyRestoreInfo) -> PtyResult<()> {
    let PtyRestoreInfo::UnixSocket { supervisor_pid, .. } = restore_info else {
        return Ok(());
    };
    let pid = i32::try_from(*supervisor_pid).map_err(PtyError::backend)?;
    let deadline = Instant::now() + CLOSE_CONFIRM_TIMEOUT;
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) && unix_pid_is_missing(*supervisor_pid)? {
                return Ok(());
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(PtyError::from(error));
            }
        }
        if Instant::now() >= deadline {
            return Err(PtyError::Backend(
                "timed out reaping failed session supervisor launch".to_owned(),
            ));
        }
        thread::sleep(CLOSE_CONFIRM_POLL_INTERVAL);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn probe_reconnected_supervisor_exit(
    _session_id: &str,
    restore_info: &PtyRestoreInfo,
) -> PtyResult<Option<super::PtyExitStatus>> {
    let PtyRestoreInfo::UnixSocket { supervisor_pid, .. } = restore_info else {
        return Ok(None);
    };
    Ok(unix_pid_is_missing(*supervisor_pid)?.then(|| super::PtyExitStatus::exited(0)))
}

#[cfg(not(unix))]
fn probe_reconnected_supervisor_exit(
    _session_id: &str,
    _restore_info: &PtyRestoreInfo,
) -> PtyResult<Option<super::PtyExitStatus>> {
    Err(PtyError::Backend(
        "reconnected supervisor cleanup is unsupported on this platform".to_owned(),
    ))
}

fn orphaned_supervisor_pids(
    runtime_dir: &Path,
    valid_session_ids: &HashSet<String>,
    supervisors: &[SupervisorProcess],
) -> Vec<u32> {
    supervisors
        .iter()
        .filter(|supervisor| supervisor.socket_path.parent() == Some(runtime_dir))
        .filter(|supervisor| !valid_session_ids.contains(&supervisor.session_id))
        .map(|supervisor| supervisor.pid)
        .collect()
}

fn discover_termd_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_termd").map(PathBuf::from)
        && path.exists()
    {
        return path;
    }

    #[cfg(test)]
    if let Some(path) = ensure_fresh_test_termd_binary() {
        return path;
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(candidate) = current_exe
            .parent()
            .and_then(|deps_dir| deps_dir.parent())
            .map(|target_dir| target_dir.join("termd"))
            && candidate.exists()
        {
            return candidate;
        }
        return current_exe;
    }

    PathBuf::from("termd")
}

#[cfg(test)]
fn ensure_fresh_test_termd_binary() -> Option<PathBuf> {
    static TEST_TERMD_BINARY: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

    TEST_TERMD_BINARY
        .get_or_init(|| {
            let current_exe = std::env::current_exe().ok()?;
            let candidate = current_exe
                .parent()
                .and_then(|deps_dir| deps_dir.parent())
                .map(|target_dir| target_dir.join("termd"))?;

            let current_mtime = fs::metadata(&current_exe)
                .and_then(|meta| meta.modified())
                .ok();
            let candidate_mtime = fs::metadata(&candidate)
                .and_then(|meta| meta.modified())
                .ok();
            let needs_build = !candidate.exists()
                || match (candidate_mtime, current_mtime) {
                    (Some(candidate_mtime), Some(current_mtime)) => candidate_mtime < current_mtime,
                    _ => true,
                };

            if needs_build {
                // 中文注释：lib 单测运行时，`target/debug/termd` 不一定会随着当前 test
                // binary 一起重编。supervisor 子进程如果继续启动旧二进制，就不会带上本次
                // 改动，attach socket 等新行为会在测试里表现成莫名其妙的 runtime_failed。
                let status = ProcessCommand::new("cargo")
                    .args(["build", "--bin", "termd", "--quiet"])
                    .current_dir(env!("CARGO_MANIFEST_DIR"))
                    .status()
                    .ok()?;
                if !status.success() {
                    return None;
                }
            }

            candidate.exists().then_some(candidate)
        })
        .clone()
}

/// 子 supervisor 进程的启动参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSupervisorArgs {
    pub session_id: String,
    pub socket_path: PathBuf,
    pub command: CommandSpec,
    pub size: PtySize,
}

#[derive(Clone)]
struct SupervisorShared {
    session_id: Arc<String>,
    session: Arc<Mutex<Box<dyn PtySession>>>,
    state: Arc<Mutex<SupervisorState>>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Clone, Copy)]
struct PendingCleanupAuthentication {
    client_nonce: [u8; CLEANUP_AUTH_NONCE_BYTES],
    server_nonce: [u8; CLEANUP_AUTH_NONCE_BYTES],
}

#[derive(Default)]
struct SupervisorOutputQueueUsage {
    messages: usize,
    bytes: usize,
}

#[derive(Default)]
struct SupervisorOutputQueueBudget {
    usage: StdMutex<SupervisorOutputQueueUsage>,
}

impl SupervisorOutputQueueBudget {
    fn reserve(self: &Arc<Self>, retained_bytes: usize) -> Option<SupervisorOutputReservation> {
        let mut usage = self
            .usage
            .lock()
            .expect("supervisor output queue budget mutex poisoned");
        let messages = usage.messages.checked_add(1)?;
        let bytes = usage.bytes.checked_add(retained_bytes)?;
        if messages > SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES
            || bytes > SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES
        {
            return None;
        }
        usage.messages = messages;
        usage.bytes = bytes;
        Some(SupervisorOutputReservation {
            budget: Arc::clone(self),
            retained_bytes,
        })
    }
}

struct SupervisorOutputReservation {
    budget: Arc<SupervisorOutputQueueBudget>,
    retained_bytes: usize,
}

impl Drop for SupervisorOutputReservation {
    fn drop(&mut self) {
        let mut usage = self
            .budget
            .usage
            .lock()
            .expect("supervisor output queue budget mutex poisoned");
        usage.messages = usage.messages.saturating_sub(1);
        usage.bytes = usage.bytes.saturating_sub(self.retained_bytes);
    }
}

struct QueuedSupervisorOutput<T> {
    value: Option<T>,
    _reservation: SupervisorOutputReservation,
}

impl<T> QueuedSupervisorOutput<T> {
    fn value(&self) -> &T {
        self.value
            .as_ref()
            .expect("queued supervisor output should contain a value")
    }

    fn into_inner(mut self) -> T {
        self.value
            .take()
            .expect("queued supervisor output should contain a value")
    }
}

struct BoundedSupervisorOutputSender<T> {
    tx: tokio_mpsc::Sender<QueuedSupervisorOutput<T>>,
    budget: Arc<SupervisorOutputQueueBudget>,
    overflow_tx: watch::Sender<bool>,
}

impl<T> Clone for BoundedSupervisorOutputSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            budget: Arc::clone(&self.budget),
            overflow_tx: self.overflow_tx.clone(),
        }
    }
}

impl<T> BoundedSupervisorOutputSender<T> {
    fn try_send(&self, value: T, retained_bytes: usize) -> Result<(), ()> {
        let Some(reservation) = self.budget.reserve(retained_bytes) else {
            let _ = self.overflow_tx.send(true);
            return Err(());
        };

        let queued = QueuedSupervisorOutput {
            value: Some(value),
            _reservation: reservation,
        };
        if self.tx.try_send(queued).is_err() {
            let _ = self.overflow_tx.send(true);
            return Err(());
        }
        Ok(())
    }
}

struct BoundedSupervisorOutputReceiver<T> {
    rx: tokio_mpsc::Receiver<QueuedSupervisorOutput<T>>,
}

impl<T> BoundedSupervisorOutputReceiver<T> {
    async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await.map(QueuedSupervisorOutput::into_inner)
    }

    fn try_recv(&mut self) -> Result<T, tokio_mpsc::error::TryRecvError> {
        self.rx.try_recv().map(QueuedSupervisorOutput::into_inner)
    }

    async fn recv_reserved(&mut self) -> Option<QueuedSupervisorOutput<T>> {
        self.rx.recv().await
    }
}

fn bounded_supervisor_output_channel<T>() -> (
    BoundedSupervisorOutputSender<T>,
    BoundedSupervisorOutputReceiver<T>,
    watch::Receiver<bool>,
) {
    bounded_supervisor_output_channel_with_budget(Arc::new(SupervisorOutputQueueBudget::default()))
}

fn bounded_supervisor_output_channel_with_budget<T>(
    budget: Arc<SupervisorOutputQueueBudget>,
) -> (
    BoundedSupervisorOutputSender<T>,
    BoundedSupervisorOutputReceiver<T>,
    watch::Receiver<bool>,
) {
    let (tx, rx) = tokio_mpsc::channel(SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES);
    let (overflow_tx, overflow_rx) = watch::channel(false);
    (
        BoundedSupervisorOutputSender {
            tx,
            budget,
            overflow_tx,
        },
        BoundedSupervisorOutputReceiver { rx },
        overflow_rx,
    )
}

struct SupervisorState {
    cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    cleanup_capability_replaceable: bool,
    next_controller_id: u64,
    next_controller_connection_id: u64,
    active_controller_id: Option<u64>,
    active_controller_connection_id: Option<u64>,
    controller_resume_lease_id: Option<u64>,
    committed_resume_transactions: HashMap<u64, ControllerIdentity>,
    confirmed_close_operation_id: Option<u64>,
    exited: bool,
    controllers: HashMap<u64, ControllerHandle>,
    next_terminal_attach_id: u64,
    terminal_attaches: HashMap<u64, TerminalAttachHandle>,
    attached_devices: HashSet<String>,
    terminal: SupervisorTerminalCache,
}

impl SupervisorState {
    #[cfg(test)]
    fn new(size: PtySize) -> Self {
        Self::with_cleanup_capability(size, random_cleanup_capability())
    }

    #[cfg(test)]
    fn with_cleanup_capability(
        size: PtySize,
        cleanup_capability: [u8; CLEANUP_CAPABILITY_BYTES],
    ) -> Self {
        Self::with_optional_cleanup_capability(size, Some(cleanup_capability))
    }

    fn with_optional_cleanup_capability(
        size: PtySize,
        cleanup_capability: Option<[u8; CLEANUP_CAPABILITY_BYTES]>,
    ) -> Self {
        let cleanup_capability_replaceable = cleanup_capability.is_none();
        Self {
            cleanup_capability,
            cleanup_capability_replaceable,
            next_controller_id: 1,
            next_controller_connection_id: 1,
            active_controller_id: None,
            active_controller_connection_id: None,
            controller_resume_lease_id: None,
            committed_resume_transactions: HashMap::new(),
            confirmed_close_operation_id: None,
            exited: false,
            controllers: HashMap::new(),
            next_terminal_attach_id: 1,
            terminal_attaches: HashMap::new(),
            attached_devices: HashSet::new(),
            terminal: SupervisorTerminalCache::new(size),
        }
    }

    fn attach_device(&mut self, device_id: &str) {
        self.attached_devices.insert(device_id.to_owned());
    }

    fn detach_device(&mut self, device_id: &str) {
        self.attached_devices.remove(device_id);
    }

    fn has_attached_device(&self, device_id: &str) -> bool {
        self.attached_devices.contains(device_id)
    }

    fn record_output(&mut self, bytes: &[u8]) -> PtyTerminalFrame {
        self.terminal.record_output(bytes)
    }

    fn resize(&mut self, size: PtySize) -> PtyTerminalFrame {
        self.terminal.resize(size)
    }

    fn record_exit(&mut self, code: Option<i32>) -> PtyTerminalFrame {
        self.exited = true;
        self.terminal.record_exit(code)
    }

    fn snapshot_output(&self) -> Vec<u8> {
        self.terminal.snapshot_output()
    }

    fn terminal_snapshot_or_tail_with_base(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        self.terminal.terminal_snapshot_or_tail(last_terminal_seq)
    }

    fn size(&self) -> PtySize {
        self.terminal.size()
    }

    fn allocate_controller_connection_id(&mut self) -> u64 {
        let connection_id = self.next_controller_connection_id;
        self.next_controller_connection_id = self.next_controller_connection_id.saturating_add(1);
        connection_id
    }

    fn build_attach_sync_payload(
        &self,
        controller_id: u64,
        controller_connection_id: u64,
        process_id: Option<u32>,
        last_terminal_seq: Option<u64>,
    ) -> SupervisorAttachSyncPayload {
        let snapshot = SupervisorSnapshotPayload {
            size: self.size(),
            process_id,
            exited: self.exited,
            // 中文注释：terminal attach 的屏幕内容只能走 sequenced frame。
            // retained_output 是 legacy snapshot 字段；如果这里和 frames.snapshot 同时
            // 携带首屏，Web 端会把同一份 prompt 重放两次。
            retained_output: Vec::new(),
        };
        let (base_seq, frames) = self.terminal_snapshot_or_tail_with_base(last_terminal_seq);
        SupervisorAttachSyncPayload {
            controller_id,
            controller_connection_id,
            snapshot,
            base_seq,
            frames,
        }
    }

    fn attach_sync(
        &mut self,
        controller_tx: BoundedSupervisorOutputSender<SupervisorFrame>,
        process_id: Option<u32>,
        last_terminal_seq: Option<u64>,
    ) -> (ControllerIdentity, SupervisorAttachSyncPayload) {
        let id = self.next_controller_id;
        let connection_id = self.allocate_controller_connection_id();
        self.next_controller_id = self.next_controller_id.saturating_add(1);
        self.controllers.insert(
            id,
            ControllerHandle {
                tx: controller_tx,
                connection_id,
            },
        );
        // 中文注释：最新 attach 的 daemon controller 才有资格继续作为控制面 owner。
        // 旧 controller 可以暂时残留到连接自然收尾，但它们不能再修改 authority 或驱动 PTY。
        self.active_controller_id = Some(id);
        self.active_controller_connection_id = Some(connection_id);
        self.controller_resume_lease_id = Some(id);
        (
            ControllerIdentity { id, connection_id },
            self.build_attach_sync_payload(id, connection_id, process_id, last_terminal_seq),
        )
    }

    fn resume_attach_sync(
        &mut self,
        controller_id: u64,
        process_id: Option<u32>,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<(ControllerIdentity, SupervisorAttachSyncPayload)> {
        if controller_id == 0 || controller_id >= self.next_controller_id {
            return Err(PtyError::Backend(
                "session supervisor connection is not the active controller".to_owned(),
            ));
        }
        let Some(lease_controller_id) = self.controller_resume_lease_id else {
            return Err(PtyError::Backend(
                "session supervisor connection is not the active controller".to_owned(),
            ));
        };
        if lease_controller_id != controller_id {
            return Err(PtyError::Backend(
                "session supervisor connection is not the active controller".to_owned(),
            ));
        }
        if let Some(active_controller_id) = self.active_controller_id
            && active_controller_id != controller_id
        {
            return Err(PtyError::Backend(
                "session supervisor connection is not the active controller".to_owned(),
            ));
        }

        // 候选连接先取得一个未提交的 connection id，用于验证 ping/snapshot；此时旧
        // controller 仍是唯一 owner，attached authority 和 controller map 均不变。
        let connection_id = self.allocate_controller_connection_id();
        Ok((
            ControllerIdentity {
                id: controller_id,
                connection_id,
            },
            self.build_attach_sync_payload(
                controller_id,
                connection_id,
                process_id,
                last_terminal_seq,
            ),
        ))
    }

    fn commit_controller_resume(
        &mut self,
        transaction_id: u64,
        candidate: ControllerIdentity,
        controller_tx: BoundedSupervisorOutputSender<SupervisorFrame>,
    ) -> PtyResult<ControllerIdentity> {
        if let Some(committed) = self
            .committed_resume_transactions
            .get(&transaction_id)
            .copied()
        {
            if committed.id != candidate.id {
                return Err(PtyError::Backend(
                    "session supervisor resume transaction mismatch".to_owned(),
                ));
            }
            self.controllers.insert(
                committed.id,
                ControllerHandle {
                    tx: controller_tx,
                    connection_id: committed.connection_id,
                },
            );
            self.active_controller_id = Some(committed.id);
            self.active_controller_connection_id = Some(committed.connection_id);
            return Ok(committed);
        }
        if self.controller_resume_lease_id != Some(candidate.id)
            || self
                .active_controller_id
                .is_some_and(|active_id| active_id != candidate.id)
        {
            return Err(PtyError::Backend(
                "session supervisor connection is not the active controller".to_owned(),
            ));
        }
        self.controllers.insert(
            candidate.id,
            ControllerHandle {
                tx: controller_tx,
                connection_id: candidate.connection_id,
            },
        );
        self.active_controller_id = Some(candidate.id);
        self.active_controller_connection_id = Some(candidate.connection_id);
        self.committed_resume_transactions
            .insert(transaction_id, candidate);
        Ok(candidate)
    }

    fn committed_controller_resume(&self, transaction_id: u64) -> Option<ControllerIdentity> {
        self.committed_resume_transactions
            .get(&transaction_id)
            .copied()
    }

    fn is_active_controller(&self, controller: ControllerIdentity) -> bool {
        self.active_controller_id == Some(controller.id)
            && self.active_controller_connection_id == Some(controller.connection_id)
            && self
                .controllers
                .get(&controller.id)
                .is_some_and(|handle| handle.connection_id == controller.connection_id)
    }

    fn remove_controller(&mut self, controller: ControllerIdentity) {
        let should_remove = self
            .controllers
            .get(&controller.id)
            .is_some_and(|handle| handle.connection_id == controller.connection_id);
        if should_remove {
            self.controllers.remove(&controller.id);
        }
        if self.active_controller_id == Some(controller.id)
            && self.active_controller_connection_id == Some(controller.connection_id)
        {
            self.active_controller_id = None;
            self.active_controller_connection_id = None;
        }
    }

    fn register_terminal_attach(
        &mut self,
        tx: BoundedSupervisorOutputSender<SupervisorTerminalServerFrame>,
    ) -> u64 {
        let id = self.next_terminal_attach_id;
        self.next_terminal_attach_id = self.next_terminal_attach_id.saturating_add(1);
        self.terminal_attaches
            .insert(id, TerminalAttachHandle { tx });
        id
    }

    fn remove_terminal_attach(&mut self, attach_id: u64) {
        self.terminal_attaches.remove(&attach_id);
    }

    fn broadcast_terminal_frame(&mut self, session_id: &str, frame: PtyTerminalFrame) {
        let mut closed = Vec::new();
        for (id, controller) in &self.controllers {
            if controller
                .tx
                .try_send(
                    SupervisorFrame::TerminalFrame {
                        frame: frame.clone(),
                    },
                    terminal_frame_retained_bytes(&frame),
                )
                .is_err()
            {
                closed.push(*id);
            }
        }

        // 中文注释：发送失败代表对应 daemon IPC 已经断开，立即清理，避免后续广播继续
        // 在无效 receiver 上做无意义工作。
        for id in closed {
            if let Some(handle) = self.controllers.get(&id) {
                self.remove_controller(ControllerIdentity {
                    id,
                    connection_id: handle.connection_id,
                });
            }
        }

        let mut closed_terminal_attaches = Vec::new();
        for (attach_id, attach) in &self.terminal_attaches {
            if attach
                .tx
                .try_send(
                    SupervisorTerminalServerFrame::TerminalFrame {
                        session_id: session_id.to_owned(),
                        frame: frame.clone(),
                    },
                    terminal_frame_retained_bytes(&frame),
                )
                .is_err()
            {
                closed_terminal_attaches.push(*attach_id);
            }
        }
        for attach_id in closed_terminal_attaches {
            self.remove_terminal_attach(attach_id);
        }
    }
}

#[derive(Clone)]
struct ControllerHandle {
    tx: BoundedSupervisorOutputSender<SupervisorFrame>,
    connection_id: u64,
}

#[derive(Clone)]
struct TerminalAttachHandle {
    tx: BoundedSupervisorOutputSender<SupervisorTerminalServerFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControllerIdentity {
    id: u64,
    connection_id: u64,
}

/// supervisor 入口，由主二进制的隐藏子命令调用。
pub async fn run_session_supervisor(args: SessionSupervisorArgs) -> PtyResult<()> {
    let attach_socket_path =
        attach_socket_path_for_control_socket(&args.socket_path, &args.session_id);
    // 所有目录、权限和 socket publish 能力必须在启动 PTY 前验证完毕。
    let mut listener = bind_supervisor_listener(&args.socket_path, true)?;
    let mut attach_listener = match bind_supervisor_listener(&attach_socket_path, true) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = remove_supervisor_socket_if_current(&listener);
            return Err(error);
        }
    };
    let startup_grant = read_and_validate_startup_grant(
        &mut io::stdin(),
        &args.session_id,
        &args.socket_path,
        std::process::id(),
    );
    let startup_grant = match startup_grant {
        Ok(grant) => grant,
        Err(error) => {
            let _ = remove_supervisor_socket_if_current(&listener);
            let _ = remove_supervisor_socket_if_current(&attach_listener);
            return Err(error);
        }
    };
    crate::session_ownership::test_crash_checkpoint("after_grant_before_pty");
    let backend = NonBlockingPortablePtyBackend::new();
    let session = match backend.spawn(&args.command, args.size) {
        Ok(session) => session,
        Err(error) => {
            let _ = remove_supervisor_socket_if_current(&listener);
            let _ = remove_supervisor_socket_if_current(&attach_listener);
            return Err(error);
        }
    };
    crate::session_ownership::test_crash_checkpoint("after_pty_start");
    let session = Arc::new(Mutex::new(session));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let supervisor_state = SupervisorState::with_optional_cleanup_capability(
        args.size,
        Some(startup_grant.cleanup_capability),
    );
    let shared = SupervisorShared {
        session_id: Arc::new(args.session_id.clone()),
        session: Arc::clone(&session),
        state: Arc::new(Mutex::new(supervisor_state)),
        shutdown_tx,
    };

    if let Some(signal) = {
        let session = shared.session.lock().await;
        session.output_signal()
    } {
        tokio::spawn(supervisor_output_pump(shared.clone(), signal));
    }
    tokio::spawn(supervisor_exit_watcher(shared.clone()));

    loop {
        if let Err(error) = ensure_supervisor_socket_bound(&args.socket_path, &mut listener) {
            // supervisor 的首要职责是保住 PTY；socket 修复失败只能降级为告警，
            // 不能让用户正在跑的 shell 因为一个控制面入口文件异常而退出。
            tracing::warn!(%error, socket_path = %args.socket_path.display(), "failed to repair session supervisor socket");
        }
        if let Err(error) =
            ensure_supervisor_socket_bound(&attach_socket_path, &mut attach_listener)
        {
            tracing::warn!(%error, socket_path = %attach_socket_path.display(), "failed to repair session supervisor attach socket");
        }

        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = tokio::time::timeout(SUPERVISOR_SOCKET_REPAIR_INTERVAL, listener.listener.accept()) => {
                match accepted {
                    Ok(Ok((stream, _))) => {
                        let shared = shared.clone();
                        let expected_session_id = args.session_id.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_supervisor_connection(shared, expected_session_id, stream).await {
                                tracing::warn!(%error, "session supervisor connection failed");
                            }
                        });
                    }
                    Ok(Err(error)) => return Err(PtyError::from(error)),
                    Err(_) => {}
                }
            }
            accepted = tokio::time::timeout(SUPERVISOR_SOCKET_REPAIR_INTERVAL, attach_listener.listener.accept()) => {
                match accepted {
                    Ok(Ok((stream, _))) => {
                        let shared = shared.clone();
                        let expected_session_id = args.session_id.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_supervisor_terminal_attach_connection(shared, expected_session_id, stream).await {
                                tracing::warn!(%error, "session supervisor terminal attach connection failed");
                            }
                        });
                    }
                    Ok(Err(error)) => return Err(PtyError::from(error)),
                    Err(_) => {}
                }
            }
        }
    }

    let _ = remove_supervisor_socket_if_current(&listener);
    let _ = remove_supervisor_socket_if_current(&attach_listener);
    Ok(())
}

fn bind_supervisor_listener(
    socket_path: &Path,
    remove_existing: bool,
) -> PtyResult<BoundSupervisorListener> {
    let parent = socket_path.parent().ok_or_else(|| {
        PtyError::Backend(format!(
            "supervisor socket path has no runtime directory: {}",
            socket_path.display()
        ))
    })?;
    let socket_name = socket_path.file_name().ok_or_else(|| {
        PtyError::Backend(format!(
            "supervisor socket path has no file name: {}",
            socket_path.display()
        ))
    })?;
    let runtime_dir = SecureRuntimeDir::open(parent)?;
    let socket_name = c_string_from_os_str(socket_name, socket_path)?;

    let listener = bind_supervisor_listener_in_runtime_dir(
        socket_path,
        runtime_dir,
        socket_name,
        remove_existing,
    )?;
    if let Err(error) = listener.runtime_dir.ensure_path_matches() {
        let _ = remove_supervisor_socket_if_current(&listener);
        return Err(error);
    }
    Ok(listener)
}

fn bind_supervisor_listener_in_runtime_dir(
    socket_path: &Path,
    runtime_dir: SecureRuntimeDir,
    socket_name: CString,
    remove_existing: bool,
) -> PtyResult<BoundSupervisorListener> {
    bind_supervisor_listener_in_runtime_dir_with_publish(
        socket_path,
        runtime_dir,
        socket_name,
        remove_existing,
        link_supervisor_socket_noreplace,
    )
}

fn bind_supervisor_listener_in_runtime_dir_with_publish(
    socket_path: &Path,
    runtime_dir: SecureRuntimeDir,
    socket_name: CString,
    remove_existing: bool,
    publish: fn(&SecureRuntimeDir, &CString, &CString) -> io::Result<()>,
) -> PtyResult<BoundSupervisorListener> {
    runtime_dir.ensure_private()?;
    runtime_dir.ensure_path_matches()?;
    if socket_path.as_os_str().as_bytes().len() > UNIX_SOCKET_PATH_MAX_BYTES {
        return Err(PtyError::Backend(format!(
            "cannot bind supervisor socket at {}: Unix socket path exceeds {UNIX_SOCKET_PATH_MAX_BYTES} bytes",
            socket_path.display()
        )));
    }
    if remove_existing {
        remove_existing_supervisor_socket(&runtime_dir, &socket_name, socket_path)?;
    }

    let (listener, temp_name, temp_path, temp_file) =
        bind_supervisor_temp_listener(&runtime_dir, socket_path)?;
    let temp_identity = supervisor_path_identity(&temp_file)?;
    let socket_file = match (|| {
        restrict_supervisor_socket_permissions(
            &runtime_dir,
            &temp_name,
            &temp_path,
            temp_identity,
        )?;
        publish(&runtime_dir, &temp_name, &socket_name).map_err(|error| {
                PtyError::Backend(format!(
                    "cannot publish supervisor socket at {} without replacing an existing entry: {error}",
                    socket_path.display()
                ))
            })?;
        let socket_file = runtime_dir.open_socket(&socket_name, socket_path)?;
        if supervisor_path_identity(&socket_file)? != temp_identity {
            return Err(PtyError::Backend(format!(
                "published supervisor socket identity mismatch: {}",
                socket_path.display()
            )));
        }
        runtime_dir.unlink_if_identity(&temp_name, &temp_path, temp_identity)?;
        Ok(socket_file)
    })() {
        Ok(file) => file,
        Err(error) => {
            let _ = runtime_dir.unlink_if_identity(&socket_name, socket_path, temp_identity);
            let _ = runtime_dir.unlink_if_identity(&temp_name, &temp_path, temp_identity);
            return Err(error);
        }
    };

    Ok(BoundSupervisorListener {
        listener,
        runtime_dir,
        socket_file,
        socket_name,
        socket_path: socket_path.to_path_buf(),
    })
}

fn ensure_supervisor_socket_bound(
    socket_path: &Path,
    listener: &mut BoundSupervisorListener,
) -> PtyResult<()> {
    listener.runtime_dir.ensure_path_matches()?;
    listener.runtime_dir.ensure_private()?;

    match listener
        .runtime_dir
        .open_socket(&listener.socket_name, socket_path)
    {
        Ok(socket_file) => {
            if supervisor_path_identity(&socket_file)?
                != supervisor_path_identity(&listener.socket_file)?
            {
                return Err(PtyError::Backend(format!(
                    "unsafe session supervisor socket path was replaced: {}",
                    socket_path.display()
                )));
            }
            return Ok(());
        }
        Err(PtyError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Unix socket 的路径名被外部 unlink 后，已 accept 的连接仍然可用，但新的 daemon
    // 无法再按路径 attach。重新 bind 同一路径可以保留原 PTY，并恢复后续重连入口。
    let rebound = bind_supervisor_listener_in_runtime_dir(
        socket_path,
        listener.runtime_dir.try_clone()?,
        listener.socket_name.clone(),
        false,
    )?;
    if let Err(error) = rebound.runtime_dir.ensure_path_matches() {
        let _ = remove_supervisor_socket_if_current(&rebound);
        return Err(error);
    }
    *listener = rebound;
    tracing::warn!(
        socket_path = %socket_path.display(),
        "session supervisor socket path was missing; rebound listener"
    );
    Ok(())
}

fn restrict_supervisor_socket_permissions(
    runtime_dir: &SecureRuntimeDir,
    socket_name: &CString,
    path: &Path,
    expected_identity: SupervisorPathIdentity,
) -> PtyResult<()> {
    runtime_dir.ensure_private()?;
    // temp 名由 96-bit CSPRNG 生成，且 runtime dir 仅 euid 可写；权限修改前后均复核 inode。
    let result = unsafe {
        libc::fchmodat(
            runtime_dir.file.as_raw_fd(),
            socket_name.as_ptr(),
            SUPERVISOR_SOCKET_MODE as libc::mode_t,
            0,
        )
    };
    if result != 0 {
        return Err(PtyError::Backend(format!(
            "failed to restrict supervisor socket permissions at {}: {}",
            path.display(),
            io::Error::last_os_error()
        )));
    }
    let socket_file = runtime_dir.open_socket(socket_name, path)?;
    let metadata = socket_file.metadata().map_err(PtyError::from)?;
    if supervisor_path_identity(&socket_file)? != expected_identity
        || metadata.mode() & 0o777 != SUPERVISOR_SOCKET_MODE
    {
        return Err(PtyError::Backend(format!(
            "supervisor socket changed while restricting permissions: {}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_existing_supervisor_socket(
    runtime_dir: &SecureRuntimeDir,
    socket_name: &CString,
    socket_path: &Path,
) -> PtyResult<()> {
    match runtime_dir.open_socket(socket_name, socket_path) {
        Ok(socket_file) => {
            let socket_identity = supervisor_path_identity(&socket_file)?;
            runtime_dir.unlink_if_identity(socket_name, socket_path, socket_identity)
        }
        Err(PtyError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_supervisor_socket_if_current(listener: &BoundSupervisorListener) -> PtyResult<()> {
    listener.runtime_dir.unlink_if_identity(
        &listener.socket_name,
        &listener.socket_path,
        supervisor_path_identity(&listener.socket_file)?,
    )
}

impl SecureRuntimeDir {
    fn open(path: &Path) -> PtyResult<Self> {
        open_secure_runtime_dir(path, true, true)
    }

    fn try_clone(&self) -> PtyResult<Self> {
        Ok(Self {
            file: self.file.try_clone().map_err(PtyError::from)?,
            identity: self.identity,
            path: self.path.clone(),
            effective_uid: self.effective_uid,
        })
    }

    fn ensure_path_matches(&self) -> PtyResult<()> {
        let current = open_secure_runtime_dir(&self.path, false, false)?;
        if current.identity == self.identity {
            Ok(())
        } else {
            Err(PtyError::Backend(format!(
                "unsafe supervisor runtime directory was replaced: {}",
                self.path.display()
            )))
        }
    }

    fn ensure_private(&self) -> PtyResult<()> {
        let metadata = self.file.metadata().map_err(PtyError::from)?;
        if metadata.uid() != self.effective_uid
            || metadata.mode() & 0o777 != SUPERVISOR_RUNTIME_DIR_MODE
        {
            return Err(PtyError::Backend(format!(
                "unsafe supervisor runtime directory ownership or permissions: {}",
                self.path.display()
            )));
        }
        Ok(())
    }

    fn open_socket(&self, name: &CString, path: &Path) -> PtyResult<fs::File> {
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ELOOP) {
                Err(PtyError::Backend(format!(
                    "unsafe supervisor socket symlink: {}",
                    path.display()
                )))
            } else {
                Err(PtyError::from(error))
            };
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(PtyError::from)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
            return Err(PtyError::Backend(format!(
                "unsafe supervisor socket entry: {}",
                path.display()
            )));
        }
        Ok(file)
    }

    fn unlink_if_identity(
        &self,
        name: &CString,
        path: &Path,
        expected_identity: SupervisorPathIdentity,
    ) -> PtyResult<()> {
        self.ensure_private()?;
        let current = self.open_socket(name, path)?;
        if supervisor_path_identity(&current)? != expected_identity {
            return Err(PtyError::Backend(format!(
                "unsafe session supervisor socket path was replaced: {}",
                path.display()
            )));
        }
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(PtyError::from(io::Error::last_os_error()))
        }
    }
}

fn open_secure_runtime_dir(
    path: &Path,
    create_missing: bool,
    restrict_final: bool,
) -> PtyResult<SecureRuntimeDir> {
    let effective_uid = unsafe { libc::geteuid() };
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => components.push(name),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(PtyError::Backend(format!(
                    "unsafe supervisor runtime directory path: {}",
                    path.display()
                )));
            }
        }
    }
    if components.is_empty() {
        return Err(PtyError::Backend(format!(
            "unsafe supervisor runtime directory path: {}",
            path.display()
        )));
    }

    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut current = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(start)
        .map_err(PtyError::from)?;
    validate_supervisor_runtime_ancestor(&current, path, effective_uid)?;

    for (index, component) in components.iter().enumerate() {
        let name = c_string_from_os_str(component, path)?;
        let is_final = index + 1 == components.len();
        let next = match open_supervisor_directory_at(&current, &name, path) {
            Ok(directory) => directory,
            Err(PtyError::Io(error))
                if create_missing && error.kind() == io::ErrorKind::NotFound =>
            {
                let result = unsafe {
                    libc::mkdirat(
                        current.as_raw_fd(),
                        name.as_ptr(),
                        SUPERVISOR_RUNTIME_DIR_MODE as libc::mode_t,
                    )
                };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(PtyError::from(error));
                    }
                }
                open_supervisor_directory_at(&current, &name, path)?
            }
            Err(error) => return Err(error),
        };

        if is_final {
            validate_supervisor_runtime_final(&next, path, effective_uid)?;
            if restrict_final {
                restrict_supervisor_runtime_dir_permissions(&next, path, effective_uid)?;
            }
            let identity = supervisor_path_identity(&next)?;
            return Ok(SecureRuntimeDir {
                file: next,
                identity,
                path: path.to_path_buf(),
                effective_uid,
            });
        }
        validate_supervisor_runtime_ancestor(&next, path, effective_uid)?;
        current = next;
    }
    unreachable!("runtime path components are non-empty")
}

fn open_supervisor_directory_at(
    parent: &fs::File,
    name: &CString,
    path: &Path,
) -> PtyResult<fs::File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    } else {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ELOOP) | Some(libc::ENOTDIR)
        ) {
            Err(PtyError::Backend(format!(
                "unsafe supervisor runtime directory component: {}",
                path.display()
            )))
        } else {
            Err(PtyError::from(error))
        }
    }
}

fn validate_supervisor_runtime_ancestor(
    directory: &fs::File,
    path: &Path,
    effective_uid: u32,
) -> PtyResult<()> {
    let metadata = directory.metadata().map_err(PtyError::from)?;
    let owner_is_trusted = metadata.uid() == effective_uid || metadata.uid() == 0;
    let foreign_writable = metadata.mode() & 0o022 != 0;
    let sticky = metadata.mode() & libc::S_ISVTX != 0;
    if !metadata.is_dir() || !owner_is_trusted || foreign_writable && !sticky {
        return Err(PtyError::Backend(format!(
            "unsafe replaceable supervisor runtime directory ancestor: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_supervisor_runtime_final(
    directory: &fs::File,
    path: &Path,
    effective_uid: u32,
) -> PtyResult<()> {
    let metadata = directory.metadata().map_err(PtyError::from)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid {
        return Err(PtyError::Backend(format!(
            "unsafe foreign-owned supervisor runtime directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn restrict_supervisor_runtime_dir_permissions(
    runtime_dir: &fs::File,
    path: &Path,
    effective_uid: u32,
) -> PtyResult<()> {
    // owner 复核必须紧邻 fchmod，foreign-owned inode 永远不得被修改。
    validate_supervisor_runtime_final(runtime_dir, path, effective_uid)?;
    let result = unsafe {
        libc::fchmod(
            runtime_dir.as_raw_fd(),
            SUPERVISOR_RUNTIME_DIR_MODE as libc::mode_t,
        )
    };
    if result != 0 {
        return Err(PtyError::Backend(format!(
            "failed to restrict supervisor runtime directory permissions at {}: {}",
            path.display(),
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn bind_supervisor_temp_listener(
    runtime_dir: &SecureRuntimeDir,
    socket_path: &Path,
) -> PtyResult<(UnixListener, CString, PathBuf, fs::File)> {
    bind_supervisor_temp_listener_with_name(
        runtime_dir,
        socket_path,
        random_supervisor_socket_temp_name,
    )
}

fn bind_supervisor_temp_listener_with_name<F>(
    runtime_dir: &SecureRuntimeDir,
    socket_path: &Path,
    mut next_name: F,
) -> PtyResult<(UnixListener, CString, PathBuf, fs::File)>
where
    F: FnMut() -> PtyResult<CString>,
{
    for _ in 0..SUPERVISOR_SOCKET_TEMP_BIND_ATTEMPTS {
        let temp_name = next_name()?;
        let temp_path = runtime_dir
            .path
            .join(OsStr::from_bytes(temp_name.as_bytes()));
        let socket_fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if socket_fd < 0 {
            return Err(PtyError::from(io::Error::last_os_error()));
        }
        let std_listener = unsafe { StdUnixListener::from_raw_fd(socket_fd) };
        match bind_unix_socket_at(
            runtime_dir.file.as_raw_fd(),
            std_listener.as_raw_fd(),
            &temp_name,
        ) {
            Ok(()) => {
                let temp_file = runtime_dir.open_socket(&temp_name, &temp_path)?;
                if unsafe { libc::listen(std_listener.as_raw_fd(), 128) } != 0 {
                    let error = io::Error::last_os_error();
                    let identity = supervisor_path_identity(&temp_file)?;
                    let _ = runtime_dir.unlink_if_identity(&temp_name, &temp_path, identity);
                    return Err(PtyError::from(error));
                }
                let listener = UnixListener::from_std(std_listener).map_err(PtyError::from)?;
                return Ok((listener, temp_name, temp_path, temp_file));
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(PtyError::from(error)),
        }
    }
    Err(PtyError::Backend(format!(
        "failed to allocate a unique temporary supervisor socket for {}",
        socket_path.display()
    )))
}

fn bind_unix_socket_at(
    dirfd: libc::c_int,
    socket_fd: libc::c_int,
    name: &CString,
) -> io::Result<()> {
    let name_bytes = name.as_bytes_with_nul();
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    if name_bytes.len() > address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary Unix socket name exceeds sun_path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr().cast::<libc::c_char>(),
            address.sun_path.as_mut_ptr(),
            name_bytes.len(),
        );
    }
    let address_len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name_bytes.len()) as libc::socklen_t;

    // bind(2) 没有 *at 变体。fork 后的 cwd/umask 与父进程隔离，子进程只执行
    // async-signal-safe syscall，把相对路径解析锚定到已验证的 runtime dirfd。
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        let error = if unsafe { libc::fchdir(dirfd) } != 0 {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            unsafe { libc::umask(0o177) };
            let result = unsafe {
                libc::bind(
                    socket_fd,
                    (&raw const address).cast::<libc::sockaddr>(),
                    address_len,
                )
            };
            if result == 0 {
                0
            } else {
                io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO)
            }
        };
        unsafe { libc::_exit(error.clamp(0, u8::MAX as i32)) };
    }

    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(child, &mut status, 0) };
        if result == child {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if libc::WIFEXITED(status) {
        let error = libc::WEXITSTATUS(status);
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error))
        }
    } else {
        Err(io::Error::other(
            "temporary Unix socket bind helper terminated unexpectedly",
        ))
    }
}

fn random_supervisor_socket_temp_name() -> PtyResult<CString> {
    let mut random = [0_u8; SUPERVISOR_SOCKET_TEMP_RANDOM_BYTES];
    OsRng.fill_bytes(&mut random);
    c_string_from_bytes(format!(".t{}", general_purpose::URL_SAFE_NO_PAD.encode(random)).as_bytes())
}

fn link_supervisor_socket_noreplace(
    runtime_dir: &SecureRuntimeDir,
    from: &CString,
    to: &CString,
) -> io::Result<()> {
    let result = unsafe {
        libc::linkat(
            runtime_dir.file.as_raw_fd(),
            from.as_ptr(),
            runtime_dir.file.as_raw_fd(),
            to.as_ptr(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn supervisor_path_identity(file: &fs::File) -> PtyResult<SupervisorPathIdentity> {
    let metadata = file.metadata().map_err(PtyError::from)?;
    Ok(SupervisorPathIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn c_string_from_os_str(value: &OsStr, path: &Path) -> PtyResult<CString> {
    c_string_from_bytes(value.as_bytes()).map_err(|_| {
        PtyError::Backend(format!(
            "unsafe supervisor path contains an interior NUL byte: {}",
            path.display()
        ))
    })
}

fn c_string_from_bytes(value: &[u8]) -> PtyResult<CString> {
    CString::new(value).map_err(|_| {
        PtyError::Backend("unsafe supervisor path contains an interior NUL byte".to_owned())
    })
}

async fn handle_supervisor_connection(
    shared: SupervisorShared,
    expected_session_id: String,
    stream: UnixStream,
) -> PtyResult<()> {
    stream.peer_cred().map_err(PtyError::from)?;
    let (reader, writer) = stream.into_split();
    let (controller_tx, mut controller_rx, mut controller_overflow_rx) =
        bounded_supervisor_output_channel::<SupervisorFrame>();
    // 保留一个显式 sender，避免 controller 还未写入 state 或被短暂替换时，
    // outbound 分支把通道误判为关闭而结束 IPC 连接。
    let _controller_tx_keepalive = controller_tx.clone();
    let (request_tx, mut request_rx) =
        tokio_mpsc::channel::<io::Result<SupervisorRequestEnvelope>>(64);
    let writer_budget = Arc::new(SupervisorOutputQueueBudget::default());
    let (writer_control_tx, writer_control_rx, _writer_control_overflow_rx) =
        bounded_supervisor_output_channel_with_budget::<SupervisorFrame>(Arc::clone(
            &writer_budget,
        ));
    let (writer_data_tx, writer_data_rx, _writer_data_overflow_rx) =
        bounded_supervisor_output_channel_with_budget::<SupervisorFrame>(writer_budget);
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<io::Result<()>>();
    // 中文注释：读请求和写 live output 不能放在同一个 select 分支里。
    // `read_exact` 不是 cancel-safe；旧实现一边读请求一边抢写输出时，输出分支获胜会丢掉
    // 已读的一半请求字节。拆成独立 reader 后，请求帧一旦开始读取就一定会读完整。
    let reader_task = tokio::spawn(supervisor_connection_reader(reader, request_tx));
    // 中文注释：控制响应走 control 队列，live terminal frame 走 data 队列。
    // 这样大量输出只能排在响应之间，不能把 attach/input/ping 的响应整体压到输出末尾。
    let writer_task = tokio::spawn(async move {
        let result = supervisor_connection_writer(writer, writer_control_rx, writer_data_rx).await;
        let _ = writer_done_tx.send(result);
    });
    let mut controller_identity = None;
    let mut pending_resume_identity = None;
    let mut committed_resume_transaction_id = None;
    let mut pending_cleanup_authentication = None;
    let mut cleanup_authenticated = false;

    let result = 'connection: loop {
        tokio::select! {
            biased;

            writer_result = &mut writer_done_rx => {
                match writer_result {
                    Ok(Ok(())) => break Ok(()),
                    Ok(Err(error)) => break Err(PtyError::from(error)),
                    Err(_) => break Err(PtyError::Backend(
                        "session supervisor writer task stopped unexpectedly".to_owned(),
                    )),
                }
            }
            overflow = controller_overflow_rx.changed() => {
                if overflow.is_ok() && *controller_overflow_rx.borrow() {
                    break Err(PtyError::Backend(
                        "session supervisor controller output queue overflow; reconnect to resynchronize"
                            .to_owned(),
                    ));
                }
                break Ok(());
            }
            inbound = request_rx.recv() => {
                let Some(inbound) = inbound else {
                    break Ok(());
                };
                let envelope = match inbound {
                    Ok(envelope) => envelope,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                    Err(error) => break Err(PtyError::from(error)),
                };
                let mut suppress_live_through_base_seq = None;
                let request = envelope.request;
                let response = if cleanup_authenticated && !request.allowed_in_restricted_mode() {
                    SupervisorResponse::err(
                        "restricted cleanup connection cannot enter controller mode",
                    )
                } else {
                    let response = async {
                        Ok::<SupervisorResponse, PtyError>(match request {
                    SupervisorRequest::Attach { session_id } => {
                        if controller_identity.is_some() {
                            SupervisorResponse::err(
                                "session supervisor control socket is already attached",
                            )
                        } else if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let process_id = shared.session.lock().await.process_id();
                            let sync = {
                                let mut state = shared.state.lock().await;
                                let (identity, sync) =
                                    state.attach_sync(controller_tx.clone(), process_id, None);
                                controller_identity = Some(identity);
                                sync
                            };
                            suppress_live_through_base_seq = Some(sync.base_seq);
                            SupervisorResponse::ok(SupervisorResponsePayload::Snapshot(
                                sync.snapshot,
                            ))
                        }
                    }
                    SupervisorRequest::AttachSync {
                        session_id,
                        last_terminal_seq,
                        resume_controller_id,
                    } => {
                        if controller_identity.is_some() {
                            SupervisorResponse::err(
                                "session supervisor control socket is already attached",
                            )
                        } else if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let process_id = shared.session.lock().await.process_id();
                            let sync_result = {
                                let mut state = shared.state.lock().await;
                                if state.exited {
                                    Err(PtyError::Backend(
                                        "session supervisor PTY has exited".to_owned(),
                                    ))
                                } else {
                                    match resume_controller_id {
                                    Some(resume_controller_id) => state.resume_attach_sync(
                                        resume_controller_id,
                                        process_id,
                                        last_terminal_seq,
                                    ),
                                    None => Ok(state.attach_sync(
                                        controller_tx.clone(),
                                        process_id,
                                        last_terminal_seq,
                                    )),
                                    }
                                }
                            };
                            match sync_result {
                                Ok((identity, sync)) => {
                                    if resume_controller_id.is_some() {
                                        pending_resume_identity = Some(identity);
                                    } else {
                                        controller_identity = Some(identity);
                                    }
                                    suppress_live_through_base_seq = Some(sync.base_seq);
                                    SupervisorResponse::ok(SupervisorResponsePayload::AttachSync(sync))
                                }
                                Err(error) => SupervisorResponse::err(error.to_string()),
                            }
                        }
                    }
                    SupervisorRequest::CommitControllerResume { transaction_id } => {
                        match pending_resume_identity {
                            Some(candidate) => {
                                let commit_result = shared
                                    .state
                                    .lock()
                                    .await
                                    .commit_controller_resume(
                                        transaction_id,
                                        candidate,
                                        controller_tx.clone(),
                                    );
                                match commit_result {
                                    Ok(committed) => {
                                        controller_identity = Some(committed);
                                        pending_resume_identity = None;
                                        committed_resume_transaction_id = Some(transaction_id);
                                        SupervisorResponse::ok(
                                            SupervisorResponsePayload::ControllerResumeStatus {
                                                controller_id: Some(committed.id),
                                                controller_connection_id: Some(
                                                    committed.connection_id,
                                                ),
                                            },
                                        )
                                    }
                                    Err(error) => SupervisorResponse::err(error.to_string()),
                                }
                            }
                            None => SupervisorResponse::err(
                                "session supervisor has no pending controller resume",
                            ),
                        }
                    }
                    SupervisorRequest::ControllerResumeStatus { transaction_id } => {
                        let committed = shared
                            .state
                            .lock()
                            .await
                            .committed_controller_resume(transaction_id);
                        SupervisorResponse::ok(
                            SupervisorResponsePayload::ControllerResumeStatus {
                                controller_id: committed.map(|identity| identity.id),
                                controller_connection_id: committed
                                    .map(|identity| identity.connection_id),
                            },
                        )
                    }
                    SupervisorRequest::ResetAttachedDevices => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        shared.state.lock().await.attached_devices.clear();
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::ResetAttachedDevicesForController {
                        controller_id,
                        controller_connection_id,
                    } => {
                        let controller = ControllerIdentity {
                            id: controller_id,
                            connection_id: controller_connection_id,
                        };
                        let mut state = shared.state.lock().await;
                        if !state.is_active_controller(controller) {
                            SupervisorResponse::err(
                                "session supervisor reset controller is not active",
                            )
                        } else {
                            state.attached_devices.clear();
                            SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                        }
                    }
                    SupervisorRequest::AttachDevice { device_id } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        let mut state = shared.state.lock().await;
                        if state.exited {
                            SupervisorResponse::err("session supervisor PTY has exited")
                        } else {
                            state.attach_device(&device_id);
                            SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                        }
                    }
                    SupervisorRequest::DetachDevice { device_id } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        // 中文注释：detach 必须是幂等的。第一次请求如果已经在
                        // supervisor 侧生效，但响应在 IPC 断线时丢失，重试不能再把它
                        // 翻译成业务失败，否则 daemon 本地镜像无法继续同步收敛。
                        shared.state.lock().await.detach_device(&device_id);
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::DeviceAttached { device_id } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        let attached = shared.state.lock().await.has_attached_device(&device_id);
                        SupervisorResponse::ok(SupervisorResponsePayload::DeviceAttached {
                            attached,
                        })
                    }
                    SupervisorRequest::Input { data_base64 } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        if shared.state.lock().await.exited {
                            SupervisorResponse::err("session supervisor PTY has exited")
                        } else {
                            let bytes = general_purpose::STANDARD
                                .decode(data_base64)
                                .map_err(PtyError::backend)?;
                            shared.session.lock().await.write_all(&bytes)?;
                            SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                        }
                    }
                    SupervisorRequest::Resize { size } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        shared.session.lock().await.resize(size)?;
                        {
                            let mut state = shared.state.lock().await;
                            let frame = state.resize(size);
                            state.broadcast_terminal_frame(shared.session_id.as_str(), frame);
                        }
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::Snapshot => {
                        ensure_current_or_pending_controller(
                            &shared,
                            controller_identity,
                            pending_resume_identity,
                        )
                        .await?;
                        let process_id = shared.session.lock().await.process_id();
                        let state = shared.state.lock().await;
                        let payload = SupervisorSnapshotPayload {
                            size: state.size(),
                            process_id,
                            exited: state.exited,
                            retained_output: state.snapshot_output(),
                        };
                        SupervisorResponse::ok(SupervisorResponsePayload::Snapshot(payload))
                    }
                    SupervisorRequest::TerminalSnapshot { last_terminal_seq } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        let (base_seq, frames) = shared
                            .state
                            .lock()
                            .await
                            .terminal_snapshot_or_tail_with_base(last_terminal_seq);
                        suppress_live_through_base_seq = Some(base_seq);
                        SupervisorResponse::ok(SupervisorResponsePayload::TerminalFrames {
                            base_seq,
                            frames,
                        })
                    }
                    SupervisorRequest::Close => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        {
                            let mut session = shared.session.lock().await;
                            session.terminate()?;
                            let _ = session.wait();
                        }
                        let _ = shared.shutdown_tx.send(true);
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::CloseIdempotent { operation_id } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        let already_confirmed = shared
                            .state
                            .lock()
                            .await
                            .confirmed_close_operation_id
                            == Some(operation_id);
                        if !already_confirmed {
                            let mut session = shared.session.lock().await;
                            session.terminate()?;
                            session.wait()?;
                            shared.state.lock().await.confirmed_close_operation_id =
                                Some(operation_id);
                        }
                        SupervisorResponse::ok(SupervisorResponsePayload::CloseStatus {
                            confirmed_dead: true,
                        })
                    }
                    SupervisorRequest::InstallCleanupCapability {
                        session_id,
                        capability_base64,
                        migration_operation_id: _,
                    } => {
                        if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            match decode_cleanup_capability(&capability_base64) {
                                Ok(capability) => {
                                    let mut state = shared.state.lock().await;
                                    let controller_authorized = controller_identity
                                        .is_some_and(|identity| state.is_active_controller(identity));
                                    if controller_authorized {
                                        state.cleanup_capability = Some(capability);
                                        state.cleanup_capability_replaceable = false;
                                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                                    } else {
                                        SupervisorResponse::err(
                                            "cleanup capability installation is not authorized",
                                        )
                                    }
                                }
                                Err(error) => SupervisorResponse::err(error.to_string()),
                            }
                        }
                    }
                    SupervisorRequest::CleanupAuthChallenge {
                        session_id,
                        client_nonce_base64,
                    } => {
                        if controller_identity.is_some() {
                            SupervisorResponse::err(
                                "cleanup authentication cannot use a controller connection",
                            )
                        } else if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            match (
                                decode_cleanup_nonce(&client_nonce_base64),
                                shared.state.lock().await.cleanup_capability,
                            ) {
                                (Ok(client_nonce), Some(capability)) => {
                                    let mut server_nonce = [0_u8; CLEANUP_AUTH_NONCE_BYTES];
                                    OsRng.fill_bytes(&mut server_nonce);
                                    let proof = cleanup_auth_proof(
                                        &capability,
                                        &expected_session_id,
                                        &client_nonce,
                                        &server_nonce,
                                    );
                                    pending_cleanup_authentication =
                                        Some(PendingCleanupAuthentication {
                                            client_nonce,
                                            server_nonce,
                                        });
                                    SupervisorResponse::ok(
                                        SupervisorResponsePayload::CleanupAuthChallenge {
                                            server_nonce_base64: general_purpose::STANDARD
                                                .encode(server_nonce),
                                            server_proof_base64: general_purpose::STANDARD
                                                .encode(proof),
                                        },
                                    )
                                }
                                (Ok(_), None) => {
                                    SupervisorResponse::err("cleanup capability is not installed")
                                }
                                (Err(error), _) => SupervisorResponse::err(error.to_string()),
                            }
                        }
                    }
                    SupervisorRequest::CleanupAuthenticate {
                        session_id,
                        client_nonce_base64,
                        server_nonce_base64,
                        capability_base64,
                    } => {
                        let pending = pending_cleanup_authentication.take();
                        if controller_identity.is_some() {
                            SupervisorResponse::err(
                                "cleanup authentication cannot use a controller connection",
                            )
                        } else if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let supplied = decode_cleanup_capability(&capability_base64);
                            let client_nonce = decode_cleanup_nonce(&client_nonce_base64);
                            let server_nonce = decode_cleanup_nonce(&server_nonce_base64);
                            let expected_capability = shared.state.lock().await.cleanup_capability;
                            let authenticated = match (
                                pending,
                                supplied,
                                client_nonce,
                                server_nonce,
                                expected_capability,
                            ) {
                                (
                                    Some(pending),
                                    Ok(supplied),
                                    Ok(client_nonce),
                                    Ok(server_nonce),
                                    Some(expected_capability),
                                ) => {
                                    constant_time_eq(&supplied, &expected_capability)
                                        && constant_time_eq(&client_nonce, &pending.client_nonce)
                                        && constant_time_eq(&server_nonce, &pending.server_nonce)
                                }
                                _ => false,
                            };
                            if authenticated {
                                shared.state.lock().await.cleanup_capability_replaceable = false;
                                cleanup_authenticated = true;
                                SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                            } else {
                                SupervisorResponse::err("cleanup authentication failed")
                            }
                        }
                    }
                    SupervisorRequest::CleanupClose {
                        session_id,
                        operation_id,
                    } => {
                        if !cleanup_authenticated {
                            SupervisorResponse::err(
                                "restricted cleanup close requires authentication",
                            )
                        } else if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let confirmed_operation_id = shared
                                .state
                                .lock()
                                .await
                                .confirmed_close_operation_id;
                            match confirmed_operation_id {
                                Some(confirmed) if confirmed != operation_id => {
                                    SupervisorResponse::err(
                                        "a different session supervisor close operation is confirmed",
                                    )
                                }
                                Some(_) => SupervisorResponse::ok(
                                    SupervisorResponsePayload::CloseStatus {
                                        confirmed_dead: true,
                                    },
                                ),
                                None => {
                                    shared.state.lock().await.cleanup_capability_replaceable = false;
                                    let mut session = shared.session.lock().await;
                                    session.terminate()?;
                                    session.wait()?;
                                    shared.state.lock().await.confirmed_close_operation_id =
                                        Some(operation_id);
                                    SupervisorResponse::ok(
                                        SupervisorResponsePayload::CloseStatus {
                                            confirmed_dead: true,
                                        },
                                    )
                                }
                            }
                        }
                    }
                    SupervisorRequest::CloseStatus { operation_id } => {
                        if controller_identity.is_none() && !cleanup_authenticated {
                            SupervisorResponse::err(
                                "session supervisor close status requires authority",
                            )
                        } else {
                            let confirmed_dead = shared
                                .state
                                .lock()
                                .await
                                .confirmed_close_operation_id
                                == Some(operation_id);
                            SupervisorResponse::ok(SupervisorResponsePayload::CloseStatus {
                                confirmed_dead,
                            })
                        }
                    }
                    SupervisorRequest::FinalizeClose { operation_id } => {
                        if controller_identity.is_none() && !cleanup_authenticated {
                            SupervisorResponse::err(
                                "session supervisor close finalization requires authority",
                            )
                        } else if shared
                            .state
                            .lock()
                            .await
                            .confirmed_close_operation_id
                            != Some(operation_id)
                        {
                            SupervisorResponse::err(
                                "session supervisor close operation is not confirmed",
                            )
                        } else {
                            let _ = shared.shutdown_tx.send(true);
                            SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                        }
                    }
                    SupervisorRequest::NaturalExitStatus => {
                        let exited = shared.state.lock().await.exited;
                        SupervisorResponse::ok(SupervisorResponsePayload::CloseStatus {
                            confirmed_dead: exited,
                        })
                    }
                    SupervisorRequest::FinalizeNaturalExit => {
                        if !shared.state.lock().await.exited {
                            SupervisorResponse::err("session supervisor PTY has not exited")
                        } else {
                            let _ = shared.shutdown_tx.send(true);
                            SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                        }
                    }
                    SupervisorRequest::Ping => {
                        if !cleanup_authenticated {
                            ensure_current_or_pending_controller(
                                &shared,
                                controller_identity,
                                pending_resume_identity,
                            )
                            .await?;
                        }
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                        })
                    }
                    .await;
                    match response {
                        Ok(response) => response,
                        Err(error) => SupervisorResponse::err(error.to_string()),
                    }
                };

                let frame = SupervisorFrame::Response {
                    request_id: envelope.request_id,
                    response,
                };
                let delayed_live_frames = suppress_live_through_base_seq
                    .map(|base_seq| drain_controller_frames_after_sync(&mut controller_rx, base_seq))
                    .unwrap_or_default();
                let retained_bytes = match supervisor_serialized_frame_bytes(&frame) {
                    Ok(retained_bytes) => retained_bytes,
                    Err(error) => break Err(PtyError::from(error)),
                };
                if writer_control_tx.try_send(frame, retained_bytes).is_err() {
                    break Err(PtyError::Backend(
                        "session supervisor writer output queue overflow".to_owned(),
                    ));
                }
                for frame in delayed_live_frames {
                    if !is_current_controller(&shared, controller_identity).await {
                        break;
                    }
                    let retained_bytes = match supervisor_serialized_frame_bytes(&frame) {
                        Ok(retained_bytes) => retained_bytes,
                        Err(error) => break 'connection Err(PtyError::from(error)),
                    };
                    if writer_data_tx.try_send(frame, retained_bytes).is_err() {
                        break 'connection Err(PtyError::Backend(
                            "session supervisor writer output queue overflow".to_owned(),
                        ));
                    }
                }
            }
            outbound = controller_rx.recv() => {
                let Some(frame) = outbound else {
                    break Ok(());
                };
                if !is_current_controller(&shared, controller_identity).await {
                    break Ok(());
                }
                let retained_bytes = match supervisor_serialized_frame_bytes(&frame) {
                    Ok(retained_bytes) => retained_bytes,
                    Err(error) => break Err(PtyError::from(error)),
                };
                if writer_data_tx.try_send(frame, retained_bytes).is_err() {
                    break Err(PtyError::Backend(
                        "session supervisor writer output queue overflow".to_owned(),
                    ));
                }
            }
        }
    };

    reader_task.abort();
    writer_task.abort();
    if let Some(identity) = controller_identity {
        let mut state = shared.state.lock().await;
        let committed_here = committed_resume_transaction_id.is_some_and(|transaction_id| {
            state.committed_controller_resume(transaction_id) == Some(identity)
        });
        if !committed_here {
            state.remove_controller(identity);
        }
    }

    result
}

async fn handle_supervisor_terminal_attach_connection(
    shared: SupervisorShared,
    expected_session_id: String,
    stream: UnixStream,
) -> PtyResult<()> {
    let (mut reader, mut writer) = stream.into_split();
    let bootstrap = read_frame_async::<SupervisorTerminalClientFrame>(&mut reader).await?;
    let last_terminal_seq = match bootstrap {
        SupervisorTerminalClientFrame::BootstrapAttach {
            session_id,
            last_terminal_seq,
        } if session_id == expected_session_id => last_terminal_seq,
        SupervisorTerminalClientFrame::BootstrapAttach { .. } => {
            let _ = write_frame_async(
                &mut writer,
                &SupervisorTerminalServerFrame::Close {
                    reason: "protocol_error".to_owned(),
                    message: Some("session id mismatch".to_owned()),
                },
            )
            .await;
            return Err(PtyError::Backend("session id mismatch".to_owned()));
        }
        SupervisorTerminalClientFrame::Input { .. }
        | SupervisorTerminalClientFrame::Resize { .. }
        | SupervisorTerminalClientFrame::HeartbeatPong { .. } => {
            let _ = write_frame_async(
                &mut writer,
                &SupervisorTerminalServerFrame::Close {
                    reason: "protocol_error".to_owned(),
                    message: Some("missing bootstrap attach".to_owned()),
                },
            )
            .await;
            return Err(PtyError::Backend(
                "session supervisor terminal attach is missing bootstrap".to_owned(),
            ));
        }
    };

    let (request_tx, mut request_rx) =
        tokio_mpsc::channel::<io::Result<SupervisorTerminalClientFrame>>(64);
    let reader_task = tokio::spawn(supervisor_terminal_connection_reader(reader, request_tx));
    let (terminal_tx, mut terminal_rx, mut terminal_overflow_rx) =
        bounded_supervisor_output_channel::<SupervisorTerminalServerFrame>();
    let process_id = shared.session.lock().await.process_id();
    let (attach_id, attach_sync, base_seq) = {
        let mut state = shared.state.lock().await;
        let attach_id = state.register_terminal_attach(terminal_tx.clone());
        let snapshot = SupervisorSnapshotPayload {
            size: state.size(),
            process_id,
            exited: state.exited,
            // 中文注释：独立 terminal socket 也必须保持“首屏只走 frames”。
            // 否则 direct/relay 两条 attach 路径会出现不同的重放行为。
            retained_output: Vec::new(),
        };
        let (base_seq, frames) = state.terminal_snapshot_or_tail_with_base(last_terminal_seq);
        (
            attach_id,
            SupervisorTerminalServerFrame::AttachSync {
                session_id: expected_session_id.clone(),
                base_seq,
                snapshot,
                frames,
            },
            base_seq,
        )
    };

    let result = async {
        write_frame_async(&mut writer, &attach_sync).await?;
        for frame in drain_terminal_attach_frames_after_sync(&mut terminal_rx, base_seq) {
            write_frame_async(&mut writer, &frame).await?;
        }

        let mut heartbeat_interval = tokio::time::interval(SUPERVISOR_TERMINAL_HEARTBEAT_INTERVAL);
        let mut pending_heartbeat_nonce: Option<String> = None;
        let mut pending_heartbeat_sent_at: Option<Instant> = None;

        loop {
            tokio::select! {
                biased;

                overflow = terminal_overflow_rx.changed() => {
                    if overflow.is_ok() && *terminal_overflow_rx.borrow() {
                        let _ = write_frame_async(
                            &mut writer,
                            &SupervisorTerminalServerFrame::Close {
                                reason: "slow_consumer".to_owned(),
                                message: Some(
                                    "terminal output queue overflow; reattach to resynchronize"
                                        .to_owned(),
                                ),
                            },
                        )
                        .await;
                    }
                    return Ok(());
                }

                inbound = request_rx.recv() => {
                    let Some(inbound) = inbound else {
                        return Ok(());
                    };
                    let frame = match inbound {
                        Ok(frame) => frame,
                        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                        Err(error) => return Err(PtyError::from(error)),
                    };
                    match frame {
                        SupervisorTerminalClientFrame::BootstrapAttach { .. } => {
                            return Err(PtyError::Backend(
                                "terminal attach bootstrap must only be sent once".to_owned(),
                            ));
                        }
                        SupervisorTerminalClientFrame::Input { data } => {
                            shared.session.lock().await.write_all(&data)?;
                        }
                        SupervisorTerminalClientFrame::Resize { size } => {
                            shared.session.lock().await.resize(size)?;
                            let mut state = shared.state.lock().await;
                            let frame = state.resize(size);
                            state.broadcast_terminal_frame(shared.session_id.as_str(), frame);
                        }
                        SupervisorTerminalClientFrame::HeartbeatPong { nonce } => {
                            if pending_heartbeat_nonce.as_deref() == Some(nonce.as_str()) {
                                pending_heartbeat_nonce = None;
                                pending_heartbeat_sent_at = None;
                            }
                        }
                    }
                }
                outbound = terminal_rx.recv() => {
                    let Some(outbound) = outbound else {
                        return Ok(());
                    };
                    write_frame_async(&mut writer, &outbound).await?;
                }
                _ = heartbeat_interval.tick() => {
                    if let (Some(nonce), Some(sent_at)) = (&pending_heartbeat_nonce, pending_heartbeat_sent_at)
                        && sent_at.elapsed() >= SUPERVISOR_TERMINAL_HEARTBEAT_TIMEOUT
                    {
                        let _ = write_frame_async(
                            &mut writer,
                            &SupervisorTerminalServerFrame::Close {
                                reason: "heartbeat_timeout".to_owned(),
                                message: Some(format!("missed heartbeat pong for nonce {nonce}")),
                            },
                        ).await;
                        return Ok(());
                    }
                    if pending_heartbeat_nonce.is_none() {
                        let nonce = format!(
                            "{}-{}",
                            current_unix_timestamp_millis(),
                            process_id.unwrap_or_default(),
                        );
                        pending_heartbeat_sent_at = Some(Instant::now());
                        pending_heartbeat_nonce = Some(nonce.clone());
                        write_frame_async(
                            &mut writer,
                            &SupervisorTerminalServerFrame::HeartbeatPing {
                                nonce,
                                timeout_ms: SUPERVISOR_TERMINAL_HEARTBEAT_TIMEOUT.as_millis() as u64,
                            },
                        ).await?;
                    }
                }
            }
        }
    }
    .await;

    reader_task.abort();
    shared.state.lock().await.remove_terminal_attach(attach_id);
    result
}

async fn supervisor_connection_reader(
    mut reader: tokio::net::unix::OwnedReadHalf,
    request_tx: tokio_mpsc::Sender<io::Result<SupervisorRequestEnvelope>>,
) {
    loop {
        match read_frame_async::<SupervisorRequestEnvelope>(&mut reader).await {
            Ok(envelope) => {
                if request_tx.send(Ok(envelope)).await.is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                let _ = request_tx.send(Err(error)).await;
                break;
            }
        }
    }
}

async fn supervisor_terminal_connection_reader(
    mut reader: tokio::net::unix::OwnedReadHalf,
    request_tx: tokio_mpsc::Sender<io::Result<SupervisorTerminalClientFrame>>,
) {
    loop {
        match read_frame_async::<SupervisorTerminalClientFrame>(&mut reader).await {
            Ok(frame) => {
                if request_tx.send(Ok(frame)).await.is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                let _ = request_tx.send(Err(error)).await;
                break;
            }
        }
    }
}

async fn supervisor_connection_writer(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut control_rx: BoundedSupervisorOutputReceiver<SupervisorFrame>,
    mut data_rx: BoundedSupervisorOutputReceiver<SupervisorFrame>,
) -> io::Result<()> {
    let mut control_open = true;
    let mut data_open = true;

    while control_open || data_open {
        tokio::select! {
            biased;

            frame = control_rx.recv_reserved(), if control_open => {
                let Some(frame) = frame else {
                    control_open = false;
                    continue;
                };
                write_frame_async(&mut writer, frame.value()).await?;
            }
            frame = data_rx.recv_reserved(), if data_open => {
                let Some(frame) = frame else {
                    data_open = false;
                    continue;
                };
                write_frame_async(&mut writer, frame.value()).await?;
            }
        }
    }

    Ok(())
}

async fn ensure_current_controller(
    shared: &SupervisorShared,
    controller: Option<ControllerIdentity>,
) -> PtyResult<()> {
    let Some(controller) = controller else {
        return Err(PtyError::Backend(
            "session supervisor connection is not attached".to_owned(),
        ));
    };
    let state = shared.state.lock().await;
    if !state.is_active_controller(controller) {
        return Err(PtyError::Backend(
            "session supervisor connection is not the active controller".to_owned(),
        ));
    }
    Ok(())
}

async fn ensure_current_or_pending_controller(
    shared: &SupervisorShared,
    controller: Option<ControllerIdentity>,
    pending: Option<ControllerIdentity>,
) -> PtyResult<()> {
    if controller.is_some() {
        return ensure_current_controller(shared, controller).await;
    }
    let Some(pending) = pending else {
        return Err(PtyError::Backend(
            "session supervisor connection is not attached".to_owned(),
        ));
    };
    let state = shared.state.lock().await;
    if state.controller_resume_lease_id != Some(pending.id)
        || state
            .active_controller_id
            .is_some_and(|active_id| active_id != pending.id)
    {
        return Err(PtyError::Backend(
            "session supervisor connection is not the active controller".to_owned(),
        ));
    }
    Ok(())
}

async fn is_current_controller(
    shared: &SupervisorShared,
    controller: Option<ControllerIdentity>,
) -> bool {
    let Some(controller) = controller else {
        return false;
    };
    let state = shared.state.lock().await;
    state.is_active_controller(controller)
}

fn drain_controller_frames_after_sync(
    controller_rx: &mut BoundedSupervisorOutputReceiver<SupervisorFrame>,
    base_seq: u64,
) -> Vec<SupervisorFrame> {
    let mut delayed = Vec::new();
    while let Ok(frame) = controller_rx.try_recv() {
        match &frame {
            SupervisorFrame::TerminalFrame {
                frame: terminal_frame,
            } if terminal_frame
                .terminal_seq()
                .is_some_and(|seq| seq <= base_seq) =>
            {
                // 中文注释：AttachSync/TerminalSnapshot 的响应已经覆盖到 base_seq；
                // 仍停在 outbound 队列里的旧 live frame 必须丢弃，否则会在响应之后重放旧序号。
            }
            _ => delayed.push(frame),
        }
    }
    delayed
}

fn drain_terminal_attach_frames_after_sync(
    controller_rx: &mut BoundedSupervisorOutputReceiver<SupervisorTerminalServerFrame>,
    base_seq: u64,
) -> Vec<SupervisorTerminalServerFrame> {
    let mut delayed = Vec::new();
    while let Ok(frame) = controller_rx.try_recv() {
        match &frame {
            SupervisorTerminalServerFrame::TerminalFrame {
                frame: terminal_frame,
                ..
            } if terminal_frame
                .terminal_seq()
                .is_some_and(|seq| seq <= base_seq) =>
            {
                // 中文注释：attach_sync 已经覆盖到 base_seq，队列里更早的 live frame 必须丢弃。
            }
            _ => delayed.push(frame),
        }
    }
    delayed
}

async fn supervisor_output_pump(shared: SupervisorShared, mut output_signal: watch::Receiver<u64>) {
    drain_supervisor_output_until_idle(&shared).await;

    while output_signal.changed().await.is_ok() {
        drain_supervisor_output_until_idle(&shared).await;
    }
}

async fn drain_supervisor_output_until_idle(shared: &SupervisorShared) {
    while drain_supervisor_output(shared).await {
        // 中文注释：持续刷屏时不要让 output pump 连续占用 runtime；让 IPC reader、
        // input、resize、attach snapshot 等控制请求有机会先拿到 session/state 锁。
        tokio::task::yield_now().await;
    }
}

async fn drain_supervisor_output(shared: &SupervisorShared) -> bool {
    let mut chunks = 0_usize;
    let mut bytes = 0_usize;
    let mut batch = Vec::with_capacity(SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES);
    loop {
        if chunks >= SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK
            || bytes >= SUPERVISOR_OUTPUT_PUMP_MAX_BYTES_PER_TICK
        {
            publish_supervisor_output_batch(shared, &mut batch).await;
            return true;
        }

        let mut buffer = vec![0_u8; SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES];
        let read = match shared.session.lock().await.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                publish_supervisor_output_batch(shared, &mut batch).await;
                tracing::warn!(%error, "session supervisor failed to read PTY output");
                return false;
            }
        };
        if read == 0 {
            publish_supervisor_output_batch(shared, &mut batch).await;
            return false;
        }

        buffer.truncate(read);
        chunks = chunks.saturating_add(1);
        bytes = bytes.saturating_add(read);
        if batch.len().saturating_add(buffer.len()) > SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES {
            publish_supervisor_output_batch(shared, &mut batch).await;
        }
        batch.extend_from_slice(&buffer);
        if batch.len() >= SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES {
            publish_supervisor_output_batch(shared, &mut batch).await;
        }
    }
}

async fn publish_supervisor_output_batch(shared: &SupervisorShared, batch: &mut Vec<u8>) {
    if batch.is_empty() {
        return;
    }
    let mut state = shared.state.lock().await;
    let frame = state.record_output(batch);
    state.broadcast_terminal_frame(shared.session_id.as_str(), frame);
    batch.clear();
}

async fn supervisor_exit_watcher(shared: SupervisorShared) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let exit_status = match shared.session.lock().await.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, "session supervisor failed to poll PTY exit status");
                return;
            }
        };
        let code = if exit_status.signal.is_none() {
            Some(exit_status.exit_code as i32)
        } else {
            None
        };
        let mut state = shared.state.lock().await;
        let frame = state.record_exit(code);
        state.broadcast_terminal_frame(shared.session_id.as_str(), frame);
        return;
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SupervisorRequestEnvelope {
    request_id: u64,
    request: SupervisorRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorRequest {
    Attach {
        session_id: String,
    },
    AttachSync {
        session_id: String,
        last_terminal_seq: Option<u64>,
        resume_controller_id: Option<u64>,
    },
    CommitControllerResume {
        transaction_id: u64,
    },
    ControllerResumeStatus {
        transaction_id: u64,
    },
    ResetAttachedDevices,
    ResetAttachedDevicesForController {
        controller_id: u64,
        controller_connection_id: u64,
    },
    AttachDevice {
        device_id: String,
    },
    DetachDevice {
        device_id: String,
    },
    DeviceAttached {
        device_id: String,
    },
    Input {
        data_base64: String,
    },
    Resize {
        size: PtySize,
    },
    Snapshot,
    TerminalSnapshot {
        last_terminal_seq: Option<u64>,
    },
    Close,
    CloseIdempotent {
        operation_id: u64,
    },
    InstallCleanupCapability {
        session_id: String,
        capability_base64: String,
        #[serde(default)]
        migration_operation_id: Option<u64>,
    },
    CleanupAuthChallenge {
        session_id: String,
        client_nonce_base64: String,
    },
    CleanupAuthenticate {
        session_id: String,
        client_nonce_base64: String,
        server_nonce_base64: String,
        capability_base64: String,
    },
    CleanupClose {
        session_id: String,
        operation_id: u64,
    },
    CloseStatus {
        operation_id: u64,
    },
    FinalizeClose {
        operation_id: u64,
    },
    NaturalExitStatus,
    FinalizeNaturalExit,
    Ping,
}

impl SupervisorRequest {
    fn allowed_in_restricted_mode(&self) -> bool {
        matches!(
            self,
            Self::CleanupAuthChallenge { .. }
                | Self::CleanupAuthenticate { .. }
                | Self::CleanupClose { .. }
                | Self::CloseStatus { .. }
                | Self::FinalizeClose { .. }
                | Self::Ping
        )
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::AttachSync { .. } => "attach_sync",
            Self::CommitControllerResume { .. } => "commit_controller_resume",
            Self::ControllerResumeStatus { .. } => "controller_resume_status",
            Self::ResetAttachedDevices => "reset_attached_devices",
            Self::ResetAttachedDevicesForController { .. } => {
                "reset_attached_devices_for_controller"
            }
            Self::AttachDevice { .. } => "attach_device",
            Self::DetachDevice { .. } => "detach_device",
            Self::DeviceAttached { .. } => "device_attached",
            Self::Input { .. } => "input",
            Self::Resize { .. } => "resize",
            Self::Snapshot => "snapshot",
            Self::TerminalSnapshot { .. } => "terminal_snapshot",
            Self::Close => "close",
            Self::CloseIdempotent { .. } => "close_idempotent",
            Self::InstallCleanupCapability { .. } => "install_cleanup_capability",
            Self::CleanupAuthChallenge { .. } => "cleanup_auth_challenge",
            Self::CleanupAuthenticate { .. } => "cleanup_authenticate",
            Self::CleanupClose { .. } => "cleanup_close",
            Self::CloseStatus { .. } => "close_status",
            Self::FinalizeClose { .. } => "finalize_close",
            Self::NaturalExitStatus => "natural_exit_status",
            Self::FinalizeNaturalExit => "finalize_natural_exit",
            Self::Ping => "ping",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorFrame {
    Response {
        request_id: u64,
        response: SupervisorResponse,
    },
    TerminalFrame {
        frame: PtyTerminalFrame,
    },
}

enum SupervisorRequestCompletion {
    Response(SupervisorResponse),
    TransportError(PtyError),
}

impl SupervisorRequestCompletion {
    fn into_result(self) -> Result<SupervisorResponsePayload, SupervisorRequestFailure> {
        match self {
            Self::Response(response) => response
                .into_result()
                .map_err(SupervisorRequestFailure::Response),
            Self::TransportError(error) => Err(SupervisorRequestFailure::Transport(error)),
        }
    }
}

enum SupervisorRequestFailure {
    Response(PtyError),
    Transport(PtyError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SupervisorResponse {
    Ok { payload: SupervisorResponsePayload },
    Err { message: String },
}

impl SupervisorResponse {
    fn ok(payload: SupervisorResponsePayload) -> Self {
        Self::Ok { payload }
    }

    fn err(message: impl Into<String>) -> Self {
        Self::Err {
            message: message.into(),
        }
    }

    fn into_result(self) -> PtyResult<SupervisorResponsePayload> {
        match self {
            Self::Ok { payload } => Ok(payload),
            Self::Err { message } => Err(PtyError::Backend(message)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SupervisorResponsePayload {
    Empty,
    Snapshot(SupervisorSnapshotPayload),
    AttachSync(SupervisorAttachSyncPayload),
    DeviceAttached {
        attached: bool,
    },
    TerminalFrames {
        base_seq: u64,
        frames: Vec<PtyTerminalFrame>,
    },
    ControllerResumeStatus {
        controller_id: Option<u64>,
        controller_connection_id: Option<u64>,
    },
    CleanupAuthChallenge {
        server_nonce_base64: String,
        server_proof_base64: String,
    },
    CloseStatus {
        confirmed_dead: bool,
    },
}

impl SupervisorResponsePayload {
    fn expect_empty(self) -> PtyResult<()> {
        match self {
            Self::Empty => Ok(()),
            Self::Snapshot(_)
            | Self::AttachSync(_)
            | Self::DeviceAttached { .. }
            | Self::TerminalFrames { .. }
            | Self::ControllerResumeStatus { .. }
            | Self::CleanupAuthChallenge { .. }
            | Self::CloseStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned unexpected snapshot payload".to_owned(),
            )),
        }
    }

    fn into_snapshot(self) -> PtyResult<SupervisorSnapshotPayload> {
        match self {
            Self::Snapshot(payload) => Ok(payload),
            Self::Empty
            | Self::AttachSync(_)
            | Self::DeviceAttached { .. }
            | Self::TerminalFrames { .. }
            | Self::ControllerResumeStatus { .. }
            | Self::CleanupAuthChallenge { .. }
            | Self::CloseStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned empty payload".to_owned(),
            )),
        }
    }

    fn into_attach_sync(self) -> PtyResult<SupervisorAttachSyncPayload> {
        match self {
            Self::AttachSync(payload) => Ok(payload),
            Self::Empty
            | Self::Snapshot(_)
            | Self::DeviceAttached { .. }
            | Self::TerminalFrames { .. }
            | Self::ControllerResumeStatus { .. }
            | Self::CleanupAuthChallenge { .. }
            | Self::CloseStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned unexpected attach sync payload".to_owned(),
            )),
        }
    }

    fn into_device_attached(self) -> PtyResult<bool> {
        match self {
            Self::DeviceAttached { attached } => Ok(attached),
            Self::Empty
            | Self::Snapshot(_)
            | Self::AttachSync(_)
            | Self::TerminalFrames { .. }
            | Self::ControllerResumeStatus { .. }
            | Self::CleanupAuthChallenge { .. }
            | Self::CloseStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned unexpected device attachment payload".to_owned(),
            )),
        }
    }

    #[allow(dead_code)]
    fn into_terminal_frames(self) -> PtyResult<(u64, Vec<PtyTerminalFrame>)> {
        match self {
            Self::TerminalFrames { base_seq, frames } => Ok((base_seq, frames)),
            Self::Empty
            | Self::Snapshot(_)
            | Self::AttachSync(_)
            | Self::DeviceAttached { .. }
            | Self::ControllerResumeStatus { .. }
            | Self::CleanupAuthChallenge { .. }
            | Self::CloseStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned unexpected terminal frames payload".to_owned(),
            )),
        }
    }

    fn into_controller_resume_status(self) -> PtyResult<Option<ControllerIdentity>> {
        match self {
            Self::ControllerResumeStatus {
                controller_id: Some(id),
                controller_connection_id: Some(connection_id),
            } => Ok(Some(ControllerIdentity { id, connection_id })),
            Self::ControllerResumeStatus {
                controller_id: None,
                controller_connection_id: None,
            } => Ok(None),
            Self::ControllerResumeStatus { .. } => Err(PtyError::Backend(
                "session supervisor returned incomplete resume status".to_owned(),
            )),
            _ => Err(PtyError::Backend(
                "session supervisor returned unexpected resume status payload".to_owned(),
            )),
        }
    }

    fn into_cleanup_auth_challenge(self) -> PtyResult<(String, String)> {
        match self {
            Self::CleanupAuthChallenge {
                server_nonce_base64,
                server_proof_base64,
            } => Ok((server_nonce_base64, server_proof_base64)),
            _ => Err(PtyError::Backend(
                "session supervisor returned unexpected cleanup authentication payload".to_owned(),
            )),
        }
    }

    fn into_close_status(self) -> PtyResult<bool> {
        match self {
            Self::CloseStatus { confirmed_dead } => Ok(confirmed_dead),
            _ => Err(PtyError::Backend(
                "session supervisor returned unexpected close status payload".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SupervisorSnapshotPayload {
    pub(crate) size: PtySize,
    pub(crate) process_id: Option<u32>,
    #[serde(default)]
    pub(crate) exited: bool,
    #[serde(with = "base64_bytes")]
    pub(crate) retained_output: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisorAttachSyncPayload {
    controller_id: u64,
    controller_connection_id: u64,
    snapshot: SupervisorSnapshotPayload,
    base_seq: u64,
    frames: Vec<PtyTerminalFrame>,
}

fn read_frame_sync<T>(reader: &mut StdUnixStream) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = checked_supervisor_payload_len(length)?;
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(invalid_data)
}

fn write_frame_sync<T>(writer: &mut StdUnixStream, value: &T) -> io::Result<()>
where
    T: Serialize,
{
    let payload_len = supervisor_json_payload_len(value)?;
    let length = supervisor_payload_length_prefix(payload_len)?;
    writer.write_all(&length)?;
    let written = write_supervisor_json_payload(&mut *writer, value)?;
    if written != payload_len {
        return Err(invalid_data(
            "session supervisor serialized frame length changed during write",
        ));
    }
    writer.flush()?;
    Ok(())
}

async fn read_frame_async<T>(reader: &mut tokio::net::unix::OwnedReadHalf) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = checked_supervisor_payload_len(length)?;
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(invalid_data)
}

async fn write_frame_async<T>(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> io::Result<()>
where
    T: Serialize,
{
    let payload_len = supervisor_json_payload_len(value)?;
    let length = supervisor_payload_length_prefix(payload_len)?;
    let mut payload = Vec::with_capacity(payload_len);
    let written = write_supervisor_json_payload(&mut payload, value)?;
    if written != payload_len {
        return Err(invalid_data(
            "session supervisor serialized frame length changed during write",
        ));
    }
    writer.write_all(&length).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

fn read_raw_frame_sync(reader: &mut StdUnixStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let payload_len = checked_supervisor_payload_len(length)?;
    let mut frame = vec![0_u8; 4 + payload_len];
    frame[..4].copy_from_slice(&length);
    reader.read_exact(&mut frame[4..])?;
    Ok(frame)
}

fn checked_supervisor_payload_len(length: [u8; 4]) -> io::Result<usize> {
    let payload_len = u32::from_le_bytes(length) as usize;
    if payload_len > MAX_SUPERVISOR_FRAME_BYTES - 4 {
        return Err(supervisor_frame_too_large());
    }
    Ok(payload_len)
}

fn supervisor_payload_length_prefix(payload_len: usize) -> io::Result<[u8; 4]> {
    if payload_len > MAX_SUPERVISOR_FRAME_BYTES - 4 {
        return Err(supervisor_frame_too_large());
    }
    let payload_len = u32::try_from(payload_len)
        .map_err(|_| invalid_data("session supervisor frame length cannot be represented"))?;
    Ok(payload_len.to_le_bytes())
}

struct BoundedSupervisorPayloadWriter<W> {
    inner: W,
    written: usize,
}

impl<W> BoundedSupervisorPayloadWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W> Write for BoundedSupervisorPayloadWriter<W>
where
    W: Write,
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_SUPERVISOR_FRAME_BYTES - 4).saturating_sub(self.written) {
            return Err(supervisor_frame_too_large());
        }
        let written = self.inner.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn supervisor_json_payload_len<T>(value: &T) -> io::Result<usize>
where
    T: Serialize,
{
    write_supervisor_json_payload(io::sink(), value)
}

fn supervisor_serialized_frame_bytes<T>(value: &T) -> io::Result<usize>
where
    T: Serialize,
{
    supervisor_json_payload_len(value)?
        .checked_add(4)
        .ok_or_else(supervisor_frame_too_large)
}

fn write_supervisor_json_payload<T, W>(writer: W, value: &T) -> io::Result<usize>
where
    T: Serialize,
    W: Write,
{
    let mut writer = BoundedSupervisorPayloadWriter::new(writer);
    serde_json::to_writer(&mut writer, value).map_err(serde_json_io_error)?;
    Ok(writer.written)
}

fn serde_json_io_error(error: serde_json::Error) -> io::Error {
    match error.io_error_kind() {
        Some(kind) => io::Error::new(kind, error),
        None => invalid_data(error),
    }
}

fn supervisor_frame_too_large() -> io::Error {
    invalid_data(format!(
        "session supervisor frame exceeds maximum of {MAX_SUPERVISOR_FRAME_BYTES} bytes"
    ))
}

pub(crate) fn validate_length_prefixed_supervisor_frame(bytes: &[u8]) -> io::Result<usize> {
    let length: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| invalid_data("session supervisor frame is missing length prefix"))?
        .try_into()
        .expect("four-byte prefix slice should convert");
    let payload_len = checked_supervisor_payload_len(length)?;
    if bytes.len() != 4 + payload_len {
        return Err(invalid_data(
            "session supervisor frame length does not match payload",
        ));
    }
    Ok(payload_len)
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn current_unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SupervisorTerminalClientFrame {
    BootstrapAttach {
        session_id: String,
        last_terminal_seq: Option<u64>,
    },
    Input {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Resize {
        size: PtySize,
    },
    HeartbeatPong {
        nonce: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SupervisorTerminalServerFrame {
    AttachSync {
        session_id: String,
        base_seq: u64,
        snapshot: SupervisorSnapshotPayload,
        frames: Vec<PtyTerminalFrame>,
    },
    TerminalFrame {
        session_id: String,
        frame: PtyTerminalFrame,
    },
    HeartbeatPing {
        nonce: String,
        timeout_ms: u64,
    },
    Close {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

pub(crate) fn decode_supervisor_terminal_client_frame(
    bytes: &[u8],
) -> PtyResult<SupervisorTerminalClientFrame> {
    decode_length_prefixed_json(bytes).map_err(PtyError::from)
}

fn decode_length_prefixed_json<T>(bytes: &[u8]) -> io::Result<T>
where
    T: DeserializeOwned,
{
    validate_length_prefixed_supervisor_frame(bytes)?;
    serde_json::from_slice(&bytes[4..]).map_err(invalid_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::env;
    use std::ffi::OsStr;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};

    #[test]
    fn startup_grant_rejects_eof_short_frame_and_wrong_binding() {
        let grant = PtyStartupGrant::new([7; 16], [9; CLEANUP_CAPABILITY_BYTES]);
        let frame = encode_startup_grant(&grant, "session-a", Path::new("/tmp/a.sock"), 42);

        assert!(
            read_and_validate_startup_grant(
                &mut io::empty(),
                "session-a",
                Path::new("/tmp/a.sock"),
                42,
            )
            .is_err()
        );
        assert!(
            read_and_validate_startup_grant(
                &mut &frame[..frame.len() - 1],
                "session-a",
                Path::new("/tmp/a.sock"),
                42,
            )
            .is_err()
        );
        assert!(
            read_and_validate_startup_grant(
                &mut frame.as_slice(),
                "session-b",
                Path::new("/tmp/a.sock"),
                42,
            )
            .is_err()
        );
        assert!(
            read_and_validate_startup_grant(
                &mut frame.as_slice(),
                "session-a",
                Path::new("/tmp/a.sock"),
                43,
            )
            .is_err()
        );
    }

    struct NoopPtySession;

    impl PtySession for NoopPtySession {
        fn read(&mut self, _buffer: &mut [u8]) -> PtyResult<usize> {
            Ok(0)
        }

        fn write_all(&mut self, _bytes: &[u8]) -> PtyResult<()> {
            Ok(())
        }

        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::default(),
                process_id: None,
                retained_output: Vec::new(),
            })
        }

        fn terminate(&mut self) -> PtyResult<()> {
            Ok(())
        }

        fn try_wait(&mut self) -> PtyResult<Option<super::super::PtyExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> PtyResult<super::super::PtyExitStatus> {
            Ok(super::super::PtyExitStatus::exited(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    struct CloseCountingPtySession {
        terminate_count: Arc<AtomicU64>,
        wait_count: Arc<AtomicU64>,
    }

    impl PtySession for CloseCountingPtySession {
        fn read(&mut self, _buffer: &mut [u8]) -> PtyResult<usize> {
            Ok(0)
        }

        fn write_all(&mut self, _bytes: &[u8]) -> PtyResult<()> {
            Ok(())
        }

        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::default(),
                process_id: Some(42),
                retained_output: Vec::new(),
            })
        }

        fn terminate(&mut self) -> PtyResult<()> {
            self.terminate_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn try_wait(&mut self) -> PtyResult<Option<super::super::PtyExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> PtyResult<super::super::PtyExitStatus> {
            self.wait_count.fetch_add(1, Ordering::Relaxed);
            Ok(super::super::PtyExitStatus::exited(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    async fn test_supervisor_request(
        reader: &mut tokio::net::unix::OwnedReadHalf,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        request_id: u64,
        request: SupervisorRequest,
    ) -> PtyResult<SupervisorResponsePayload> {
        write_frame_async(
            writer,
            &SupervisorRequestEnvelope {
                request_id,
                request,
            },
        )
        .await?;
        loop {
            match read_frame_async::<SupervisorFrame>(reader).await? {
                SupervisorFrame::Response {
                    request_id: response_id,
                    response,
                } if response_id == request_id => return response.into_result(),
                SupervisorFrame::TerminalFrame { .. } => {}
                SupervisorFrame::Response { .. } => {
                    return Err(PtyError::Backend(
                        "test supervisor returned mismatched response".to_owned(),
                    ));
                }
            }
        }
    }

    async fn test_cleanup_authenticate(
        reader: &mut tokio::net::unix::OwnedReadHalf,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        request_id: u64,
        session_id: &str,
        capability: &[u8; CLEANUP_CAPABILITY_BYTES],
    ) -> PtyResult<()> {
        let mut client_nonce = [0_u8; CLEANUP_AUTH_NONCE_BYTES];
        client_nonce[0] = 1;
        let (server_nonce_base64, _) = test_supervisor_request(
            reader,
            writer,
            request_id,
            SupervisorRequest::CleanupAuthChallenge {
                session_id: session_id.to_owned(),
                client_nonce_base64: general_purpose::STANDARD.encode(client_nonce),
            },
        )
        .await?
        .into_cleanup_auth_challenge()?;
        test_supervisor_request(
            reader,
            writer,
            request_id + 1,
            SupervisorRequest::CleanupAuthenticate {
                session_id: session_id.to_owned(),
                client_nonce_base64: general_purpose::STANDARD.encode(client_nonce),
                server_nonce_base64,
                capability_base64: general_purpose::STANDARD.encode(capability),
            },
        )
        .await?
        .expect_empty()
    }

    struct BurstPtySession {
        remaining_chunks: usize,
    }

    impl PtySession for BurstPtySession {
        fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
            if self.remaining_chunks == 0 {
                return Ok(0);
            }
            self.remaining_chunks -= 1;
            buffer[..4].copy_from_slice(b"xxxx");
            Ok(4)
        }

        fn write_all(&mut self, _bytes: &[u8]) -> PtyResult<()> {
            Ok(())
        }

        fn resize(&mut self, _size: PtySize) -> PtyResult<()> {
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: PtySize::default(),
                process_id: None,
                retained_output: Vec::new(),
            })
        }

        fn terminate(&mut self) -> PtyResult<()> {
            Ok(())
        }

        fn try_wait(&mut self) -> PtyResult<Option<super::super::PtyExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> PtyResult<super::super::PtyExitStatus> {
            Ok(super::super::PtyExitStatus::exited(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    fn test_restore_info() -> PtyRestoreInfo {
        PtyRestoreInfo::UnixSocket {
            socket_path: PathBuf::from("/tmp/termd-supervisor-test.sock"),
            supervisor_pid: 42,
            supervisor_status: PtySupervisorStatus::Running,
        }
    }

    fn test_supervisor_client_with_queues(
        writer: StdUnixStream,
        pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
        pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
        output_signal_tx: watch::Sender<u64>,
        output_signal_rx: watch::Receiver<u64>,
    ) -> SupervisorPtySession {
        SupervisorPtySession {
            session_id: "test-session".to_owned(),
            restore_info: test_restore_info(),
            supervisor_child: Arc::new(StdMutex::new(None)),
            writer: StdMutex::new(writer),
            pending_requests: Arc::new(StdMutex::new(HashMap::new())),
            pending_output,
            pending_terminal_frames,
            terminal_mirror: Arc::new(StdMutex::new(SupervisorTerminalMirror::new(PtySize::new(
                24, 80,
            )))),
            bootstrap_terminal_frames: Arc::new(StdMutex::new(None)),
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
            close_operation_id: StdMutex::new(None),
            cleanup_capability: None,
            exited: Arc::new(AtomicBool::new(false)),
        }
    }

    const TEST_SUPERVISOR_FRAME_MAX_BYTES: usize = 8 * 1024 * 1024;
    const TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES: usize = 128;
    const TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

    fn test_bounded_output_channel<T>() -> (
        BoundedSupervisorOutputSender<T>,
        BoundedSupervisorOutputReceiver<T>,
    ) {
        let (tx, rx, _overflow_rx) = bounded_supervisor_output_channel();
        (tx, rx)
    }

    #[tokio::test]
    async fn shared_output_budget_releases_on_consume_drop_and_failed_send() {
        let budget = Arc::new(SupervisorOutputQueueBudget::default());
        let (first_tx, mut first_rx, _) =
            bounded_supervisor_output_channel_with_budget::<u8>(Arc::clone(&budget));
        let (second_tx, mut second_rx, _) =
            bounded_supervisor_output_channel_with_budget::<u8>(Arc::clone(&budget));
        first_tx
            .try_send(1, TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES)
            .expect("first lane should reserve the full shared byte budget");
        let held = first_rx
            .recv_reserved()
            .await
            .expect("reserved message should be received");
        assert!(second_tx.try_send(2, 1).is_err());
        drop(held);
        second_tx
            .try_send(2, 1)
            .expect("dropping the in-flight message should release its reservation");
        assert_eq!(second_rx.recv().await, Some(2));

        let (closed_tx, closed_rx, _) =
            bounded_supervisor_output_channel_with_budget::<u8>(Arc::clone(&budget));
        drop(closed_rx);
        assert!(
            closed_tx
                .try_send(3, TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES)
                .is_err()
        );
        second_tx
            .try_send(4, TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES)
            .expect("failed channel send must release its reservation");
        assert_eq!(second_rx.recv().await, Some(4));

        for _ in 0..64 {
            first_tx
                .try_send(5, 0)
                .expect("first lane message should reserve");
            second_tx
                .try_send(6, 0)
                .expect("second lane message should reserve");
        }
        assert!(first_tx.try_send(7, 0).is_err());
        assert_eq!(first_rx.recv().await, Some(5));
        first_tx
            .try_send(7, 0)
            .expect("consuming one message should release the shared count budget");
    }

    #[test]
    fn length_prefixed_decoder_rejects_u32_max_as_oversized() {
        let error =
            decode_length_prefixed_json::<SupervisorTerminalClientFrame>(&u32::MAX.to_le_bytes())
                .expect_err("u32::MAX frame prefix must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("exceeds maximum"),
            "oversized prefix should report the shared frame limit: {error}"
        );
    }

    #[test]
    fn supervisor_frame_limit_counts_the_four_byte_prefix() {
        assert_eq!(
            checked_supervisor_payload_len(
                ((TEST_SUPERVISOR_FRAME_MAX_BYTES - 4) as u32).to_le_bytes()
            )
            .expect("payload at the exact total-frame limit should be accepted"),
            TEST_SUPERVISOR_FRAME_MAX_BYTES - 4
        );
        let error = checked_supervisor_payload_len(
            ((TEST_SUPERVISOR_FRAME_MAX_BYTES - 3) as u32).to_le_bytes(),
        )
        .expect_err("payload one byte above the total-frame limit must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_json_writer_accepts_exact_payload_limit_and_rejects_one_byte_more() {
        let exact = "x".repeat(TEST_SUPERVISOR_FRAME_MAX_BYTES - 6);
        assert_eq!(
            supervisor_json_payload_len(&exact)
                .expect("exact-limit JSON payload should be accepted"),
            TEST_SUPERVISOR_FRAME_MAX_BYTES - 4
        );
        let oversized = "x".repeat(TEST_SUPERVISOR_FRAME_MAX_BYTES - 5);
        let error = supervisor_json_payload_len(&oversized)
            .expect_err("one-byte oversized JSON payload must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn sync_frame_reader_accepts_exact_limit_prefix_before_payload_read() {
        let (mut writer, mut reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 4) as u32).to_le_bytes())
            .expect("test prefix should write");
        drop(writer);

        let error = read_frame_sync::<SupervisorRequestEnvelope>(&mut reader)
            .expect_err("missing accepted payload should end with EOF");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn sync_frame_reader_rejects_oversized_prefix_before_payload_read() {
        let (mut writer, mut reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 3) as u32).to_le_bytes())
            .expect("test prefix should write");
        drop(writer);

        let error = read_frame_sync::<SupervisorRequestEnvelope>(&mut reader)
            .expect_err("oversized sync frame must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn raw_frame_reader_rejects_oversized_prefix_before_payload_read() {
        let (mut writer, mut reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 3) as u32).to_le_bytes())
            .expect("test prefix should write");
        drop(writer);

        let error =
            read_raw_frame_sync(&mut reader).expect_err("oversized raw frame must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn raw_frame_reader_accepts_exact_limit_prefix_before_payload_read() {
        let (mut writer, mut reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 4) as u32).to_le_bytes())
            .expect("test prefix should write");
        drop(writer);

        let error = read_raw_frame_sync(&mut reader)
            .expect_err("missing accepted raw payload should end with EOF");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frame_writer_rejects_oversized_serialized_payload_before_socket_write() {
        let (mut writer, mut peer) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("test peer timeout should configure");
        let frame = SupervisorTerminalClientFrame::Input {
            data: vec![0_u8; 6 * 1024 * 1024],
        };

        let error = write_frame_sync(&mut writer, &frame)
            .expect_err("oversized serialized frame must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds maximum"));
        let mut byte = [0_u8; 1];
        assert!(
            peer.read(&mut byte).is_err(),
            "writer must not emit a prefix"
        );
    }

    #[tokio::test]
    async fn async_frame_reader_rejects_oversized_prefix_before_payload_read() {
        let (std_writer, std_reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        std_writer
            .set_nonblocking(true)
            .expect("test writer should become nonblocking");
        std_reader
            .set_nonblocking(true)
            .expect("test reader should become nonblocking");
        let writer = UnixStream::from_std(std_writer).expect("Tokio writer should open");
        let reader = UnixStream::from_std(std_reader).expect("Tokio reader should open");
        let (_reader_peer, mut writer) = writer.into_split();
        let (mut reader, _writer_peer) = reader.into_split();
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 3) as u32).to_le_bytes())
            .await
            .expect("test prefix should write");
        writer.shutdown().await.expect("test writer should close");

        let error = read_frame_async::<SupervisorRequestEnvelope>(&mut reader)
            .await
            .expect_err("oversized async frame must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[tokio::test]
    async fn async_frame_reader_accepts_exact_limit_prefix_before_payload_read() {
        let (std_writer, std_reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        std_writer
            .set_nonblocking(true)
            .expect("test writer should become nonblocking");
        std_reader
            .set_nonblocking(true)
            .expect("test reader should become nonblocking");
        let writer = UnixStream::from_std(std_writer).expect("Tokio writer should open");
        let reader = UnixStream::from_std(std_reader).expect("Tokio reader should open");
        let (_reader_peer, mut writer) = writer.into_split();
        let (mut reader, _writer_peer) = reader.into_split();
        writer
            .write_all(&((TEST_SUPERVISOR_FRAME_MAX_BYTES - 4) as u32).to_le_bytes())
            .await
            .expect("test prefix should write");
        writer.shutdown().await.expect("test writer should close");

        let error = read_frame_async::<SupervisorRequestEnvelope>(&mut reader)
            .await
            .expect_err("missing accepted async payload should end with EOF");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bootstrap_sync_publishes_authoritative_tail_before_concurrent_live_frame() {
        let (mut writer, reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_frames = Arc::new(StdMutex::new(VecDeque::new()));
        let terminal_mirror = Arc::new(StdMutex::new(SupervisorTerminalMirror::new(PtySize::new(
            4, 40,
        ))));
        terminal_mirror
            .lock()
            .expect("terminal mirror mutex should not be poisoned")
            .apply_frame(&PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"one".to_vec(),
            });
        let (response_tx, response_rx) = mpsc::channel();
        pending_requests
            .lock()
            .expect("pending request mutex should not be poisoned")
            .insert(7, response_tx);
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let reader_pending_requests = Arc::clone(&pending_requests);
        let reader_pending_frames = Arc::clone(&pending_frames);
        let reader_terminal_mirror = Arc::clone(&terminal_mirror);
        let bootstrap_terminal_frames = Arc::new(StdMutex::new(Some(VecDeque::new())));
        let reader_bootstrap_terminal_frames = Arc::clone(&bootstrap_terminal_frames);
        let reader_signal = output_signal_tx.clone();
        let reader_thread = thread::spawn(move || {
            supervisor_reader_loop(
                reader,
                reader_pending_requests,
                reader_pending_frames,
                reader_terminal_mirror,
                reader_bootstrap_terminal_frames,
                reader_signal,
                Arc::new(AtomicBool::new(false)),
            );
        });
        let sync = SupervisorAttachSyncPayload {
            controller_id: 3,
            controller_connection_id: 4,
            base_seq: 5,
            snapshot: SupervisorSnapshotPayload {
                size: PtySize::new(4, 40),
                process_id: Some(42),
                exited: false,
                retained_output: Vec::new(),
            },
            frames: (2..=5)
                .map(|terminal_seq| PtyTerminalFrame::Output {
                    terminal_seq,
                    data: terminal_seq.to_string().into_bytes(),
                })
                .collect(),
        };
        write_frame_sync(
            &mut writer,
            &SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 6,
                    data: b"six".to_vec(),
                },
            },
        )
        .expect("concurrent live frame should write");
        write_frame_sync(
            &mut writer,
            &SupervisorFrame::Response {
                request_id: 7,
                response: SupervisorResponse::ok(SupervisorResponsePayload::AttachSync(
                    sync.clone(),
                )),
            },
        )
        .expect("attach sync response should write");
        drop(writer);
        let received_sync = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("attach sync response should arrive")
            .into_result()
            .unwrap_or_else(|_| panic!("attach sync response should succeed"))
            .into_attach_sync()
            .expect("response should contain attach sync");
        reader_thread.join().expect("reader thread should stop");

        let (session_writer, _peer) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        let session = SupervisorPtySession {
            session_id: "test-session".to_owned(),
            restore_info: test_restore_info(),
            supervisor_child: Arc::new(StdMutex::new(None)),
            writer: StdMutex::new(session_writer),
            pending_requests,
            pending_output: Arc::new(StdMutex::new(VecDeque::new())),
            pending_terminal_frames: Arc::clone(&pending_frames),
            terminal_mirror,
            bootstrap_terminal_frames,
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(8),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::new(4, 40)),
            cached_process_id: StdMutex::new(Some(42)),
            close_operation_id: StdMutex::new(None),
            cleanup_capability: None,
            exited: Arc::new(AtomicBool::new(false)),
        };
        session.seed_attach_sync(received_sync);

        let terminal_seqs = pending_frames
            .lock()
            .expect("pending terminal frame mutex should not be poisoned")
            .iter()
            .filter_map(PtyTerminalFrame::terminal_seq)
            .collect::<Vec<_>>();
        assert_eq!(terminal_seqs, vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn daemon_reader_replaces_detached_output_backlog_with_bounded_snapshot() {
        let (mut writer, reader) =
            StdUnixStream::pair().expect("test Unix stream pair should open");
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_frames = Arc::new(StdMutex::new(VecDeque::new()));
        let terminal_mirror = Arc::new(StdMutex::new(SupervisorTerminalMirror::new(PtySize::new(
            4, 40,
        ))));
        let (output_signal_tx, _output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let reader_pending_frames = Arc::clone(&pending_frames);
        let reader_thread = thread::spawn(move || {
            supervisor_reader_loop(
                reader,
                pending_requests,
                reader_pending_frames,
                terminal_mirror,
                Arc::new(StdMutex::new(None)),
                output_signal_tx,
                Arc::new(AtomicBool::new(false)),
            );
        });

        for terminal_seq in 1..=TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES as u64 + 1 {
            write_frame_sync(
                &mut writer,
                &SupervisorFrame::TerminalFrame {
                    frame: PtyTerminalFrame::Output {
                        terminal_seq,
                        data: vec![b'x'; 64 * 1024],
                    },
                },
            )
            .expect("test terminal frame should write");
        }
        drop(writer);
        reader_thread.join().expect("reader thread should stop");

        let pending = pending_frames
            .lock()
            .expect("pending terminal frame mutex should not be poisoned");
        let retained_bytes = pending
            .iter()
            .map(|frame| frame.bytes_for_legacy_read().map_or(1, <[u8]>::len))
            .sum::<usize>();
        assert!(pending.len() <= TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES);
        assert!(retained_bytes <= TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_BYTES);
        assert!(
            pending
                .iter()
                .any(|frame| matches!(frame, PtyTerminalFrame::Snapshot { base_seq: 129, .. })),
            "overflow must replace stale live output with an authoritative snapshot"
        );
    }

    #[test]
    fn slow_controller_overflow_disconnects_only_consumer_and_allows_reattach() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (controller_tx, _controller_rx) = test_bounded_output_channel();
        state.attach_sync(controller_tx, Some(42), None);

        let output = vec![b'x'; 64 * 1024];
        for _ in 0..=TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES {
            let frame = state.record_output(&output);
            state.broadcast_terminal_frame("session", frame);
        }

        assert!(
            state.controllers.is_empty(),
            "slow controller must be detached without terminating the session"
        );

        let (reattach_tx, mut reattach_rx) = test_bounded_output_channel();
        let (_identity, sync) = state.attach_sync(reattach_tx, Some(42), None);
        assert!(matches!(
            sync.frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq: 129, .. }]
        ));

        let recovery = state.record_output(b"recovered");
        state.broadcast_terminal_frame("session", recovery);
        assert!(matches!(
            reattach_rx.try_recv(),
            Ok(SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 130,
                    data,
                },
            }) if data == b"recovered"
        ));
    }

    #[test]
    fn slow_terminal_attach_overflow_disconnects_only_consumer_and_allows_reattach() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (attach_tx, _attach_rx) = test_bounded_output_channel();
        state.register_terminal_attach(attach_tx);

        let output = vec![b'x'; 64 * 1024];
        for _ in 0..=TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES {
            let frame = state.record_output(&output);
            state.broadcast_terminal_frame("session", frame);
        }

        assert!(
            state.terminal_attaches.is_empty(),
            "slow attach must be detached without terminating the session"
        );

        let (reattach_tx, mut reattach_rx) = test_bounded_output_channel();
        state.register_terminal_attach(reattach_tx);
        let recovery = state.record_output(b"recovered");
        state.broadcast_terminal_frame("session", recovery);
        assert!(matches!(
            reattach_rx.try_recv(),
            Ok(SupervisorTerminalServerFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 130,
                    data,
                },
                ..
            }) if data == b"recovered"
        ));
    }

    #[test]
    fn supervisor_runtime_dir_uses_shared_base_directory_for_relative_state_paths() {
        let current_dir = env::current_dir().expect("current dir should exist");

        let runtime_dir = supervisor_runtime_dir(Path::new("daemon-state.json"))
            .expect("relative state path should have a safe runtime directory");

        assert_eq!(runtime_dir.parent(), Some(current_dir.as_path()));
        // 中文注释：长 worktree 路径下 runtime dir 会降级成 `ts` 以满足 Unix socket
        // 路径长度限制；这个测试只关心相对 state path 仍共享当前目录作为基准。
        assert!(matches!(
            runtime_dir.file_name(),
            Some(name)
                if name == OsStr::new("termd-supervisors")
                    || name == OsStr::new("ts")
                    || name == OsStr::new("t")
        ));
    }

    #[test]
    fn supervisor_runtime_dir_uses_state_parent_for_absolute_state_paths() {
        let runtime_dir = supervisor_runtime_dir(Path::new("/var/lib/termd/daemon-state.json"))
            .expect("absolute state path should have a safe runtime directory");

        assert_eq!(runtime_dir.parent(), Some(Path::new("/var/lib/termd")));
        assert_eq!(runtime_dir, Path::new("/var/lib/termd/termd-supervisors"));
    }

    #[test]
    fn supervisor_socket_file_name_stays_short_under_long_state_directory() {
        let state_path = Path::new(
            "/tmp/termd-server-test-1234567890-1234567890-1234567890-very-long-state-name/daemon-state.json",
        );
        let backend = SupervisorPtyBackend::for_state_path(state_path);
        let socket_path = backend
            .socket_path_for_session(
                "123e4567-e89b-12d3-a456-426614174000-this-session-name-is-deliberately-long",
            )
            .expect("long state directory should still have a safe socket path");

        assert!(
            socket_path.to_string_lossy().len() < 108,
            "supervisor socket path must stay under Unix socket path limits: {}",
            socket_path.display()
        );
    }

    #[test]
    fn supervisor_attach_socket_file_name_stays_short_under_long_state_directory() {
        let state_path = Path::new(
            "/tmp/termd-server-test-1234567890-1234567890-1234567890-very-long-state-name/daemon-state.json",
        );
        let backend = SupervisorPtyBackend::for_state_path(state_path);
        let session_id =
            "123e4567-e89b-12d3-a456-426614174000-this-session-name-is-deliberately-long";
        let control_socket_path = backend
            .socket_path_for_session(session_id)
            .expect("long state directory should still have a safe control socket path");
        let attach_socket_path =
            attach_socket_path_for_control_socket(&control_socket_path, session_id);

        assert!(
            attach_socket_path.to_string_lossy().len() < 108,
            "supervisor attach socket path must stay under Unix socket path limits: {}",
            attach_socket_path.display()
        );
    }

    #[test]
    fn supervisor_runtime_dir_falls_back_when_attach_socket_is_longer_than_control_socket() {
        let control_name_len = short_supervisor_socket_file_name("probe").len();
        let attach_name_len = short_supervisor_attach_socket_file_name("probe").len();
        assert!(attach_name_len > control_name_len);

        let preferred_dir_name = "termd-supervisors";
        let preferred_len = UNIX_SOCKET_PATH_MAX_BYTES - 2 - control_name_len;
        let base_prefix = "/tmp/";
        let base_dir_len = preferred_len
            .checked_sub(1 + preferred_dir_name.len())
            .expect("preferred runtime dir must leave room for base dir");
        let filler_len = base_dir_len
            .checked_sub(base_prefix.len())
            .expect("base dir length must fit under /tmp prefix");
        let base_dir = PathBuf::from(format!("{base_prefix}{}", "a".repeat(filler_len)));
        let state_path = base_dir.join("daemon-state.json");
        let preferred = base_dir.join(preferred_dir_name);

        assert!(
            preferred.to_string_lossy().len() + 1 + control_name_len < UNIX_SOCKET_PATH_MAX_BYTES,
            "control socket should still fit under preferred runtime dir",
        );
        assert!(
            preferred.to_string_lossy().len() + 1 + attach_name_len >= UNIX_SOCKET_PATH_MAX_BYTES,
            "attach socket should overflow preferred runtime dir so the fallback is exercised",
        );

        let runtime_dir = supervisor_runtime_dir(&state_path)
            .expect("short runtime subdirectory should still fit");
        assert_eq!(runtime_dir, base_dir.join("ts"));
    }

    #[test]
    fn supervisor_runtime_dir_preserves_107_byte_limit_and_rejects_108_bytes() {
        let attach_name_len = short_supervisor_attach_socket_file_name("probe").len();
        let base_prefix = "/tmp/";
        let base_dir = |socket_len| {
            let base_len = socket_len - 1 - "t".len() - 1 - attach_name_len;
            PathBuf::from(format!(
                "{base_prefix}{}",
                "b".repeat(base_len - base_prefix.len())
            ))
        };

        let exact = base_dir(UNIX_SOCKET_PATH_MAX_BYTES);
        assert_eq!(
            supervisor_runtime_dir(&exact.join("state.json")).unwrap(),
            exact.join("t"),
            "107-byte attach path must use the one-byte safe fallback"
        );
        let overflow = base_dir(UNIX_SOCKET_PATH_MAX_BYTES + 1);
        let error = supervisor_runtime_dir(&overflow.join("state.json"))
            .expect_err("108-byte attach path must fail explicitly");
        assert!(error.contains("exceeds 107 bytes"));
    }

    #[test]
    fn orphan_detection_selects_only_current_runtime_dir_unrecorded_supervisors() {
        let runtime_dir = Path::new("/var/lib/termd/termd-supervisors");
        let valid_session_ids = HashSet::from(["kept-session".to_owned()]);
        let supervisors = vec![
            SupervisorProcess {
                pid: 11,
                session_id: "kept-session".to_owned(),
                socket_path: runtime_dir.join("kept-session.sock"),
                size: Some(PtySize::default()),
            },
            SupervisorProcess {
                pid: 12,
                session_id: "orphan-session".to_owned(),
                socket_path: runtime_dir.join("orphan-session.sock"),
                size: Some(PtySize::default()),
            },
            SupervisorProcess {
                pid: 13,
                session_id: "orphan-session".to_owned(),
                socket_path: PathBuf::from("/tmp/other-termd-supervisors/orphan-session.sock"),
                size: Some(PtySize::default()),
            },
        ];

        let orphan_pids = orphaned_supervisor_pids(runtime_dir, &valid_session_ids, &supervisors);

        assert_eq!(orphan_pids, vec![12]);
    }

    struct SupervisorTestDir(PathBuf);

    impl SupervisorTestDir {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "termd-supervisor-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system clock should be after unix epoch")
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl std::ops::Deref for SupervisorTestDir {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<Path> for SupervisorTestDir {
        fn as_ref(&self) -> &Path {
            self
        }
    }

    impl Drop for SupervisorTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn slow_attach_proxy_consumer_gets_recoverable_overflow_error() {
        let runtime_dir = SupervisorTestDir::new("slow-attach-proxy");
        let socket_path = runtime_dir.join("attach.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("test listener should bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("proxy should connect");
            let _: SupervisorTerminalClientFrame =
                read_frame_sync(&mut stream).expect("bootstrap should decode");
            for terminal_seq in 1..=TEST_SUPERVISOR_OUTPUT_QUEUE_MAX_MESSAGES as u64 + 1 {
                write_frame_sync(
                    &mut stream,
                    &SupervisorTerminalServerFrame::TerminalFrame {
                        session_id: "session".to_owned(),
                        frame: PtyTerminalFrame::Output {
                            terminal_seq,
                            data: b"x".to_vec(),
                        },
                    },
                )
                .expect("test attach frame should write");
            }
        });
        let mut proxy = SupervisorAttachProxy::connect(
            "session",
            socket_path,
            "slow-proxy",
            PtyAttachmentBootstrap::default(),
        )
        .expect("proxy should connect");
        server.join().expect("test server should stop");

        let deadline = Instant::now() + Duration::from_secs(2);
        while proxy
            .reader_error
            .lock()
            .expect("reader error mutex should not be poisoned")
            .is_none()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }

        let error = proxy
            .read_frame()
            .expect_err("overflowed attach proxy must require a recoverable reattach");
        assert!(error.to_string().contains("reattach"));
        assert!(
            proxy
                .pending_frames
                .lock()
                .expect("pending frame mutex should not be poisoned")
                .is_empty(),
            "overflow must release stale queued output"
        );
    }

    fn path_mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    fn path_identity(path: &Path) -> SupervisorPathIdentity {
        let metadata = fs::symlink_metadata(path).unwrap();
        SupervisorPathIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn socket_path_with_length(runtime_dir: &Path, length: usize) -> PathBuf {
        let prefix_len = runtime_dir.as_os_str().as_bytes().len() + 1;
        assert!(
            prefix_len < length,
            "runtime directory must leave room for a socket name"
        );
        runtime_dir.join("s".repeat(length - prefix_len))
    }

    fn assert_no_supervisor_temp_sockets(runtime_dir: &Path) {
        assert!(
            fs::read_dir(runtime_dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".t")),
            "supervisor operation left a temporary socket"
        );
    }

    async fn assert_listener_accepts(listener: &BoundSupervisorListener, socket_path: &Path) {
        let client = StdUnixStream::connect(socket_path)
            .unwrap_or_else(|error| panic!("connect to {} failed: {error}", socket_path.display()));
        let (accepted, _) =
            tokio::time::timeout(Duration::from_secs(2), listener.listener.accept())
                .await
                .expect("listener accept timed out")
                .expect("listener accept failed");
        drop((client, accepted));
    }

    #[tokio::test]
    async fn supervisor_listener_accepts_real_107_byte_socket_path() {
        let runtime_dir = SupervisorTestDir::new("path-limit-107");
        let socket_path = socket_path_with_length(&runtime_dir, UNIX_SOCKET_PATH_MAX_BYTES);
        assert_eq!(
            socket_path.as_os_str().as_bytes().len(),
            UNIX_SOCKET_PATH_MAX_BYTES
        );

        let listener = bind_supervisor_listener(&socket_path, true)
            .expect("107-byte socket path must bind and listen");
        assert_listener_accepts(&listener, &socket_path).await;
        remove_supervisor_socket_if_current(&listener).unwrap();
    }

    #[tokio::test]
    async fn supervisor_listener_rejects_real_108_byte_socket_path_clearly() {
        let runtime_dir = SupervisorTestDir::new("path-limit-108");
        let socket_path = socket_path_with_length(&runtime_dir, UNIX_SOCKET_PATH_MAX_BYTES + 1);
        assert_eq!(
            socket_path.as_os_str().as_bytes().len(),
            UNIX_SOCKET_PATH_MAX_BYTES + 1
        );

        let error = match bind_supervisor_listener(&socket_path, true) {
            Ok(_) => panic!("108-byte socket path must not bind"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("Unix socket path exceeds 107 bytes"),
            "unexpected bind error: {error}"
        );
        assert!(!socket_path.exists());
        assert_no_supervisor_temp_sockets(&runtime_dir);
    }

    #[test]
    fn supervisor_runtime_directory_traversal_rejects_replaceable_components() {
        let root = SupervisorTestDir::new("runtime-components");
        let target_dir = root.join("target");
        let runtime_dir = root.join("termd-supervisors");
        fs::create_dir(&target_dir).unwrap();
        fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target_dir, &runtime_dir).unwrap();

        SecureRuntimeDir::open(&runtime_dir).expect_err("final symlink must be rejected");
        assert_eq!(path_mode(&target_dir), 0o755);

        fs::remove_file(&runtime_dir).unwrap();
        let linked_parent = root.join("linked-parent");
        let nested_runtime = linked_parent.join("nested/runtime");
        symlink(&target_dir, &linked_parent).unwrap();

        SecureRuntimeDir::open(&nested_runtime).expect_err("ancestor symlink must be rejected");
        assert!(!target_dir.join("nested").exists());
        assert_eq!(path_mode(&target_dir), 0o755);

        let writable_parent = root.join("writable-parent");
        let runtime_dir = writable_parent.join("nested").join("runtime");
        fs::create_dir(&writable_parent).unwrap();
        fs::set_permissions(&writable_parent, fs::Permissions::from_mode(0o777)).unwrap();
        SecureRuntimeDir::open(&runtime_dir).expect_err("writable ancestor must be rejected");
        assert!(!writable_parent.join("nested").exists());
    }

    #[test]
    fn supervisor_rejects_foreign_owned_final_dir_without_chmod() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        let root = SupervisorTestDir::new("foreign-owner");
        let runtime_dir = root.join("runtime");
        fs::create_dir(&runtime_dir).unwrap();
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let name = c_string_from_os_str(runtime_dir.as_os_str(), &runtime_dir).unwrap();
        assert_eq!(unsafe { libc::chown(name.as_ptr(), 65_534, u32::MAX) }, 0);
        SecureRuntimeDir::open(&runtime_dir).expect_err("foreign final dir must be rejected");
        assert_eq!(path_mode(&runtime_dir), 0o755);
        assert_eq!(unsafe { libc::chown(name.as_ptr(), 0, u32::MAX) }, 0);
    }

    #[tokio::test]
    async fn supervisor_rejects_socket_symlink_without_touching_target() {
        let runtime_dir = SupervisorTestDir::new("socket-symlink");
        let socket_path = runtime_dir.join("control.sock");
        let target_path = runtime_dir.join("socket-target");
        fs::write(&target_path, b"target").unwrap();
        fs::set_permissions(&target_path, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target_path, &socket_path).unwrap();

        assert!(bind_supervisor_listener(&socket_path, true).is_err());
        assert_eq!(path_mode(&target_path), 0o644);
    }

    #[tokio::test]
    async fn supervisor_listener_permissions_are_private_after_bind_and_rebind() {
        let root = SupervisorTestDir::new("listener-mode");
        let runtime_dir = root.join("runtime");
        fs::create_dir(&runtime_dir).unwrap();
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let socket_path = runtime_dir.join("control.sock");
        let mut listener = bind_supervisor_listener(&socket_path, true).unwrap();
        assert_eq!(path_mode(&runtime_dir), 0o700);
        assert_eq!(path_mode(&socket_path), 0o600);
        assert_listener_accepts(&listener, &socket_path).await;

        fs::remove_file(&socket_path).unwrap();
        ensure_supervisor_socket_bound(&socket_path, &mut listener).unwrap();
        assert_eq!(path_mode(&socket_path), 0o600);
        assert_listener_accepts(&listener, &socket_path).await;
    }

    #[tokio::test]
    async fn supervisor_temp_bind_collision_exhaustion_leaves_target_unchanged() {
        let root = SupervisorTestDir::new("temp-collision");
        let runtime_dir = root.join("runtime");
        let socket_path = runtime_dir.join("control.sock");
        let listener = bind_supervisor_listener(&socket_path, true).unwrap();
        let target_identity = path_identity(&socket_path);
        let collision_name = CString::new(".tcollision").unwrap();
        let collision_path = runtime_dir.join(OsStr::from_bytes(collision_name.as_bytes()));
        let collision_listener = StdUnixListener::bind(&collision_path).unwrap();

        let error = match bind_supervisor_temp_listener_with_name(
            &listener.runtime_dir,
            &socket_path,
            || Ok(collision_name.clone()),
        ) {
            Ok(_) => panic!("repeated temp-name collisions must exhaust the retry budget"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("failed to allocate a unique temporary supervisor socket"),
            "unexpected collision error: {error}"
        );

        drop(collision_listener);
        fs::remove_file(&collision_path).unwrap();
        assert_no_supervisor_temp_sockets(&runtime_dir);
        assert_eq!(path_identity(&socket_path), target_identity);
        assert_listener_accepts(&listener, &socket_path).await;
    }

    #[tokio::test]
    async fn supervisor_linkat_unsupported_cleans_temp_and_preserves_target() {
        fn unsupported_linkat(
            _runtime_dir: &SecureRuntimeDir,
            _from: &CString,
            _to: &CString,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "injected linkat unsupported",
            ))
        }

        let root = SupervisorTestDir::new("linkat-unsupported");
        let runtime_dir = root.join("runtime");
        let socket_path = runtime_dir.join("control.sock");
        let listener = bind_supervisor_listener(&socket_path, true).unwrap();
        let target_identity = path_identity(&socket_path);

        let error = match bind_supervisor_listener_in_runtime_dir_with_publish(
            &socket_path,
            listener.runtime_dir.try_clone().unwrap(),
            listener.socket_name.clone(),
            false,
            unsupported_linkat,
        ) {
            Ok(_) => panic!("unsupported linkat publication must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("injected linkat unsupported"),
            "unexpected publication error: {error}"
        );
        assert_no_supervisor_temp_sockets(&runtime_dir);
        assert_eq!(path_identity(&socket_path), target_identity);
        assert_listener_accepts(&listener, &socket_path).await;
    }

    #[tokio::test]
    async fn supervisor_publish_and_cleanup_preserve_replaced_socket() {
        let root = SupervisorTestDir::new("replaced-socket");
        let runtime_dir = root.join("runtime");
        let socket_path = runtime_dir.join("control.sock");
        let listener = bind_supervisor_listener(&socket_path, true).unwrap();

        fs::remove_file(&socket_path).unwrap();
        let _replacement = StdUnixListener::bind(&socket_path).unwrap();
        let replacement_identity = path_identity(&socket_path);
        remove_supervisor_socket_if_current(&listener)
            .expect_err("cleanup must reject replacement");

        let runtime = listener.runtime_dir.try_clone().unwrap();
        let name = listener.socket_name.clone();
        assert!(
            bind_supervisor_listener_in_runtime_dir(&socket_path, runtime, name, false).is_err()
        );

        assert_eq!(path_identity(&socket_path), replacement_identity);
        assert!(
            fs::read_dir(&runtime_dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".t")),
            "failed publish left a temporary socket"
        );
    }

    #[tokio::test]
    async fn supervisor_repair_and_cleanup_use_original_runtime_dir_inode() {
        let root = SupervisorTestDir::new("runtime-replaced");
        let runtime_dir = root.join("runtime");
        let original_runtime_dir = root.join("original-runtime");
        let socket_path = runtime_dir.join("control.sock");
        let mut listener = bind_supervisor_listener(&socket_path, true).unwrap();

        fs::rename(&runtime_dir, &original_runtime_dir).unwrap();
        fs::create_dir(&runtime_dir).unwrap();
        let replacement_socket_path = runtime_dir.join("control.sock");
        let _replacement = StdUnixListener::bind(&replacement_socket_path).unwrap();

        ensure_supervisor_socket_bound(&socket_path, &mut listener)
            .expect_err("repair must reject replaced runtime dir");
        remove_supervisor_socket_if_current(&listener).unwrap();
        assert!(replacement_socket_path.exists());
        assert!(!original_runtime_dir.join("control.sock").exists());
    }

    #[test]
    fn supervisor_snapshot_remains_screen_derived_after_large_control_output() {
        let mut state = SupervisorState::new(PtySize::new(8, 80));
        state.record_output(b"\x1b[4;1Hcurrent status");

        let mut osc_noise = Vec::with_capacity(64 * 1024);
        while osc_noise.len() < 64 * 1024 {
            osc_noise.extend_from_slice(b"\x1b]0;ignored-title\x07");
        }
        state.record_output(&osc_noise);

        let snapshot = String::from_utf8_lossy(&state.snapshot_output()).to_string();
        assert!(
            snapshot.contains("current status"),
            "supervisor snapshot should be screen-derived, got {snapshot:?}"
        );
    }

    #[test]
    fn supervisor_terminal_snapshot_uses_session_seq_and_tail_window() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let first = state.record_output(b"alpha\n");
        let second = state.record_output(b"beta\n");
        assert_eq!(first.terminal_seq(), Some(1));
        assert_eq!(second.terminal_seq(), Some(2));

        let snapshot = state.terminal_snapshot_or_tail_with_base(None).1;
        assert_eq!(snapshot.len(), 1);
        assert!(matches!(
            snapshot[0],
            PtyTerminalFrame::Snapshot { base_seq: 2, .. }
        ));

        let tail = state.terminal_snapshot_or_tail_with_base(Some(1)).1;
        assert_eq!(tail, vec![second]);

        assert!(
            state
                .terminal_snapshot_or_tail_with_base(Some(2))
                .1
                .is_empty(),
            "up-to-date clients do not need snapshot or tail"
        );
    }

    #[test]
    fn attach_sync_returns_snapshot_and_live_boundary_for_new_controller() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        state.record_output(b"before\n");
        let (controller_tx, mut controller_rx) = test_bounded_output_channel();

        let (_controller_id, sync) = state.attach_sync(controller_tx, Some(42), None);

        assert_eq!(sync.base_seq, 1);
        assert!(
            sync.snapshot.retained_output.is_empty(),
            "terminal attach 首屏只能由 sequenced snapshot frame 承载，不能再走 retained_output"
        );
        assert!(matches!(
            sync.frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq: 1, .. }]
        ));

        let frame = state.record_output(b"after\n");
        state.broadcast_terminal_frame("session-under-test", frame);
        assert!(matches!(
            controller_rx
                .try_recv()
                .expect("live frame should be sent to the attached controller"),
            SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 2,
                    ..
                }
            }
        ));
    }

    #[test]
    fn attach_sync_returns_tail_when_last_seq_is_in_journal() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        state.record_output(b"alpha\n");
        state.record_output(b"beta\n");
        let (controller_tx, _controller_rx) = test_bounded_output_channel();

        let (_controller_id, sync) = state.attach_sync(controller_tx, Some(42), Some(1));

        assert_eq!(sync.base_seq, 2);
        assert!(
            sync.snapshot.retained_output.is_empty(),
            "tail attach 也不能混入 legacy retained_output"
        );
        assert_eq!(
            sync.frames,
            vec![PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"beta\n".to_vec(),
            }]
        );
    }

    #[test]
    fn supervisor_state_tracks_attached_devices_independently_from_controllers() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));

        assert!(!state.has_attached_device("dev-a"));

        state.attach_device("dev-a");
        state.attach_device("dev-a");
        state.attach_device("dev-b");

        assert!(state.has_attached_device("dev-a"));
        assert!(state.has_attached_device("dev-b"));

        state.detach_device("dev-a");

        assert!(!state.has_attached_device("dev-a"));
        assert!(state.has_attached_device("dev-b"));
    }

    #[test]
    fn attach_sync_prefers_snapshot_when_tail_is_much_larger_than_snapshot() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        for index in 0..2000 {
            state.record_output(
                format!(
                    "line-{index:04} 0123456789abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUV\n"
                )
                .as_bytes(),
            );
        }
        let (controller_tx, _controller_rx) = test_bounded_output_channel();

        let (_controller_id, sync) = state.attach_sync(controller_tx, Some(42), Some(0));

        assert_eq!(sync.base_seq, 2000);
        assert!(matches!(
            sync.frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq, data, .. }]
                if *base_seq == 2000 && data.len() < TERMINAL_ATTACH_TAIL_MAX_BYTES
        ));
    }

    #[test]
    fn attach_sync_keeps_multiple_controllers_and_broadcasts_live_output() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (old_tx, mut old_rx) = test_bounded_output_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, mut new_rx) = test_bounded_output_channel();
        let (new_id, _new_sync) = state.attach_sync(new_tx, Some(42), Some(0));

        assert_ne!(old_id, new_id);
        assert_eq!(state.controllers.len(), 2);

        let frame = state.record_output(b"shared-live-output\n");
        state.broadcast_terminal_frame("session-under-test", frame);

        assert!(
            old_rx.try_recv().is_ok(),
            "old controller should keep receiving live frames after another attach"
        );
        assert!(
            new_rx.try_recv().is_ok(),
            "new controller should receive live frames after attach"
        );
    }

    #[tokio::test]
    async fn attach_sync_only_allows_requests_from_latest_controller_id() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (old_tx, _old_rx) = test_bounded_output_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, _new_rx) = test_bounded_output_channel();
        let (new_id, _new_sync) = state.attach_sync(new_tx, Some(42), Some(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new("session-under-test".to_owned()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(state)),
            shutdown_tx,
        };

        let old_error = ensure_current_controller(&shared, Some(old_id))
            .await
            .expect_err("old controller id should lose authority after takeover");
        assert!(
            old_error.to_string().contains("not the active controller"),
            "old controller should be rejected as stale owner"
        );
        ensure_current_controller(&shared, Some(new_id))
            .await
            .expect("new controller id should become active owner");
    }

    #[test]
    fn resume_attach_sync_rejects_stale_controller_after_active_owner_disconnects() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (old_tx, _old_rx) = test_bounded_output_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, _new_rx) = test_bounded_output_channel();
        let (new_id, _new_sync) = state.attach_sync(new_tx, Some(42), Some(0));

        state.remove_controller(new_id);
        assert_eq!(
            state.active_controller_id, None,
            "active owner disconnect should clear only the live owner slot"
        );
        assert_eq!(
            state.active_controller_connection_id, None,
            "active owner disconnect should clear the live owner connection slot too"
        );
        assert_eq!(
            state.controller_resume_lease_id,
            Some(new_id.id),
            "resume lease must stay on the latest owner after disconnect"
        );

        let error = state
            .resume_attach_sync(old_id.id, Some(42), Some(0))
            .expect_err("stale controller must not recover after newer owner disconnects");
        assert!(
            error.to_string().contains("not the active controller"),
            "stale controller should stay fenced even after active slot is empty"
        );
    }

    #[tokio::test]
    async fn resume_attach_sync_allows_latest_lease_holder_after_disconnect() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (owner_tx, _owner_rx) = test_bounded_output_channel();
        let (owner_identity, _owner_sync) = state.attach_sync(owner_tx, Some(42), None);

        state.remove_controller(owner_identity);
        let (resumed_identity, _resumed_sync) = state
            .resume_attach_sync(owner_identity.id, Some(42), Some(0))
            .expect("latest lease holder should be able to resume after disconnect");

        assert_eq!(resumed_identity.id, owner_identity.id);
        assert_ne!(
            resumed_identity.connection_id, owner_identity.connection_id,
            "resume should mint a fresh connection generation for the same lease holder"
        );
        assert_eq!(state.active_controller_id, None);
        assert_eq!(
            state.active_controller_connection_id, None,
            "candidate resume must not mutate owner generation before commit"
        );
        assert!(state.controllers.is_empty());
        let (retry_tx, _retry_rx) = test_bounded_output_channel();
        state
            .commit_controller_resume(1, resumed_identity, retry_tx)
            .expect("validated candidate should commit");
        assert_eq!(state.active_controller_id, Some(owner_identity.id));
        assert_eq!(
            state.active_controller_connection_id,
            Some(resumed_identity.connection_id)
        );
    }

    #[tokio::test]
    async fn resume_attach_sync_fences_previous_connection_generation_of_same_controller() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (owner_tx, _owner_rx) = test_bounded_output_channel();
        let (owner_identity, _owner_sync) = state.attach_sync(owner_tx, Some(42), None);

        state.remove_controller(owner_identity);
        let (resumed_identity, _resumed_sync) = state
            .resume_attach_sync(owner_identity.id, Some(42), Some(0))
            .expect("lease holder resume should succeed");
        let (retry_tx, _retry_rx) = test_bounded_output_channel();
        state
            .commit_controller_resume(1, resumed_identity, retry_tx)
            .expect("validated candidate should commit");
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new("session-under-test".to_owned()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(state)),
            shutdown_tx,
        };

        let old_error = ensure_current_controller(&shared, Some(owner_identity))
            .await
            .expect_err("old connection generation should be fenced after resume");
        assert!(
            old_error.to_string().contains("not the active controller"),
            "stale generation of same lease holder must be rejected"
        );
        ensure_current_controller(&shared, Some(resumed_identity))
            .await
            .expect("resumed connection generation should become the only active owner");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_resume_validation_keeps_old_controller_usable_without_leaks() {
        let runtime_dir = SupervisorTestDir::new("resume-validation");
        let socket_path = runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let session_id = "resume-validation-session".to_owned();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };
        let accept_shared = shared.clone();
        let accept_session_id = session_id.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("test connection should accept");
                let connection_shared = accept_shared.clone();
                let expected_session_id = accept_session_id.clone();
                tokio::spawn(async move {
                    let _ = handle_supervisor_connection(
                        connection_shared,
                        expected_session_id,
                        stream,
                    )
                    .await;
                });
            }
        });

        let mut client =
            SupervisorPtySession::connect(&session_id, socket_path, std::process::id(), None)
                .expect("initial controller should connect");
        client.attach_device("dev-a").unwrap();
        let original = client.current_controller_id().expect("controller identity");

        for failed_point in [
            ResumeValidationPoint::AttachSync,
            ResumeValidationPoint::Ping,
            ResumeValidationPoint::Snapshot,
            ResumeValidationPoint::WriterClone,
            ResumeValidationPoint::ReaderThread,
        ] {
            let error = client
                .reconnect_ipc_with_validation(|point| {
                    if point == failed_point {
                        Err(PtyError::Backend(format!("injected {point:?} failure")))
                    } else {
                        Ok(())
                    }
                })
                .expect_err("candidate validation failure must abort resume");
            assert!(error.to_string().contains("injected"));
            assert!(client.has_attached_device("dev-a").unwrap());
            client.write_all(b"old-controller-still-usable").unwrap();

            let state = shared.state.lock().await;
            assert_eq!(state.controller_resume_lease_id, Some(original.id));
            assert_eq!(state.active_controller_id, Some(original.id));
            assert_eq!(
                state.active_controller_connection_id,
                Some(original.connection_id)
            );
            assert_eq!(state.controllers.len(), 1);
            assert!(state.has_attached_device("dev-a"));
        }

        client
            .reconnect_ipc()
            .expect("validated resume should commit");
        let resumed = client.current_controller_id().expect("resumed identity");
        assert_eq!(resumed.id, original.id);
        assert_ne!(resumed.connection_id, original.connection_id);
        assert!(client.has_attached_device("dev-a").unwrap());
        client.write_all(b"resumed-controller-usable").unwrap();
        let state = shared.state.lock().await;
        assert_eq!(state.controllers.len(), 1);
        assert_eq!(
            state.active_controller_connection_id,
            Some(resumed.connection_id)
        );
        drop(state);

        accept_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_resume_commit_response_replays_same_transaction_and_keeps_controller() {
        let runtime_dir = SupervisorTestDir::new("resume-response-loss");
        let socket_path = runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let session_id = "resume-response-loss-session".to_owned();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };
        let accept_shared = shared.clone();
        let accept_session_id = session_id.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("test connection should accept");
                let connection_shared = accept_shared.clone();
                let expected_session_id = accept_session_id.clone();
                tokio::spawn(async move {
                    let _ = handle_supervisor_connection(
                        connection_shared,
                        expected_session_id,
                        stream,
                    )
                    .await;
                });
            }
        });

        let mut client =
            SupervisorPtySession::connect(&session_id, socket_path, std::process::id(), None)
                .expect("initial controller should connect");
        client.attach_device("dev-a").unwrap();
        let original = client.current_controller_id().expect("controller identity");
        let mut lose_first_commit_response = true;

        client
            .reconnect_ipc_with_validation(|point| {
                if point == ResumeValidationPoint::CommitResponse && lose_first_commit_response {
                    lose_first_commit_response = false;
                    Err(PtyError::Backend(
                        "injected commit response loss".to_owned(),
                    ))
                } else {
                    Ok(())
                }
            })
            .expect("same resume transaction should recover after response loss");

        let resumed = client.current_controller_id().expect("resumed identity");
        assert_eq!(resumed.id, original.id);
        assert!(client.has_attached_device("dev-a").unwrap());
        client
            .write_all(b"controller-usable-after-response-loss")
            .unwrap();
        let state = shared.state.lock().await;
        assert_eq!(state.controllers.len(), 1);
        assert_eq!(state.active_controller_id, Some(resumed.id));
        assert_eq!(
            state.active_controller_connection_id,
            Some(resumed.connection_id)
        );
        assert_eq!(state.committed_resume_transactions.len(), 1);
        drop(state);

        accept_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reader_start_failure_replays_committed_resume_transaction() {
        let runtime_dir = SupervisorTestDir::new("resume-reader-start-failure");
        let socket_path = runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let session_id = "resume-reader-start-failure-session".to_owned();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };
        let accept_shared = shared.clone();
        let accept_session_id = session_id.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("test connection should accept");
                let connection_shared = accept_shared.clone();
                let expected_session_id = accept_session_id.clone();
                tokio::spawn(async move {
                    let _ = handle_supervisor_connection(
                        connection_shared,
                        expected_session_id,
                        stream,
                    )
                    .await;
                });
            }
        });

        let mut client =
            SupervisorPtySession::connect(&session_id, socket_path, std::process::id(), None)
                .expect("initial controller should connect");
        client.attach_device("dev-a").unwrap();
        let original = client.current_controller_id().expect("controller identity");
        let mut fail_reader_start_once = true;
        client
            .reconnect_ipc_with_validation(|point| {
                if point == ResumeValidationPoint::ReaderStart && fail_reader_start_once {
                    fail_reader_start_once = false;
                    Err(PtyError::Backend(
                        "injected reader start failure".to_owned(),
                    ))
                } else {
                    Ok(())
                }
            })
            .expect("committed resume should replay after reader start failure");
        assert!(
            !fail_reader_start_once,
            "reader start failure must be injected"
        );

        let resumed = client.current_controller_id().expect("resumed identity");
        assert_eq!(resumed.id, original.id);
        assert!(client.has_attached_device("dev-a").unwrap());
        client.write_all(b"reader-start-recovered").unwrap();
        let state = shared.state.lock().await;
        assert_eq!(state.controllers.len(), 1);
        assert_eq!(state.active_controller_id, Some(resumed.id));
        assert_eq!(
            state.active_controller_connection_id,
            Some(resumed.connection_id)
        );
        assert_eq!(state.committed_resume_transactions.len(), 1);
        assert!(state.has_attached_device("dev-a"));
        drop(state);
        accept_task.abort();
    }

    #[tokio::test]
    async fn lost_close_response_is_queryable_and_duplicate_close_does_not_reterminate() {
        let session_id = "close-response-loss-session".to_owned();
        let terminate_count = Arc::new(AtomicU64::new(0));
        let wait_count = Arc::new(AtomicU64::new(0));
        let cleanup_capability = [41_u8; CLEANUP_CAPABILITY_BYTES];
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(CloseCountingPtySession {
                terminate_count: Arc::clone(&terminate_count),
                wait_count: Arc::clone(&wait_count),
            }))),
            state: Arc::new(Mutex::new(SupervisorState::with_cleanup_capability(
                PtySize::new(24, 80),
                cleanup_capability,
            ))),
            shutdown_tx,
        };

        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let first_shared = shared.clone();
        let first_session_id = session_id.clone();
        let first_task = tokio::spawn(async move {
            handle_supervisor_connection(first_shared, first_session_id, server_stream).await
        });
        let (mut first_reader, mut first_writer) = client_stream.into_split();
        let initial = test_supervisor_request(
            &mut first_reader,
            &mut first_writer,
            1,
            SupervisorRequest::AttachSync {
                session_id: session_id.clone(),
                last_terminal_seq: None,
                resume_controller_id: None,
            },
        )
        .await
        .unwrap()
        .into_attach_sync()
        .unwrap();
        assert_ne!(initial.controller_id, 0);
        let operation_id = 41_u64;
        write_frame_async(
            &mut first_writer,
            &SupervisorRequestEnvelope {
                request_id: 2,
                request: SupervisorRequest::CloseIdempotent { operation_id },
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if shared.state.lock().await.confirmed_close_operation_id == Some(operation_id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close operation should be recorded before response recovery");
        drop(first_reader);
        drop(first_writer);
        let _ = first_task.await.unwrap();

        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let second_shared = shared.clone();
        let second_session_id = session_id.clone();
        let second_task = tokio::spawn(async move {
            handle_supervisor_connection(second_shared, second_session_id, server_stream).await
        });
        let (mut second_reader, mut second_writer) = client_stream.into_split();
        test_cleanup_authenticate(
            &mut second_reader,
            &mut second_writer,
            3,
            &session_id,
            &cleanup_capability,
        )
        .await
        .unwrap();
        assert!(
            test_supervisor_request(
                &mut second_reader,
                &mut second_writer,
                5,
                SupervisorRequest::CloseStatus { operation_id },
            )
            .await
            .unwrap()
            .into_close_status()
            .unwrap()
        );
        assert!(
            test_supervisor_request(
                &mut second_reader,
                &mut second_writer,
                6,
                SupervisorRequest::CleanupClose {
                    session_id,
                    operation_id,
                },
            )
            .await
            .unwrap()
            .into_close_status()
            .unwrap()
        );
        assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
        assert_eq!(wait_count.load(Ordering::Relaxed), 1);

        second_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn naked_cleanup_connection_cannot_terminate_with_an_active_controller() {
        let session_id = "cleanup-capability-session".to_owned();
        let terminate_count = Arc::new(AtomicU64::new(0));
        let wait_count = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(CloseCountingPtySession {
                terminate_count: Arc::clone(&terminate_count),
                wait_count: Arc::clone(&wait_count),
            }))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };

        let (controller_server, controller_client) = UnixStream::pair().unwrap();
        let controller_shared = shared.clone();
        let controller_session_id = session_id.clone();
        let controller_task = tokio::spawn(async move {
            handle_supervisor_connection(
                controller_shared,
                controller_session_id,
                controller_server,
            )
            .await
        });
        let (mut controller_reader, mut controller_writer) = controller_client.into_split();
        test_supervisor_request(
            &mut controller_reader,
            &mut controller_writer,
            1,
            SupervisorRequest::AttachSync {
                session_id: session_id.clone(),
                last_terminal_seq: None,
                resume_controller_id: None,
            },
        )
        .await
        .unwrap()
        .into_attach_sync()
        .unwrap();

        let (cleanup_server, cleanup_client) = UnixStream::pair().unwrap();
        let cleanup_shared = shared.clone();
        let cleanup_session_id = session_id.clone();
        let cleanup_task = tokio::spawn(async move {
            handle_supervisor_connection(cleanup_shared, cleanup_session_id, cleanup_server).await
        });
        let (mut cleanup_reader, mut cleanup_writer) = cleanup_client.into_split();
        assert!(
            test_supervisor_request(
                &mut cleanup_reader,
                &mut cleanup_writer,
                2,
                SupervisorRequest::CleanupClose {
                    session_id,
                    operation_id: 91,
                },
            )
            .await
            .is_err()
        );
        assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
        assert_eq!(wait_count.load(Ordering::Relaxed), 0);

        controller_task.abort();
        cleanup_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_cleanup_capability_cannot_terminate_or_claim_operation_id() {
        let session_id = "wrong-cleanup-capability-session".to_owned();
        let terminate_count = Arc::new(AtomicU64::new(0));
        let wait_count = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let capability = [7_u8; CLEANUP_CAPABILITY_BYTES];
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(CloseCountingPtySession {
                terminate_count: Arc::clone(&terminate_count),
                wait_count: Arc::clone(&wait_count),
            }))),
            state: Arc::new(Mutex::new(SupervisorState::with_cleanup_capability(
                PtySize::new(24, 80),
                capability,
            ))),
            shutdown_tx,
        };

        let (attacker_server, attacker_client) = UnixStream::pair().unwrap();
        let attacker_shared = shared.clone();
        let attacker_session_id = session_id.clone();
        let attacker_task = tokio::spawn(async move {
            handle_supervisor_connection(attacker_shared, attacker_session_id, attacker_server)
                .await
        });
        let (mut attacker_reader, mut attacker_writer) = attacker_client.into_split();
        assert!(
            test_cleanup_authenticate(
                &mut attacker_reader,
                &mut attacker_writer,
                1,
                &session_id,
                &[8_u8; CLEANUP_CAPABILITY_BYTES],
            )
            .await
            .is_err()
        );
        assert!(
            test_supervisor_request(
                &mut attacker_reader,
                &mut attacker_writer,
                3,
                SupervisorRequest::CleanupClose {
                    session_id: session_id.clone(),
                    operation_id: 92,
                },
            )
            .await
            .is_err()
        );
        assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
        assert_eq!(wait_count.load(Ordering::Relaxed), 0);
        assert_eq!(shared.state.lock().await.confirmed_close_operation_id, None);

        let (cleanup_server, cleanup_client) = UnixStream::pair().unwrap();
        let cleanup_shared = shared.clone();
        let cleanup_session_id = session_id.clone();
        let cleanup_task = tokio::spawn(async move {
            handle_supervisor_connection(cleanup_shared, cleanup_session_id, cleanup_server).await
        });
        let (mut cleanup_reader, mut cleanup_writer) = cleanup_client.into_split();
        test_cleanup_authenticate(
            &mut cleanup_reader,
            &mut cleanup_writer,
            4,
            &session_id,
            &capability,
        )
        .await
        .unwrap();
        assert!(
            test_supervisor_request(
                &mut cleanup_reader,
                &mut cleanup_writer,
                6,
                SupervisorRequest::CleanupClose {
                    session_id,
                    operation_id: 93,
                },
            )
            .await
            .unwrap()
            .into_close_status()
            .unwrap()
        );
        assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
        assert_eq!(wait_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            shared.state.lock().await.confirmed_close_operation_id,
            Some(93)
        );

        attacker_task.abort();
        cleanup_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_cleanup_connection_cannot_expand_into_controller_mode() {
        let session_id = "restricted-cleanup-mode-session".to_owned();
        let capability = [31_u8; CLEANUP_CAPABILITY_BYTES];
        let terminate_count = Arc::new(AtomicU64::new(0));
        let wait_count = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(CloseCountingPtySession {
                terminate_count: Arc::clone(&terminate_count),
                wait_count: Arc::clone(&wait_count),
            }))),
            state: Arc::new(Mutex::new(SupervisorState::with_cleanup_capability(
                PtySize::new(24, 80),
                capability,
            ))),
            shutdown_tx,
        };
        let (server, client) = UnixStream::pair().unwrap();
        let server_shared = shared.clone();
        let server_session_id = session_id.clone();
        let task = tokio::spawn(async move {
            handle_supervisor_connection(server_shared, server_session_id, server).await
        });
        let (mut reader, mut writer) = client.into_split();
        test_cleanup_authenticate(&mut reader, &mut writer, 1, &session_id, &capability)
            .await
            .unwrap();

        let forbidden = [
            SupervisorRequest::Attach {
                session_id: session_id.clone(),
            },
            SupervisorRequest::AttachSync {
                session_id: session_id.clone(),
                last_terminal_seq: None,
                resume_controller_id: None,
            },
            SupervisorRequest::CommitControllerResume { transaction_id: 11 },
            SupervisorRequest::ControllerResumeStatus { transaction_id: 11 },
            SupervisorRequest::ResetAttachedDevices,
            SupervisorRequest::AttachDevice {
                device_id: "forbidden-device".to_owned(),
            },
            SupervisorRequest::DetachDevice {
                device_id: "forbidden-device".to_owned(),
            },
            SupervisorRequest::DeviceAttached {
                device_id: "forbidden-device".to_owned(),
            },
            SupervisorRequest::Input {
                data_base64: general_purpose::STANDARD.encode(b"forbidden"),
            },
            SupervisorRequest::Resize {
                size: PtySize::new(30, 100),
            },
            SupervisorRequest::Snapshot,
            SupervisorRequest::TerminalSnapshot {
                last_terminal_seq: None,
            },
            SupervisorRequest::CloseIdempotent { operation_id: 401 },
            SupervisorRequest::InstallCleanupCapability {
                session_id: session_id.clone(),
                capability_base64: general_purpose::STANDARD.encode([32_u8; 32]),
                migration_operation_id: None,
            },
            SupervisorRequest::NaturalExitStatus,
            SupervisorRequest::FinalizeNaturalExit,
        ];
        for (index, request) in forbidden.into_iter().enumerate() {
            assert!(
                test_supervisor_request(&mut reader, &mut writer, 3 + index as u64, request,)
                    .await
                    .is_err()
            );
        }
        let state = shared.state.lock().await;
        assert_eq!(state.active_controller_id, None);
        assert!(state.controllers.is_empty());
        drop(state);
        assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
        assert_eq!(wait_count.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(target_os = "linux")]
    async fn legacy_cleanup_install_requires_active_controller_without_side_effects() {
        let session_id = "legacy-install-peer-identity-session".to_owned();
        let terminate_count = Arc::new(AtomicU64::new(0));
        let wait_count = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(CloseCountingPtySession {
                terminate_count: Arc::clone(&terminate_count),
                wait_count: Arc::clone(&wait_count),
            }))),
            state: Arc::new(Mutex::new(
                SupervisorState::with_optional_cleanup_capability(PtySize::new(24, 80), None),
            )),
            shutdown_tx,
        };
        let (server, client) = UnixStream::pair().unwrap();
        let server_shared = shared.clone();
        let server_session_id = session_id.clone();
        let task = tokio::spawn(async move {
            handle_supervisor_connection(server_shared, server_session_id, server).await
        });
        let (mut reader, mut writer) = client.into_split();
        assert!(
            test_supervisor_request(
                &mut reader,
                &mut writer,
                1,
                SupervisorRequest::InstallCleanupCapability {
                    session_id: session_id.clone(),
                    capability_base64: general_purpose::STANDARD.encode([55_u8; 32]),
                    migration_operation_id: Some(55),
                },
            )
            .await
            .is_err()
        );
        assert_eq!(shared.state.lock().await.cleanup_capability, None);
        assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
        assert_eq!(wait_count.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[test]
    #[cfg(all(unix, not(target_os = "linux")))]
    fn unix_pid_fallback_confirms_only_esrch() {
        assert!(!supervisor_pid_confirmed_dead(std::process::id()).unwrap());
        assert!(supervisor_pid_confirmed_dead(i32::MAX as u32).unwrap());
    }

    #[test]
    fn supervisor_request_response_error_is_not_retryable_transport_failure() {
        let error = SupervisorRequestCompletion::Response(SupervisorResponse::err(
            "session supervisor connection is not the active controller",
        ))
        .into_result()
        .expect_err("business rejection should not masquerade as transport retry");

        assert!(
            matches!(error, SupervisorRequestFailure::Response(_)),
            "stale controller rejection must stay in response lane so request() can stop reconnecting"
        );
    }

    #[test]
    fn supervisor_request_transport_error_stays_retryable() {
        let error = SupervisorRequestCompletion::TransportError(PtyError::Backend(
            "session supervisor request timed out".to_owned(),
        ))
        .into_result()
        .expect_err("transport breakage should stay retryable");

        assert!(
            matches!(error, SupervisorRequestFailure::Transport(_)),
            "timeout/disconnect failures should remain in transport lane for reconnect retry"
        );
    }

    #[tokio::test]
    async fn supervisor_output_drain_yields_after_budget_instead_of_reading_unbounded_backlog() {
        let session_id = "budget-session".to_owned();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(BurstPtySession {
                remaining_chunks: SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK + 1,
            }))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };

        let has_more = drain_supervisor_output(&shared).await;

        assert!(
            has_more,
            "drain 应在预算耗尽时返回，让 supervisor 请求处理获得调度机会"
        );
        let state = shared.state.lock().await;
        assert_eq!(
            state.terminal.journal_len(),
            1,
            "同一 drain tick 的小 read 应聚合，但不能越过调度预算"
        );
    }

    #[tokio::test]
    async fn supervisor_output_burst_does_not_overflow_healthy_terminal_attach() {
        const BURST_CHUNKS: usize = 900;
        let session_id = "burst-session".to_owned();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let mut state = SupervisorState::new(PtySize::new(24, 80));
        let (attach_tx, mut attach_rx) = test_bounded_output_channel();
        let attach_id = state.register_terminal_attach(attach_tx);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id),
            session: Arc::new(Mutex::new(Box::new(BurstPtySession {
                remaining_chunks: BURST_CHUNKS,
            }))),
            state: Arc::new(Mutex::new(state)),
            shutdown_tx,
        };

        drain_supervisor_output_until_idle(&shared).await;

        assert!(
            shared
                .state
                .lock()
                .await
                .terminal_attaches
                .contains_key(&attach_id),
            "producer-side small reads must not be mistaken for a slow terminal consumer"
        );
        let mut frame_count = 0_usize;
        let mut output_bytes = 0_usize;
        while let Ok(frame) = attach_rx.try_recv() {
            if let SupervisorTerminalServerFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output { data, .. },
                ..
            } = frame
            {
                frame_count = frame_count.saturating_add(1);
                output_bytes = output_bytes.saturating_add(data.len());
            }
        }
        assert_eq!(output_bytes, BURST_CHUNKS * 4);
        assert_eq!(
            frame_count,
            BURST_CHUNKS.div_ceil(SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK),
            "each scheduled drain tick should publish one bounded aggregate frame"
        );
    }

    #[tokio::test]
    async fn supervisor_ipc_prioritizes_control_response_while_output_is_backlogged() {
        let session_id = "priority-session".to_owned();
        let (server_stream, client_stream) =
            UnixStream::pair().expect("test unix stream pair should open");
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };
        let shared_for_connection = shared.clone();
        let expected_session_id = session_id.clone();
        let connection_task = tokio::spawn(async move {
            handle_supervisor_connection(shared_for_connection, expected_session_id, server_stream)
                .await
        });
        let (mut client_reader, mut client_writer) = client_stream.into_split();

        write_frame_async(
            &mut client_writer,
            &SupervisorRequestEnvelope {
                request_id: 1,
                request: SupervisorRequest::AttachSync {
                    session_id: session_id.clone(),
                    last_terminal_seq: None,
                    resume_controller_id: None,
                },
            },
        )
        .await
        .expect("attach request should write");
        match read_frame_async::<SupervisorFrame>(&mut client_reader)
            .await
            .expect("attach response should read")
        {
            SupervisorFrame::Response {
                request_id,
                response,
            } => {
                assert_eq!(request_id, 1);
                response
                    .into_result()
                    .expect("attach response should be ok")
                    .into_attach_sync()
                    .expect("attach response should carry sync payload");
            }
            other => panic!("expected attach response, got {other:?}"),
        }

        let controller = shared
            .state
            .lock()
            .await
            .controllers
            .values()
            .next()
            .cloned()
            .expect("attach should install controller");
        for seq in 1..=128_u64 {
            controller
                .tx
                .try_send(
                    SupervisorFrame::TerminalFrame {
                        frame: PtyTerminalFrame::Output {
                            terminal_seq: seq,
                            data: vec![b'x'; 4096],
                        },
                    },
                    4096,
                )
                .expect("live output should queue");
        }

        write_frame_async(
            &mut client_writer,
            &SupervisorRequestEnvelope {
                request_id: 2,
                request: SupervisorRequest::Ping,
            },
        )
        .await
        .expect("ping request should write");

        let mut output_frames_before_ping_response = 0_usize;
        loop {
            let frame = tokio::time::timeout(
                Duration::from_secs(2),
                read_frame_async::<SupervisorFrame>(&mut client_reader),
            )
            .await
            .expect("ping response should not be stuck behind all output")
            .expect("supervisor frame should read");
            match frame {
                SupervisorFrame::Response {
                    request_id: 2,
                    response,
                } => {
                    response
                        .into_result()
                        .expect("ping response should be ok")
                        .expect_empty()
                        .expect("ping response should be empty");
                    break;
                }
                SupervisorFrame::TerminalFrame { .. } => {
                    output_frames_before_ping_response += 1;
                    assert!(
                        output_frames_before_ping_response < 128,
                        "control response must not wait for the entire live-output backlog"
                    );
                }
                other => panic!("unexpected frame before ping response: {other:?}"),
            }
        }

        connection_task.abort();
    }

    #[tokio::test]
    async fn repeated_attach_on_one_control_socket_is_rejected_without_duplicate_controller() {
        let session_id = "single-attach-session".to_owned();
        let (server_stream, client_stream) =
            UnixStream::pair().expect("test unix stream pair should open");
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(SupervisorState::new(PtySize::new(24, 80)))),
            shutdown_tx,
        };
        let shared_for_connection = shared.clone();
        let expected_session_id = session_id.clone();
        let connection_task = tokio::spawn(async move {
            handle_supervisor_connection(shared_for_connection, expected_session_id, server_stream)
                .await
        });
        let (mut client_reader, mut client_writer) = client_stream.into_split();

        for request_id in 1..=2 {
            write_frame_async(
                &mut client_writer,
                &SupervisorRequestEnvelope {
                    request_id,
                    request: SupervisorRequest::AttachSync {
                        session_id: session_id.clone(),
                        last_terminal_seq: None,
                        resume_controller_id: None,
                    },
                },
            )
            .await
            .expect("attach request should write");
            let SupervisorFrame::Response {
                request_id: response_id,
                response,
            } = read_frame_async::<SupervisorFrame>(&mut client_reader)
                .await
                .expect("attach response should read")
            else {
                panic!("expected attach response");
            };
            assert_eq!(response_id, request_id);
            if request_id == 1 {
                response.into_result().expect("first attach should succeed");
            } else {
                let error = response
                    .into_result()
                    .expect_err("second attach on the same socket must fail");
                assert!(error.to_string().contains("already attached"));
            }
        }

        assert_eq!(shared.state.lock().await.controllers.len(), 1);
        {
            let mut state = shared.state.lock().await;
            let frame = state.record_output(b"once");
            state.broadcast_terminal_frame(&session_id, frame);
        }
        let live = tokio::time::timeout(
            Duration::from_secs(1),
            read_frame_async::<SupervisorFrame>(&mut client_reader),
        )
        .await
        .expect("retained controller should receive live output")
        .expect("live output frame should decode");
        assert!(matches!(
            live,
            SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 1,
                    ref data,
                },
            } if data == b"once"
        ));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                read_frame_async::<SupervisorFrame>(&mut client_reader),
            )
            .await
            .is_err(),
            "rejected duplicate attach must not create a second broadcast recipient"
        );
        connection_task.abort();
    }

    #[tokio::test]
    async fn slow_control_peer_holds_shared_byte_reservation_until_write_finishes() {
        let session_id = "slow-control-session".to_owned();
        let (server_stream, client_stream) =
            UnixStream::pair().expect("test unix stream pair should open");
        let mut state = SupervisorState::new(PtySize::new(1000, 5000));
        let output = vec![b'x'; 4 * 1024 * 1024];
        for chunk in output.chunks(64 * 1024) {
            state.record_output(chunk);
        }
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session_id: Arc::new(session_id.clone()),
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(state)),
            shutdown_tx,
        };
        let shared_for_connection = shared.clone();
        let expected_session_id = session_id.clone();
        let connection_task = tokio::spawn(async move {
            handle_supervisor_connection(shared_for_connection, expected_session_id, server_stream)
                .await
        });
        let (mut client_reader, mut client_writer) = client_stream.into_split();
        write_frame_async(
            &mut client_writer,
            &SupervisorRequestEnvelope {
                request_id: 1,
                request: SupervisorRequest::AttachSync {
                    session_id,
                    last_terminal_seq: None,
                    resume_controller_id: None,
                },
            },
        )
        .await
        .expect("attach request should write");
        let _: SupervisorFrame = read_frame_async(&mut client_reader)
            .await
            .expect("attach response should read");

        for request_id in 2..=3 {
            write_frame_async(
                &mut client_writer,
                &SupervisorRequestEnvelope {
                    request_id,
                    request: SupervisorRequest::Snapshot,
                },
            )
            .await
            .expect("snapshot request should write");
        }

        let result = tokio::time::timeout(Duration::from_secs(2), connection_task)
            .await
            .expect("shared control budget should disconnect the slow peer")
            .expect("connection task should join");
        let error = result.expect_err("slow peer should exceed the shared control budget");
        assert!(error.to_string().contains("output queue"));
        assert!(shared.state.lock().await.controllers.is_empty());
    }

    #[test]
    fn attach_sync_falls_back_to_snapshot_when_requested_seq_is_outside_journal() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        for index in 0..=TERMINAL_JOURNAL_MAX_EVENTS {
            state.record_output(format!("line-{index}\n").as_bytes());
        }
        let (controller_tx, _controller_rx) = test_bounded_output_channel();

        let (_controller_id, sync) = state.attach_sync(controller_tx, Some(42), Some(0));

        assert_eq!(sync.base_seq, (TERMINAL_JOURNAL_MAX_EVENTS + 1) as u64);
        assert!(matches!(
            sync.frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq, .. }]
                if *base_seq == (TERMINAL_JOURNAL_MAX_EVENTS + 1) as u64
        ));
    }

    #[test]
    fn attach_sync_drops_queued_live_frames_already_covered_by_base_seq() {
        let (controller_tx, mut controller_rx) = test_bounded_output_channel();
        controller_tx
            .try_send(
                SupervisorFrame::TerminalFrame {
                    frame: PtyTerminalFrame::Output {
                        terminal_seq: 1,
                        data: b"old".to_vec(),
                    },
                },
                3,
            )
            .expect("test queue should accept old frame");
        controller_tx
            .try_send(
                SupervisorFrame::TerminalFrame {
                    frame: PtyTerminalFrame::Output {
                        terminal_seq: 3,
                        data: b"new".to_vec(),
                    },
                },
                3,
            )
            .expect("test queue should accept new frame");

        let delayed = drain_controller_frames_after_sync(&mut controller_rx, 2);

        assert!(matches!(
            delayed.as_slice(),
            [SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 3,
                    data,
                },
            }] if data == b"new"
        ));
        assert!(
            controller_rx.try_recv().is_err(),
            "drain should leave no covered frame in the controller queue"
        );
    }

    #[test]
    fn supervisor_terminal_resize_rebases_tail_to_snapshot() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let output = state.record_output(b"before resize");
        let resize = state.resize(PtySize::new(10, 100));

        assert_eq!(output.terminal_seq(), Some(1));
        assert_eq!(resize.terminal_seq(), Some(2));
        match state
            .terminal_snapshot_or_tail_with_base(Some(1))
            .1
            .as_slice()
        {
            [
                PtyTerminalFrame::Snapshot {
                    base_seq,
                    size,
                    data,
                },
            ] => {
                assert_eq!(*base_seq, 2);
                assert_eq!(*size, PtySize::new(10, 100));
                assert!(
                    data.windows(b"before resize".len())
                        .any(|window| window == b"before resize"),
                    "resize 跨越恢复必须返回包含当前屏幕内容的 snapshot"
                );
            }
            other => panic!("resize-crossing tail must return snapshot, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_terminal_exit_consumes_session_seq() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let output = state.record_output(b"before exit");
        let exit = state.record_exit(Some(0));

        assert_eq!(output.terminal_seq(), Some(1));
        assert_eq!(exit.terminal_seq(), Some(2));
        assert_eq!(
            state.terminal_snapshot_or_tail_with_base(Some(1)).1,
            vec![exit]
        );
    }

    #[test]
    fn daemon_terminal_mirror_returns_tail_without_supervisor_roundtrip() {
        let mut mirror = SupervisorTerminalMirror::new(PtySize::new(4, 40));
        mirror.reset_from_snapshot(PtySize::new(4, 40), 1, b"alpha\n");
        mirror.apply_frame(&PtyTerminalFrame::Output {
            terminal_seq: 2,
            data: b"beta\n".to_vec(),
        });

        let (base_seq, tail) = mirror.terminal_snapshot_or_tail(Some(1));

        assert_eq!(base_seq, 2);
        assert_eq!(
            tail,
            vec![PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"beta\n".to_vec(),
            }]
        );
        assert!(
            mirror.terminal_snapshot_or_tail(Some(2)).1.is_empty(),
            "已追平 current_seq 的客户端不应再收到 snapshot 或 tail"
        );
    }

    #[test]
    fn daemon_terminal_mirror_falls_back_to_snapshot_after_sequence_gap() {
        let mut mirror = SupervisorTerminalMirror::new(PtySize::new(4, 40));
        mirror.reset_from_snapshot(PtySize::new(4, 40), 1, b"alpha\n");
        mirror.apply_frame(&PtyTerminalFrame::Output {
            terminal_seq: 4,
            data: b"delta\n".to_vec(),
        });

        let (base_seq, frames) = mirror.terminal_snapshot_or_tail(Some(1));

        assert_eq!(base_seq, 4);
        assert!(matches!(
            frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq: 4, data, .. }]
                if String::from_utf8_lossy(data.as_slice()).contains("delta")
        ));
    }

    #[test]
    fn daemon_terminal_mirror_ignores_live_frames_already_covered_by_snapshot() {
        let mut mirror = SupervisorTerminalMirror::new(PtySize::new(4, 40));
        mirror.reset_from_snapshot(PtySize::new(4, 40), 5, b"snapshot\n");

        assert!(
            !mirror.apply_frame(&PtyTerminalFrame::Output {
                terminal_seq: 4,
                data: b"old\n".to_vec(),
            }),
            "旧 live frame 已被 snapshot 覆盖，不能再次污染 daemon mirror"
        );
        assert!(mirror.terminal_snapshot_or_tail(Some(5)).1.is_empty());
    }

    #[test]
    fn daemon_terminal_mirror_does_not_let_late_attach_snapshot_roll_back_live_tail() {
        let mut mirror = SupervisorTerminalMirror::new(PtySize::new(4, 40));
        mirror.reset_from_snapshot(PtySize::new(4, 40), 5, b"snapshot\n");
        mirror.apply_frame(&PtyTerminalFrame::Output {
            terminal_seq: 6,
            data: b"live-after-snapshot\n".to_vec(),
        });

        // 中文注释：模拟 AttachSync response 返回后，reader 已经先收到 seq=6；
        // daemon 再处理旧 snapshot(base_seq=5) 时不能把 mirror 回退。
        mirror.apply_snapshot_and_tail(PtySize::new(4, 40), 5, b"stale-snapshot\n", &[]);

        let (base_seq, frames) = mirror.terminal_snapshot_or_tail(Some(5));
        assert_eq!(base_seq, 6);
        assert_eq!(
            frames,
            vec![PtyTerminalFrame::Output {
                terminal_seq: 6,
                data: b"live-after-snapshot\n".to_vec(),
            }]
        );
    }

    #[test]
    fn supervisor_client_read_rearms_signal_when_pending_output_remains() {
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let mut session = test_supervisor_client_with_queues(
            writer,
            Arc::new(StdMutex::new(VecDeque::from([
                b"first".to_vec(),
                b"second".to_vec(),
            ]))),
            Arc::new(StdMutex::new(VecDeque::new())),
            output_signal_tx,
            output_signal_rx,
        );
        let mut signal = session
            .output_signal()
            .expect("supervisor client should expose output signal");
        signal.borrow_and_update();
        let mut buffer = vec![0_u8; 16];

        let read = session.read(&mut buffer).expect("read should not fail");

        assert_eq!(&buffer[..read], b"first");
        assert!(
            signal.has_changed().unwrap_or(false),
            "remaining supervisor output should rearm daemon watcher"
        );
    }

    #[test]
    fn supervisor_client_terminal_frame_read_does_not_rearm_for_legacy_pending_output() {
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let mut session = test_supervisor_client_with_queues(
            writer,
            Arc::new(StdMutex::new(VecDeque::from([b"legacy".to_vec()]))),
            Arc::new(StdMutex::new(VecDeque::new())),
            output_signal_tx,
            output_signal_rx,
        );
        let mut signal = session
            .output_signal()
            .expect("supervisor client should expose output signal");
        signal.borrow_and_update();

        let frame = session
            .read_terminal_frame()
            .expect("terminal frame read should not fail");

        assert!(frame.is_none());
        assert!(
            !signal.has_changed().unwrap_or(false),
            "legacy raw output must not spin terminal_frame watcher"
        );
    }

    #[test]
    fn supervisor_client_terminal_frame_read_rearms_when_frames_remain() {
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let mut session = test_supervisor_client_with_queues(
            writer,
            Arc::new(StdMutex::new(VecDeque::new())),
            Arc::new(StdMutex::new(VecDeque::from([
                PtyTerminalFrame::Output {
                    terminal_seq: 1,
                    data: b"first".to_vec(),
                },
                PtyTerminalFrame::Output {
                    terminal_seq: 2,
                    data: b"second".to_vec(),
                },
            ]))),
            output_signal_tx,
            output_signal_rx,
        );
        let mut signal = session
            .output_signal()
            .expect("supervisor client should expose output signal");
        signal.borrow_and_update();

        let frame = session
            .read_terminal_frame()
            .expect("terminal frame read should not fail");

        assert_eq!(frame.and_then(|frame| frame.terminal_seq()), Some(1));
        assert!(
            signal.has_changed().unwrap_or(false),
            "remaining terminal frames should rearm daemon watcher"
        );
    }

    #[test]
    fn supervisor_client_terminal_snapshot_uses_daemon_mirror_without_ipc_request() {
        let (client_stream, _peer) =
            StdUnixStream::pair().expect("test unix stream pair should open");
        let writer = client_stream
            .try_clone()
            .expect("test stream should clone writer");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_output = Arc::new(StdMutex::new(VecDeque::new()));
        let pending_terminal_frames = Arc::new(StdMutex::new(VecDeque::from([
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"old".to_vec(),
            },
            PtyTerminalFrame::Output {
                terminal_seq: 9,
                data: b"new".to_vec(),
            },
        ])));
        let terminal_mirror = Arc::new(StdMutex::new(SupervisorTerminalMirror::new(PtySize::new(
            24, 80,
        ))));
        {
            let mut mirror = terminal_mirror
                .lock()
                .expect("terminal mirror mutex should not be poisoned");
            mirror.reset_from_snapshot(PtySize::new(24, 80), 7, b"snapshot\n");
            mirror.apply_frame(&PtyTerminalFrame::Output {
                terminal_seq: 8,
                data: b"tail\n".to_vec(),
            });
        }
        let mut session = SupervisorPtySession {
            session_id: "test-session".to_owned(),
            restore_info: test_restore_info(),
            supervisor_child: Arc::new(StdMutex::new(None)),
            writer: StdMutex::new(writer),
            pending_requests,
            pending_output,
            pending_terminal_frames,
            terminal_mirror,
            bootstrap_terminal_frames: Arc::new(StdMutex::new(None)),
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
            close_operation_id: StdMutex::new(None),
            cleanup_capability: None,
            exited: Arc::new(AtomicBool::new(false)),
        };

        let frames = session
            .terminal_snapshot(Some(7))
            .expect("terminal snapshot should be served from daemon mirror");

        assert_eq!(
            frames,
            vec![PtyTerminalFrame::Output {
                terminal_seq: 8,
                data: b"tail\n".to_vec(),
            }]
        );
    }

    #[test]
    fn supervisor_client_seed_attach_sync_drops_old_pending_snapshot_before_legacy_read() {
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let pending_terminal_frames = Arc::new(StdMutex::new(VecDeque::from([
            PtyTerminalFrame::Snapshot {
                base_seq: 0,
                size: PtySize::new(24, 80),
                data: b"old-screen".to_vec(),
            },
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"old-tail".to_vec(),
            },
        ])));
        let mut session = test_supervisor_client_with_queues(
            writer,
            Arc::new(StdMutex::new(VecDeque::new())),
            Arc::clone(&pending_terminal_frames),
            output_signal_tx,
            output_signal_rx,
        );

        session.seed_attach_sync(SupervisorAttachSyncPayload {
            controller_id: 7,
            controller_connection_id: 8,
            base_seq: 2,
            snapshot: SupervisorSnapshotPayload {
                size: PtySize::new(24, 80),
                process_id: Some(42),
                exited: false,
                retained_output: Vec::new(),
            },
            frames: vec![PtyTerminalFrame::Snapshot {
                base_seq: 2,
                size: PtySize::new(24, 80),
                data: b"new-screen".to_vec(),
            }],
        });

        let mut buffer = vec![0_u8; 64];
        let read = session
            .read(&mut buffer)
            .expect("legacy read should not fail");

        assert_eq!(&buffer[..read], b"new-screen");
        assert!(
            session.read(&mut buffer).expect("legacy read should drain") == 0,
            "old pending snapshot/tail must not be replayed after attach_sync reseed"
        );
    }

    #[test]
    fn parses_session_supervisor_cmdline() {
        let size = PtySize::with_pixels(33, 101, 1200, 900);
        let size_base64 = general_purpose::STANDARD
            .encode(serde_json::to_vec(&size).expect("test size should serialize"));
        let args = vec![
            "/usr/local/bin/termd".to_owned(),
            "__session-supervisor".to_owned(),
            "--session-id".to_owned(),
            "session-a".to_owned(),
            "--socket-path".to_owned(),
            "/var/lib/termd/termd-supervisors/session-a.sock".to_owned(),
            "--size-base64".to_owned(),
            size_base64,
        ];

        let supervisor = parse_supervisor_cmdline(42, &args).expect("supervisor should parse");

        assert_eq!(supervisor.pid, 42);
        assert_eq!(supervisor.session_id, "session-a");
        assert_eq!(
            supervisor.socket_path,
            PathBuf::from("/var/lib/termd/termd-supervisors/session-a.sock")
        );
        assert_eq!(supervisor.size, Some(size));
    }
}

mod base64_bytes {
    use base64::{Engine as _, engine::general_purpose};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        general_purpose::STANDARD
            .decode(value)
            .map_err(serde::de::Error::custom)
    }
}
