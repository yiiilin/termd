//! 每个 session 一个独立 supervisor 的 PTY backend。
//!
//! daemon 主进程不再直接持有真实 PTY；它只通过 Unix socket 和 session supervisor 通信。
//! supervisor 进程继续使用 termd 当前二进制启动，并在自己的进程空间里托管 PTY、
//! 保留最近输出快照，以及在 daemon 重启后接受新的 attach。

mod terminal_journal;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc as tokio_mpsc, oneshot, watch};

use crate::net::pty_bridge::NonBlockingPortablePtyBackend;

use self::terminal_journal::{SupervisorTerminalCache, SupervisorTerminalMirror};
#[cfg(test)]
use self::terminal_journal::{TERMINAL_ATTACH_TAIL_MAX_BYTES, TERMINAL_JOURNAL_MAX_EVENTS};
use super::{
    CommandSpec, PtyAttachment, PtyAttachmentBootstrap, PtyBackend, PtyError, PtyRestoreInfo,
    PtyResult, PtySession, PtySize, PtySnapshot, PtySupervisorStatus, PtyTerminalFrame,
};

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_SOCKET_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_SIGNAL_INIT: u64 = 0;
const RETAINED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES: usize = 64 * 1024;
const SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK: usize = 64;
const SUPERVISOR_OUTPUT_PUMP_MAX_BYTES_PER_TICK: usize = 4 * 1024 * 1024;
const SUPERVISOR_TERMINAL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const SUPERVISOR_TERMINAL_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;

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
    runtime_dir: PathBuf,
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

    fn socket_path_for_session(&self, session_id: &str) -> PathBuf {
        self.runtime_dir
            .join(short_supervisor_socket_file_name(session_id))
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
        let orphan_pids =
            orphaned_supervisor_pids(&self.runtime_dir, &valid_session_ids, &supervisors);

        Ok(orphan_pids.len())
    }

    /// 列出当前 state 目录下仍存活、且命令行足够完整的 session supervisor。
    pub fn live_supervisor_restore_candidates(&self) -> PtyResult<Vec<SupervisorRestoreCandidate>> {
        let supervisors = supervisor_processes_from_proc()?;
        Ok(supervisors
            .into_iter()
            .filter(|supervisor| {
                supervisor.socket_path.parent() == Some(self.runtime_dir.as_path())
            })
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
        fs::create_dir_all(&self.runtime_dir).map_err(PtyError::from)?;
        let socket_path = self.socket_path_for_session(session_id);

        // 清理上一次异常退出留下的 stale socket；如果仍有活 supervisor，后续 connect 会失败并暴露。
        let _ = fs::remove_file(&socket_path);

        let command_base64 = general_purpose::STANDARD
            .encode(serde_json::to_vec(command).map_err(PtyError::backend)?);
        let size_base64 =
            general_purpose::STANDARD.encode(serde_json::to_vec(&size).map_err(PtyError::backend)?);
        let socket_path_arg = socket_path.to_string_lossy().to_string();

        let child = ProcessCommand::new(&self.binary_path)
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
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(PtyError::from)?;
        let supervisor_pid = child.id();

        self.connect_session(session_id, &socket_path, supervisor_pid, Some(child))
    }

    fn connect_session(
        &self,
        session_id: &str,
        socket_path: &Path,
        supervisor_pid: u32,
        child: Option<Child>,
    ) -> PtyResult<Box<dyn PtySession>> {
        let session = SupervisorPtySession::connect(
            session_id,
            socket_path.to_path_buf(),
            supervisor_pid,
            child,
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
                let mut session =
                    self.connect_session(session_id, socket_path, *supervisor_pid, None)?;
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
    supervisor_child: StdMutex<Option<Child>>,
    writer: StdMutex<StdUnixStream>,
    pending_requests: Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
    output_signal_tx: watch::Sender<u64>,
    output_signal_rx: watch::Receiver<u64>,
    next_request_id: AtomicU64,
    controller_identity: StdMutex<Option<ControllerIdentity>>,
    cached_size: StdMutex<PtySize>,
    cached_process_id: StdMutex<Option<u32>>,
}

impl SupervisorPtySession {
    fn connect(
        session_id: &str,
        socket_path: PathBuf,
        supervisor_pid: u32,
        child: Option<Child>,
    ) -> PtyResult<Self> {
        let stream = connect_supervisor_socket(&socket_path)?;
        let writer = stream.try_clone().map_err(PtyError::from)?;
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_output = Arc::new(StdMutex::new(VecDeque::new()));
        let pending_terminal_frames = Arc::new(StdMutex::new(VecDeque::new()));
        let terminal_mirror = Arc::new(StdMutex::new(SupervisorTerminalMirror::new(
            PtySize::default(),
        )));
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        spawn_supervisor_reader_thread(
            session_id,
            stream,
            Arc::clone(&pending_requests),
            Arc::clone(&pending_terminal_frames),
            Arc::clone(&terminal_mirror),
            output_signal_tx.clone(),
        )?;

        let session = Self {
            session_id: session_id.to_owned(),
            restore_info: PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                supervisor_status: PtySupervisorStatus::Running,
            },
            supervisor_child: StdMutex::new(child),
            writer: StdMutex::new(writer),
            pending_requests,
            pending_output,
            pending_terminal_frames,
            terminal_mirror,
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::default()),
            cached_process_id: StdMutex::new(None),
        };

        let sync = session.attach_sync(None)?;
        session.seed_attach_sync(sync);
        // 中文注释：新的 daemon controller 接管 session 时，要把旧 daemon 遗留的
        // attached-device authority 清空；否则 daemon 重启后，已经掉线的设备仍会被
        // supervisor 当作已 attach。
        session
            .request(SupervisorRequest::ResetAttachedDevices)?
            .expect_empty()?;

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
            write_frame_sync(&mut *writer, &envelope)
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

        let stream = connect_supervisor_socket(&socket_path)?;
        let writer = stream.try_clone().map_err(PtyError::from)?;
        *self
            .writer
            .lock()
            .expect("supervisor writer mutex poisoned") = writer;
        spawn_supervisor_reader_thread(
            &self.session_id,
            stream,
            Arc::clone(&self.pending_requests),
            Arc::clone(&self.pending_terminal_frames),
            Arc::clone(&self.terminal_mirror),
            self.output_signal_tx.clone(),
        )?;

        let sync = self.attach_sync_once(
            self.current_controller_id().map(|identity| identity.id),
            None,
        )?;
        self.seed_attach_sync(sync);
        let _ = supervisor_pid;
        Ok(())
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

    fn attach_sync_once(
        &self,
        resume_controller_id: Option<u64>,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<SupervisorAttachSyncPayload> {
        self.request_once(SupervisorRequest::AttachSync {
            session_id: self.session_id.clone(),
            last_terminal_seq,
            resume_controller_id,
        })
        .map_err(SupervisorRequestFailure::into_pty_error)?
        .into_attach_sync()
    }

    fn seed_attach_sync(&self, sync: SupervisorAttachSyncPayload) {
        *self
            .controller_identity
            .lock()
            .expect("controller identity mutex poisoned") = Some(ControllerIdentity {
            id: sync.controller_id,
            connection_id: sync.controller_connection_id,
        });
        let snapshot = sync.snapshot;
        *self.cached_size.lock().expect("cached size mutex poisoned") = snapshot.size;
        *self
            .cached_process_id
            .lock()
            .expect("cached pid mutex poisoned") = snapshot.process_id;
        {
            let mut mirror = self
                .terminal_mirror
                .lock()
                .expect("terminal mirror mutex poisoned");
            mirror.apply_snapshot_and_tail(
                snapshot.size,
                sync.base_seq,
                &snapshot.retained_output,
                &sync.frames,
            );
        }

        self.drop_pending_terminal_frames_through(sync.base_seq);
        if !sync.frames.is_empty() {
            // 中文注释：`read()` 是 termctl/测试仍在使用的 legacy byte-stream 兼容面。
            // 现在 attach 首屏只通过 sequenced frames 传输，因此 daemon 侧也要把
            // attach_sync 的 snapshot/tail 注入本地 terminal frame 队列；否则重连后
            // legacy reader 会看不到首屏输出。但不要恢复 retained_output，Web watched
            // attach 仍以 frames 为唯一权威，避免同一屏内容双播。
            self.pending_terminal_frames
                .lock()
                .expect("pending terminal frames mutex poisoned")
                .extend(sync.frames);
            let next = self.output_signal_tx.borrow().wrapping_add(1);
            let _ = self.output_signal_tx.send(next);
        }
        if !snapshot.retained_output.is_empty() {
            self.pending_output
                .lock()
                .expect("pending output mutex poisoned")
                .push_back(snapshot.retained_output);
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
}

/// daemon watched attachment 对应的 supervisor attach 代理。
///
/// 中文注释：这个代理只搬运 opaque frame，不再把 terminal output/input/heartbeat
/// 解释回 daemon 业务对象。
struct SupervisorAttachProxy {
    writer: StdMutex<StdUnixStream>,
    pending_frames: Arc<StdMutex<VecDeque<Vec<u8>>>>,
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
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        spawn_terminal_attach_reader_thread(
            attachment_id,
            stream,
            Arc::clone(&pending_frames),
            output_signal_tx.clone(),
        )?;
        Ok(Self {
            writer: StdMutex::new(writer),
            pending_frames,
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
        let Ok(child_slot) = self.supervisor_child.get_mut() else {
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
    output_signal_tx: watch::Sender<u64>,
) -> PtyResult<()> {
    let thread_name = format!("termd-supervisor-attach-{attachment_id}");
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            while let Ok(frame) = read_raw_frame_sync(&mut reader) {
                pending_frames
                    .lock()
                    .expect("terminal attach pending frame mutex poisoned")
                    .push_back(frame);
                let next = output_signal_tx.borrow().wrapping_add(1);
                let _ = output_signal_tx.send(next);
            }
        })
        .map(|_| ())
        .map_err(PtyError::backend)
}

fn spawn_supervisor_reader_thread(
    session_id: &str,
    reader: StdUnixStream,
    pending_requests: Arc<StdMutex<HashMap<u64, mpsc::Sender<SupervisorRequestCompletion>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
    output_signal_tx: watch::Sender<u64>,
) -> PtyResult<()> {
    let thread_name = format!("termd-supervisor-ipc-{session_id}");
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            supervisor_reader_loop(
                reader,
                pending_requests,
                pending_terminal_frames,
                terminal_mirror,
                output_signal_tx,
            );
        })
        .map(|_| ())
        .map_err(PtyError::backend)
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
            self.pending_output
                .lock()
                .expect("pending output mutex poisoned")
                .push_front(remaining);
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
        let close_result = self.request(SupervisorRequest::Close)?.expect_empty();
        if let Err(error) = close_result {
            self.restore_info =
                restore_info_with_status(&self.restore_info, PtySupervisorStatus::Running);
            return Err(error);
        }

        // 只有直接 spawn supervisor 的 daemon 持有 Child 句柄；重连 daemon 没有父子关系，
        // 不能 wait 已经被其他父进程收养的 supervisor。
        if let Some(mut child) = self
            .supervisor_child
            .lock()
            .expect("supervisor child mutex poisoned")
            .take()
        {
            child.wait().map_err(PtyError::from)?;
        }
        self.restore_info =
            restore_info_with_status(&self.restore_info, PtySupervisorStatus::Closed);
        Ok(())
    }

    fn try_wait(&mut self) -> PtyResult<Option<super::PtyExitStatus>> {
        Ok(None)
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

impl SupervisorPtySession {
    fn drop_pending_terminal_frames_through(&self, base_seq: u64) {
        // 中文注释：daemon 和 supervisor 的 IPC reader 会持续接收 live frame。新 attach
        // 已经用 snapshot/tail 同步到 base_seq 后，队列中不晚于该序号的旧 frame 必须丢弃；
        // 否则 snapshot 后会再次重放 attach 前的大量历史输出。
        self.pending_terminal_frames
            .lock()
            .expect("pending terminal frames mutex poisoned")
            .retain(|frame| {
                // 中文注释：pending snapshot 是一张旧状态图，没有 terminal_seq。
                // 新 attach_sync 已经给出更权威的 snapshot/tail 后，旧 snapshot 必须丢弃；
                // 否则 legacy read 会把旧首屏和新首屏连续读出，形成重复回显。
                !matches!(frame, PtyTerminalFrame::Snapshot { .. })
                    && frame.terminal_seq().is_some_and(|seq| seq > base_seq)
            });
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
    output_signal_tx: watch::Sender<u64>,
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
                let applied = terminal_mirror
                    .lock()
                    .expect("terminal mirror mutex poisoned")
                    .apply_frame(&frame);
                if applied {
                    pending_terminal_frames
                        .lock()
                        .expect("pending terminal frames mutex poisoned")
                        .push_back(frame);
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

fn supervisor_runtime_dir(state_path: &Path) -> PathBuf {
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
    if preferred.to_string_lossy().len() + 1 + longest_socket_name_len < UNIX_SOCKET_PATH_MAX_BYTES
    {
        preferred
    } else if base_dir.to_string_lossy().len() + 1 + "ts".len() + 1 + longest_socket_name_len
        < UNIX_SOCKET_PATH_MAX_BYTES
    {
        // 中文注释：优先保留一个专用 runtime 子目录，避免把 socket 直接混在 state
        // 父目录根下；但这个短目录本身也要重新做长度预算。
        base_dir.join("ts")
    } else {
        // 中文注释：极长 state 目录下，即使 `ts` 这样的短子目录也可能让 attach socket
        // 超过 `sun_path` 上限。最后退回到 state 父目录本身，至少保证 control/attach
        // socket 都能绑定成功，而不是在 create-session 时迟到地报 `runtime_failed`。
        base_dir
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
    if preferred.to_string_lossy().len() < UNIX_SOCKET_PATH_MAX_BYTES {
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
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_termd").map(PathBuf::from) {
        if path.exists() {
            return path;
        }
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
        {
            if candidate.exists() {
                return candidate;
            }
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

struct SupervisorState {
    next_controller_id: u64,
    next_controller_connection_id: u64,
    active_controller_id: Option<u64>,
    active_controller_connection_id: Option<u64>,
    controller_resume_lease_id: Option<u64>,
    controllers: HashMap<u64, ControllerHandle>,
    next_terminal_attach_id: u64,
    terminal_attaches: HashMap<u64, TerminalAttachHandle>,
    attached_devices: HashSet<String>,
    retained_output: VecDeque<u8>,
    terminal: SupervisorTerminalCache,
}

impl SupervisorState {
    fn new(size: PtySize) -> Self {
        Self {
            next_controller_id: 1,
            next_controller_connection_id: 1,
            active_controller_id: None,
            active_controller_connection_id: None,
            controller_resume_lease_id: None,
            controllers: HashMap::new(),
            next_terminal_attach_id: 1,
            terminal_attaches: HashMap::new(),
            attached_devices: HashSet::new(),
            retained_output: VecDeque::new(),
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
        append_retained_output(&mut self.retained_output, bytes);
        self.terminal.record_output(bytes)
    }

    fn resize(&mut self, size: PtySize) -> PtyTerminalFrame {
        self.terminal.resize(size)
    }

    fn record_exit(&mut self, code: Option<i32>) -> PtyTerminalFrame {
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
        controller_tx: tokio_mpsc::UnboundedSender<SupervisorFrame>,
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
        controller_tx: tokio_mpsc::UnboundedSender<SupervisorFrame>,
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

        // 中文注释：重连中的 active controller 只能恢复自己已有的 lease，不能申请新的
        // owner 身份。这样 stale controller 即便碰上传输层错误，也无法借 reconnect
        // 抢回 authority。
        let connection_id = self.allocate_controller_connection_id();
        self.controllers.insert(
            controller_id,
            ControllerHandle {
                tx: controller_tx,
                connection_id,
            },
        );
        self.active_controller_id = Some(controller_id);
        self.active_controller_connection_id = Some(connection_id);
        self.controller_resume_lease_id = Some(controller_id);
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
        tx: tokio_mpsc::UnboundedSender<SupervisorTerminalServerFrame>,
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
                .send(SupervisorFrame::TerminalFrame {
                    frame: frame.clone(),
                })
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
                .send(SupervisorTerminalServerFrame::TerminalFrame {
                    session_id: session_id.to_owned(),
                    frame: frame.clone(),
                })
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
    tx: tokio_mpsc::UnboundedSender<SupervisorFrame>,
    connection_id: u64,
}

#[derive(Clone)]
struct TerminalAttachHandle {
    tx: tokio_mpsc::UnboundedSender<SupervisorTerminalServerFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControllerIdentity {
    id: u64,
    connection_id: u64,
}

/// supervisor 入口，由主二进制的隐藏子命令调用。
pub async fn run_session_supervisor(args: SessionSupervisorArgs) -> PtyResult<()> {
    let backend = NonBlockingPortablePtyBackend::new();
    let session = backend.spawn(&args.command, args.size)?;
    let session = Arc::new(Mutex::new(session));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let attach_socket_path =
        attach_socket_path_for_control_socket(&args.socket_path, &args.session_id);
    let shared = SupervisorShared {
        session_id: Arc::new(args.session_id.clone()),
        session: Arc::clone(&session),
        state: Arc::new(Mutex::new(SupervisorState::new(args.size))),
        shutdown_tx,
    };
    let mut listener = bind_supervisor_listener(&args.socket_path, true)?;
    let mut attach_listener = bind_supervisor_listener(&attach_socket_path, true)?;

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
            accepted = tokio::time::timeout(SUPERVISOR_SOCKET_REPAIR_INTERVAL, listener.accept()) => {
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
            accepted = tokio::time::timeout(SUPERVISOR_SOCKET_REPAIR_INTERVAL, attach_listener.accept()) => {
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

    let _ = fs::remove_file(&args.socket_path);
    let _ = fs::remove_file(&attach_socket_path);
    Ok(())
}

fn bind_supervisor_listener(socket_path: &Path, remove_existing: bool) -> PtyResult<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).map_err(PtyError::from)?;
    }
    if remove_existing {
        let _ = fs::remove_file(socket_path);
    }
    UnixListener::bind(socket_path).map_err(PtyError::from)
}

fn ensure_supervisor_socket_bound(
    socket_path: &Path,
    listener: &mut UnixListener,
) -> PtyResult<()> {
    match fs::metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
        Ok(_) => {
            return Err(PtyError::Backend(format!(
                "session supervisor socket path exists but is not a socket: {}",
                socket_path.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(PtyError::from(error)),
    }

    // Unix socket 的路径名被外部 unlink 后，已 accept 的连接仍然可用，但新的 daemon
    // 无法再按路径 attach。重新 bind 同一路径可以保留原 PTY，并恢复后续重连入口。
    *listener = bind_supervisor_listener(socket_path, false)?;
    tracing::warn!(
        socket_path = %socket_path.display(),
        "session supervisor socket path was missing; rebound listener"
    );
    Ok(())
}

async fn handle_supervisor_connection(
    shared: SupervisorShared,
    expected_session_id: String,
    stream: UnixStream,
) -> PtyResult<()> {
    let (reader, writer) = stream.into_split();
    let (controller_tx, mut controller_rx) = tokio_mpsc::unbounded_channel::<SupervisorFrame>();
    // 保留一个显式 sender，避免 controller 还未写入 state 或被短暂替换时，
    // outbound 分支把通道误判为关闭而结束 IPC 连接。
    let _controller_tx_keepalive = controller_tx.clone();
    let (request_tx, mut request_rx) =
        tokio_mpsc::unbounded_channel::<io::Result<SupervisorRequestEnvelope>>();
    let (writer_control_tx, writer_control_rx) = tokio_mpsc::unbounded_channel::<SupervisorFrame>();
    let (writer_data_tx, writer_data_rx) = tokio_mpsc::unbounded_channel::<SupervisorFrame>();
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
                let response = match envelope.request {
                    SupervisorRequest::Attach { session_id } => {
                        if session_id != expected_session_id {
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
                        if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let process_id = shared.session.lock().await.process_id();
                            let sync_result = {
                                let mut state = shared.state.lock().await;
                                match resume_controller_id {
                                    Some(resume_controller_id) => state.resume_attach_sync(
                                        resume_controller_id,
                                        controller_tx.clone(),
                                        process_id,
                                        last_terminal_seq,
                                    ),
                                    None => Ok(state.attach_sync(
                                        controller_tx.clone(),
                                        process_id,
                                        last_terminal_seq,
                                    )),
                                }
                            };
                            match sync_result {
                                Ok((identity, sync)) => {
                                    controller_identity = Some(identity);
                                    suppress_live_through_base_seq = Some(sync.base_seq);
                                    SupervisorResponse::ok(SupervisorResponsePayload::AttachSync(sync))
                                }
                                Err(error) => SupervisorResponse::err(error.to_string()),
                            }
                        }
                    }
                    SupervisorRequest::ResetAttachedDevices => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        shared.state.lock().await.attached_devices.clear();
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::AttachDevice { device_id } => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        shared.state.lock().await.attach_device(&device_id);
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
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
                        let bytes = general_purpose::STANDARD
                            .decode(data_base64)
                            .map_err(PtyError::backend)?;
                        shared.session.lock().await.write_all(&bytes)?;
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
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
                        ensure_current_controller(&shared, controller_identity).await?;
                        let process_id = shared.session.lock().await.process_id();
                        let state = shared.state.lock().await;
                        let payload = SupervisorSnapshotPayload {
                            size: state.size(),
                            process_id,
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
                    SupervisorRequest::Ping => {
                        ensure_current_controller(&shared, controller_identity).await?;
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                };

                let frame = SupervisorFrame::Response {
                    request_id: envelope.request_id,
                    response,
                };
                let delayed_live_frames = suppress_live_through_base_seq
                    .map(|base_seq| drain_controller_frames_after_sync(&mut controller_rx, base_seq))
                    .unwrap_or_default();
                if writer_control_tx.send(frame).is_err() {
                    break Err(PtyError::Backend(
                        "session supervisor writer control channel closed".to_owned(),
                    ));
                }
                for frame in delayed_live_frames {
                    if !is_current_controller(&shared, controller_identity).await {
                        break;
                    }
                    if writer_data_tx.send(frame).is_err() {
                        break 'connection Err(PtyError::Backend(
                            "session supervisor writer data channel closed".to_owned(),
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
                if writer_data_tx.send(frame).is_err() {
                    break Err(PtyError::Backend(
                        "session supervisor writer data channel closed".to_owned(),
                    ));
                }
            }
        }
    };

    reader_task.abort();
    writer_task.abort();
    if let Some(identity) = controller_identity {
        let mut state = shared.state.lock().await;
        state.remove_controller(identity);
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
        tokio_mpsc::unbounded_channel::<io::Result<SupervisorTerminalClientFrame>>();
    let reader_task = tokio::spawn(supervisor_terminal_connection_reader(reader, request_tx));
    let (terminal_tx, mut terminal_rx) =
        tokio_mpsc::unbounded_channel::<SupervisorTerminalServerFrame>();
    let process_id = shared.session.lock().await.process_id();
    let (attach_id, attach_sync, base_seq) = {
        let mut state = shared.state.lock().await;
        let attach_id = state.register_terminal_attach(terminal_tx.clone());
        let snapshot = SupervisorSnapshotPayload {
            size: state.size(),
            process_id,
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
    request_tx: tokio_mpsc::UnboundedSender<io::Result<SupervisorRequestEnvelope>>,
) {
    loop {
        match read_frame_async::<SupervisorRequestEnvelope>(&mut reader).await {
            Ok(envelope) => {
                if request_tx.send(Ok(envelope)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                let _ = request_tx.send(Err(error));
                break;
            }
        }
    }
}

async fn supervisor_terminal_connection_reader(
    mut reader: tokio::net::unix::OwnedReadHalf,
    request_tx: tokio_mpsc::UnboundedSender<io::Result<SupervisorTerminalClientFrame>>,
) {
    loop {
        match read_frame_async::<SupervisorTerminalClientFrame>(&mut reader).await {
            Ok(frame) => {
                if request_tx.send(Ok(frame)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                let _ = request_tx.send(Err(error));
                break;
            }
        }
    }
}

async fn supervisor_connection_writer(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut control_rx: tokio_mpsc::UnboundedReceiver<SupervisorFrame>,
    mut data_rx: tokio_mpsc::UnboundedReceiver<SupervisorFrame>,
) -> io::Result<()> {
    let mut control_open = true;
    let mut data_open = true;

    while control_open || data_open {
        tokio::select! {
            biased;

            frame = control_rx.recv(), if control_open => {
                let Some(frame) = frame else {
                    control_open = false;
                    continue;
                };
                write_frame_async(&mut writer, &frame).await?;
            }
            frame = data_rx.recv(), if data_open => {
                let Some(frame) = frame else {
                    data_open = false;
                    continue;
                };
                write_frame_async(&mut writer, &frame).await?;
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
    controller_rx: &mut tokio_mpsc::UnboundedReceiver<SupervisorFrame>,
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
    controller_rx: &mut tokio_mpsc::UnboundedReceiver<SupervisorTerminalServerFrame>,
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
    loop {
        if chunks >= SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK
            || bytes >= SUPERVISOR_OUTPUT_PUMP_MAX_BYTES_PER_TICK
        {
            return true;
        }

        let mut buffer = vec![0_u8; SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES];
        let read = match shared.session.lock().await.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%error, "session supervisor failed to read PTY output");
                return false;
            }
        };
        if read == 0 {
            return false;
        }

        buffer.truncate(read);
        chunks = chunks.saturating_add(1);
        bytes = bytes.saturating_add(read);
        let mut state = shared.state.lock().await;
        let frame = state.record_output(&buffer);
        state.broadcast_terminal_frame(shared.session_id.as_str(), frame);
    }
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

fn append_retained_output(retained_output: &mut VecDeque<u8>, bytes: &[u8]) {
    retained_output.extend(bytes.iter().copied());
    while retained_output.len() > RETAINED_OUTPUT_MAX_BYTES {
        retained_output.pop_front();
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
    ResetAttachedDevices,
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
    Ping,
}

impl SupervisorRequest {
    fn kind_label(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::AttachSync { .. } => "attach_sync",
            Self::ResetAttachedDevices => "reset_attached_devices",
            Self::AttachDevice { .. } => "attach_device",
            Self::DetachDevice { .. } => "detach_device",
            Self::DeviceAttached { .. } => "device_attached",
            Self::Input { .. } => "input",
            Self::Resize { .. } => "resize",
            Self::Snapshot => "snapshot",
            Self::TerminalSnapshot { .. } => "terminal_snapshot",
            Self::Close => "close",
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

impl SupervisorRequestFailure {
    fn into_pty_error(self) -> PtyError {
        match self {
            Self::Response(error) | Self::Transport(error) => error,
        }
    }
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
}

impl SupervisorResponsePayload {
    fn expect_empty(self) -> PtyResult<()> {
        match self {
            Self::Empty => Ok(()),
            Self::Snapshot(_)
            | Self::AttachSync(_)
            | Self::DeviceAttached { .. }
            | Self::TerminalFrames { .. } => Err(PtyError::Backend(
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
            | Self::TerminalFrames { .. } => Err(PtyError::Backend(
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
            | Self::TerminalFrames { .. } => Err(PtyError::Backend(
                "session supervisor returned unexpected attach sync payload".to_owned(),
            )),
        }
    }

    fn into_device_attached(self) -> PtyResult<bool> {
        match self {
            Self::DeviceAttached { attached } => Ok(attached),
            Self::Empty | Self::Snapshot(_) | Self::AttachSync(_) | Self::TerminalFrames { .. } => {
                Err(PtyError::Backend(
                    "session supervisor returned unexpected device attachment payload".to_owned(),
                ))
            }
        }
    }

    #[allow(dead_code)]
    fn into_terminal_frames(self) -> PtyResult<(u64, Vec<PtyTerminalFrame>)> {
        match self {
            Self::TerminalFrames { base_seq, frames } => Ok((base_seq, frames)),
            Self::Empty | Self::Snapshot(_) | Self::AttachSync(_) | Self::DeviceAttached { .. } => {
                Err(PtyError::Backend(
                    "session supervisor returned unexpected terminal frames payload".to_owned(),
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SupervisorSnapshotPayload {
    pub(crate) size: PtySize,
    pub(crate) process_id: Option<u32>,
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
    let length = u32::from_le_bytes(length) as usize;
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(invalid_data)
}

fn write_frame_sync<T>(writer: &mut StdUnixStream, value: &T) -> io::Result<()>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(invalid_data)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| invalid_data("session supervisor frame too large"))?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

async fn read_frame_async<T>(reader: &mut tokio::net::unix::OwnedReadHalf) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
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
    let payload = serde_json::to_vec(value).map_err(invalid_data)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| invalid_data("session supervisor frame too large"))?;
    writer.write_all(&length.to_le_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

fn read_raw_frame_sync(reader: &mut StdUnixStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let payload_len = u32::from_le_bytes(length) as usize;
    let mut payload = vec![0_u8; payload_len];
    reader.read_exact(&mut payload)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length);
    frame.extend_from_slice(&payload);
    Ok(frame)
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

#[cfg(test)]
pub(crate) fn encode_supervisor_terminal_client_frame(
    frame: &SupervisorTerminalClientFrame,
) -> PtyResult<Vec<u8>> {
    encode_length_prefixed_json(frame).map_err(PtyError::from)
}

#[cfg(test)]
pub(crate) fn decode_supervisor_terminal_client_frame(
    bytes: &[u8],
) -> PtyResult<SupervisorTerminalClientFrame> {
    decode_length_prefixed_json(bytes).map_err(PtyError::from)
}

#[cfg(test)]
pub(crate) fn encode_supervisor_terminal_server_frame(
    frame: &SupervisorTerminalServerFrame,
) -> PtyResult<Vec<u8>> {
    encode_length_prefixed_json(frame).map_err(PtyError::from)
}

#[cfg(test)]
pub(crate) fn decode_supervisor_terminal_server_frame(
    bytes: &[u8],
) -> PtyResult<SupervisorTerminalServerFrame> {
    decode_length_prefixed_json(bytes).map_err(PtyError::from)
}

#[cfg(test)]
fn encode_length_prefixed_json<T>(value: &T) -> io::Result<Vec<u8>>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(invalid_data)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| invalid_data("session supervisor frame too large"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[cfg(test)]
fn decode_length_prefixed_json<T>(bytes: &[u8]) -> io::Result<T>
where
    T: DeserializeOwned,
{
    if bytes.len() < 4 {
        return Err(invalid_data(
            "session supervisor frame is missing length prefix",
        ));
    }
    let mut length = [0_u8; 4];
    length.copy_from_slice(&bytes[..4]);
    let payload_len = u32::from_le_bytes(length) as usize;
    if bytes.len() != 4 + payload_len {
        return Err(invalid_data(
            "session supervisor frame length does not match payload",
        ));
    }
    serde_json::from_slice(&bytes[4..]).map_err(invalid_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::env;
    use std::ffi::OsStr;

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
            supervisor_child: StdMutex::new(None),
            writer: StdMutex::new(writer),
            pending_requests: Arc::new(StdMutex::new(HashMap::new())),
            pending_output,
            pending_terminal_frames,
            terminal_mirror: Arc::new(StdMutex::new(SupervisorTerminalMirror::new(PtySize::new(
                24, 80,
            )))),
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
        }
    }

    #[test]
    fn supervisor_runtime_dir_uses_shared_base_directory_for_relative_state_paths() {
        let current_dir = env::current_dir().expect("current dir should exist");

        let runtime_dir = supervisor_runtime_dir(Path::new("daemon-state.json"));

        assert_eq!(runtime_dir.parent(), Some(current_dir.as_path()));
        // 中文注释：长 worktree 路径下 runtime dir 会降级成 `ts` 以满足 Unix socket
        // 路径长度限制；这个测试只关心相对 state path 仍共享当前目录作为基准。
        assert!(matches!(
            runtime_dir.file_name(),
            Some(name) if name == OsStr::new("termd-supervisors") || name == OsStr::new("ts")
        ));
    }

    #[test]
    fn supervisor_runtime_dir_uses_state_parent_for_absolute_state_paths() {
        let runtime_dir = supervisor_runtime_dir(Path::new("/var/lib/termd/daemon-state.json"));

        assert_eq!(runtime_dir.parent(), Some(Path::new("/var/lib/termd")));
        assert_eq!(runtime_dir, Path::new("/var/lib/termd/termd-supervisors"));
    }

    #[test]
    fn supervisor_socket_file_name_stays_short_under_long_state_directory() {
        let state_path = Path::new(
            "/tmp/termd-server-test-1234567890-1234567890-1234567890-very-long-state-name/daemon-state.json",
        );
        let backend = SupervisorPtyBackend::for_state_path(state_path);
        let socket_path = backend.socket_path_for_session(
            "123e4567-e89b-12d3-a456-426614174000-this-session-name-is-deliberately-long",
        );

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
        let control_socket_path = backend.socket_path_for_session(session_id);
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

        let runtime_dir = supervisor_runtime_dir(&state_path);
        assert_eq!(runtime_dir, base_dir.join("ts"));
    }

    #[test]
    fn supervisor_runtime_dir_falls_back_to_state_parent_when_short_subdir_still_overflows() {
        let attach_name_len = short_supervisor_attach_socket_file_name("probe").len();
        let base_prefix = "/tmp/";
        let base_dir_len = UNIX_SOCKET_PATH_MAX_BYTES - 2 - attach_name_len;
        let filler_len = base_dir_len
            .checked_sub(base_prefix.len())
            .expect("base dir length must fit under /tmp prefix");
        let base_dir = PathBuf::from(format!("{base_prefix}{}", "b".repeat(filler_len)));
        let state_path = base_dir.join("daemon-state.json");

        assert!(
            base_dir.to_string_lossy().len() + 1 + attach_name_len < UNIX_SOCKET_PATH_MAX_BYTES,
            "state parent itself should still fit the attach socket",
        );
        assert!(
            base_dir.to_string_lossy().len() + 1 + "ts".len() + 1 + attach_name_len
                >= UNIX_SOCKET_PATH_MAX_BYTES,
            "short runtime subdir should still overflow so the final fallback is exercised",
        );

        let runtime_dir = supervisor_runtime_dir(&state_path);
        assert_eq!(runtime_dir, base_dir);
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

    #[tokio::test]
    async fn supervisor_listener_rebinds_when_socket_path_is_unlinked() {
        let socket_path = env::temp_dir().join(format!(
            "termd-supervisor-rebind-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        let mut listener =
            bind_supervisor_listener(&socket_path, true).expect("listener should bind");
        assert!(
            socket_path.exists(),
            "initial bind should create the socket path"
        );

        fs::remove_file(&socket_path).expect("test should unlink socket path");
        ensure_supervisor_socket_bound(&socket_path, &mut listener)
            .expect("missing socket path should be rebound");

        let metadata = fs::metadata(&socket_path).expect("socket path should exist after repair");
        assert!(
            metadata.file_type().is_socket(),
            "repair should recreate a unix socket"
        );
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn supervisor_snapshot_preserves_screen_after_raw_retained_output_trim() {
        let mut state = SupervisorState::new(PtySize::new(8, 80));
        state.record_output(b"\x1b[4;1Hcurrent status");

        let mut osc_noise = Vec::with_capacity(RETAINED_OUTPUT_MAX_BYTES + 4096);
        while osc_noise.len() <= RETAINED_OUTPUT_MAX_BYTES + 1024 {
            // OSC 标题更新不会改变终端屏幕，但会冲掉有限的原始输出环形缓存。
            osc_noise.extend_from_slice(b"\x1b]0;ignored-title\x07");
        }
        state.record_output(&osc_noise);

        let raw_retained = state.retained_output.iter().copied().collect::<Vec<_>>();
        assert!(
            !raw_retained
                .windows(b"current status".len())
                .any(|window| window == b"current status"),
            "raw retained output should have been trimmed"
        );
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
        let (controller_tx, mut controller_rx) = tokio_mpsc::unbounded_channel();

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
        let (controller_tx, _controller_rx) = tokio_mpsc::unbounded_channel();

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
        let (controller_tx, _controller_rx) = tokio_mpsc::unbounded_channel();

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
        let (old_tx, mut old_rx) = tokio_mpsc::unbounded_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, mut new_rx) = tokio_mpsc::unbounded_channel();
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
        let (old_tx, _old_rx) = tokio_mpsc::unbounded_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, _new_rx) = tokio_mpsc::unbounded_channel();
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
        let (old_tx, _old_rx) = tokio_mpsc::unbounded_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, _new_rx) = tokio_mpsc::unbounded_channel();
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

        let (retry_tx, _retry_rx) = tokio_mpsc::unbounded_channel();
        let error = state
            .resume_attach_sync(old_id.id, retry_tx, Some(42), Some(0))
            .expect_err("stale controller must not recover after newer owner disconnects");
        assert!(
            error.to_string().contains("not the active controller"),
            "stale controller should stay fenced even after active slot is empty"
        );
    }

    #[tokio::test]
    async fn resume_attach_sync_allows_latest_lease_holder_after_disconnect() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (owner_tx, _owner_rx) = tokio_mpsc::unbounded_channel();
        let (owner_identity, _owner_sync) = state.attach_sync(owner_tx, Some(42), None);

        state.remove_controller(owner_identity);
        let (retry_tx, _retry_rx) = tokio_mpsc::unbounded_channel();
        let (resumed_identity, _resumed_sync) = state
            .resume_attach_sync(owner_identity.id, retry_tx, Some(42), Some(0))
            .expect("latest lease holder should be able to resume after disconnect");

        assert_eq!(resumed_identity.id, owner_identity.id);
        assert_ne!(
            resumed_identity.connection_id, owner_identity.connection_id,
            "resume should mint a fresh connection generation for the same lease holder"
        );
        assert_eq!(state.active_controller_id, Some(owner_identity.id));
        assert_eq!(
            state.active_controller_connection_id,
            Some(resumed_identity.connection_id)
        );
    }

    #[tokio::test]
    async fn resume_attach_sync_fences_previous_connection_generation_of_same_controller() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (owner_tx, _owner_rx) = tokio_mpsc::unbounded_channel();
        let (owner_identity, _owner_sync) = state.attach_sync(owner_tx, Some(42), None);

        state.remove_controller(owner_identity);
        let (retry_tx, _retry_rx) = tokio_mpsc::unbounded_channel();
        let (resumed_identity, _resumed_sync) = state
            .resume_attach_sync(owner_identity.id, retry_tx, Some(42), Some(0))
            .expect("lease holder resume should succeed");
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
            SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK,
            "单次 drain 不应把无界 backlog 一次读空"
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
                .send(SupervisorFrame::TerminalFrame {
                    frame: PtyTerminalFrame::Output {
                        terminal_seq: seq,
                        data: vec![b'x'; 4096],
                    },
                })
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
                    request_id,
                    response,
                } if request_id == 2 => {
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

    #[test]
    fn attach_sync_falls_back_to_snapshot_when_requested_seq_is_outside_journal() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        for index in 0..=TERMINAL_JOURNAL_MAX_EVENTS {
            state.record_output(format!("line-{index}\n").as_bytes());
        }
        let (controller_tx, _controller_rx) = tokio_mpsc::unbounded_channel();

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
        let (controller_tx, mut controller_rx) = tokio_mpsc::unbounded_channel();
        controller_tx
            .send(SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 1,
                    data: b"old".to_vec(),
                },
            })
            .expect("test queue should accept old frame");
        controller_tx
            .send(SupervisorFrame::TerminalFrame {
                frame: PtyTerminalFrame::Output {
                    terminal_seq: 3,
                    data: b"new".to_vec(),
                },
            })
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
            supervisor_child: StdMutex::new(None),
            writer: StdMutex::new(writer),
            pending_requests,
            pending_output,
            pending_terminal_frames,
            terminal_mirror,
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            controller_identity: StdMutex::new(None),
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
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
    fn supervisor_client_prunes_live_frames_covered_by_snapshot() {
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let pending_terminal_frames = Arc::new(StdMutex::new(VecDeque::from([
            PtyTerminalFrame::Snapshot {
                base_seq: 0,
                size: PtySize::new(24, 80),
                data: b"old-snapshot".to_vec(),
            },
            PtyTerminalFrame::Output {
                terminal_seq: 1,
                data: b"old-1".to_vec(),
            },
            PtyTerminalFrame::Resize {
                terminal_seq: 2,
                size: PtySize::new(30, 100),
            },
            PtyTerminalFrame::Output {
                terminal_seq: 3,
                data: b"new-3".to_vec(),
            },
        ])));
        let session = test_supervisor_client_with_queues(
            writer,
            Arc::new(StdMutex::new(VecDeque::new())),
            Arc::clone(&pending_terminal_frames),
            output_signal_tx,
            output_signal_rx,
        );

        // 中文注释：snapshot(base_seq=2) 已覆盖 seq<=2 的 live frame，后续只能保留真正的新 tail。
        session.drop_pending_terminal_frames_through(2);

        let pending = pending_terminal_frames
            .lock()
            .expect("pending terminal frame mutex should not be poisoned");
        assert_eq!(
            pending
                .iter()
                .map(|frame| match frame {
                    PtyTerminalFrame::Output { terminal_seq, .. } => Some(*terminal_seq),
                    PtyTerminalFrame::Resize { terminal_seq, .. } => Some(*terminal_seq),
                    PtyTerminalFrame::Exit { terminal_seq, .. } => Some(*terminal_seq),
                    PtyTerminalFrame::Snapshot { .. } => None,
                })
                .collect::<Vec<_>>(),
            vec![Some(3)]
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
