//! 每个 session 一个独立 supervisor 的 PTY backend。
//!
//! daemon 主进程不再直接持有真实 PTY；它只通过 Unix socket 和 session supervisor 通信。
//! supervisor 进程继续使用 termd 当前二进制启动，并在自己的进程空间里托管 PTY、
//! 保留最近输出快照，以及在 daemon 重启后接受新的 attach。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc as tokio_mpsc, watch};

use crate::net::pty_bridge::NonBlockingPortablePtyBackend;
use crate::net::screen::TerminalScreen;

use super::{
    CommandSpec, PtyBackend, PtyError, PtyRestoreInfo, PtyResult, PtySession, PtySize, PtySnapshot,
    PtySupervisorStatus,
};

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_SOCKET_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_SIGNAL_INIT: u64 = 0;
const RETAINED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

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
        self.runtime_dir.join(format!("{session_id}.sock"))
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
        }
    }
}

/// daemon 侧持有的 supervisor IPC 客户端。
struct SupervisorPtySession {
    restore_info: PtyRestoreInfo,
    supervisor_child: StdMutex<Option<Child>>,
    writer: StdMutex<StdUnixStream>,
    pending_requests:
        Arc<StdMutex<HashMap<u64, mpsc::Sender<PtyResult<SupervisorResponsePayload>>>>>,
    pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    output_signal_tx: watch::Sender<u64>,
    output_signal_rx: watch::Receiver<u64>,
    next_request_id: AtomicU64,
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
        let deadline = Instant::now() + SOCKET_CONNECT_TIMEOUT;
        let stream = loop {
            match StdUnixStream::connect(&socket_path) {
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
        let reader = stream.try_clone().map_err(PtyError::from)?;
        let writer = stream.try_clone().map_err(PtyError::from)?;
        let pending_requests = Arc::new(StdMutex::new(HashMap::new()));
        let pending_output = Arc::new(StdMutex::new(VecDeque::new()));
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let pending_requests_for_thread = Arc::clone(&pending_requests);
        let pending_output_for_thread = Arc::clone(&pending_output);
        let output_signal_tx_for_thread = output_signal_tx.clone();

        thread::Builder::new()
            .name(format!("termd-supervisor-ipc-{session_id}"))
            .spawn(move || {
                supervisor_reader_loop(
                    reader,
                    pending_requests_for_thread,
                    pending_output_for_thread,
                    output_signal_tx_for_thread,
                );
            })
            .map_err(PtyError::backend)?;

        let session = Self {
            restore_info: PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                supervisor_status: PtySupervisorStatus::Running,
            },
            supervisor_child: StdMutex::new(child),
            writer: StdMutex::new(writer),
            pending_requests,
            pending_output,
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            cached_size: StdMutex::new(PtySize::default()),
            cached_process_id: StdMutex::new(None),
        };

        let attach = session.request(SupervisorRequest::Attach {
            session_id: session_id.to_owned(),
        })?;
        let snapshot = attach.into_snapshot()?;
        session.seed_snapshot(snapshot);

        Ok(session)
    }

    fn request(&self, request: SupervisorRequest) -> PtyResult<SupervisorResponsePayload> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
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
            return Err(PtyError::from(error));
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending_requests
                    .lock()
                    .expect("supervisor pending request mutex poisoned")
                    .remove(&request_id);
                Err(PtyError::Backend(
                    "session supervisor request timed out".to_owned(),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PtyError::Backend(
                "session supervisor response channel disconnected".to_owned(),
            )),
        }
    }

    fn seed_snapshot(&self, snapshot: SupervisorSnapshotPayload) {
        *self.cached_size.lock().expect("cached size mutex poisoned") = snapshot.size;
        *self
            .cached_process_id
            .lock()
            .expect("cached pid mutex poisoned") = snapshot.process_id;

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

impl PtySession for SupervisorPtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let Some(mut chunk) = self
            .pending_output
            .lock()
            .expect("pending output mutex poisoned")
            .pop_front()
        else {
            return Ok(0);
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
        if !self
            .pending_output
            .lock()
            .expect("pending output mutex poisoned")
            .is_empty()
        {
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
    }
}

fn supervisor_reader_loop(
    mut reader: StdUnixStream,
    pending_requests: Arc<
        StdMutex<HashMap<u64, mpsc::Sender<PtyResult<SupervisorResponsePayload>>>>,
    >,
    pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
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
                    let _ = sender.send(response.into_result());
                }
            }
            SupervisorFrame::Output { data_base64 } => {
                let Ok(bytes) = general_purpose::STANDARD.decode(data_base64) else {
                    fail_all_pending_requests(
                        &pending_requests,
                        "session supervisor returned invalid output base64".to_owned(),
                    );
                    return;
                };
                if !bytes.is_empty() {
                    pending_output
                        .lock()
                        .expect("pending output mutex poisoned")
                        .push_back(bytes);
                    let next = output_signal_tx.borrow().wrapping_add(1);
                    let _ = output_signal_tx.send(next);
                }
            }
        }
    }
}

fn fail_all_pending_requests(
    pending_requests: &Arc<
        StdMutex<HashMap<u64, mpsc::Sender<PtyResult<SupervisorResponsePayload>>>>,
    >,
    message: String,
) {
    let pending = std::mem::take(
        &mut *pending_requests
            .lock()
            .expect("supervisor pending request mutex poisoned"),
    );
    for (_, sender) in pending {
        let _ = sender.send(Err(PtyError::Backend(message.clone())));
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

    base_dir.join("termd-supervisors")
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
    session: Arc<Mutex<Box<dyn PtySession>>>,
    state: Arc<Mutex<SupervisorState>>,
    shutdown_tx: watch::Sender<bool>,
}

struct SupervisorState {
    next_controller_id: u64,
    controller: Option<ControllerHandle>,
    retained_output: VecDeque<u8>,
    screen: TerminalScreen,
    size: PtySize,
}

impl SupervisorState {
    fn new(size: PtySize) -> Self {
        Self {
            next_controller_id: 1,
            controller: None,
            retained_output: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
            size,
        }
    }

    fn record_output(&mut self, bytes: &[u8]) {
        // supervisor 是 PTY 真正存活的一侧；屏幕快照必须在这里维护，不能只放在 daemon 内存中。
        self.screen.apply(bytes);
        append_retained_output(&mut self.retained_output, bytes);
    }

    fn resize(&mut self, size: PtySize) {
        self.size = size;
        self.screen.resize(size.rows, size.cols);
    }

    fn snapshot_output(&self) -> Vec<u8> {
        self.screen.snapshot_bytes()
    }
}

#[derive(Clone)]
struct ControllerHandle {
    id: u64,
    tx: tokio_mpsc::UnboundedSender<SupervisorFrame>,
}

/// supervisor 入口，由主二进制的隐藏子命令调用。
pub async fn run_session_supervisor(args: SessionSupervisorArgs) -> PtyResult<()> {
    let backend = NonBlockingPortablePtyBackend::new();
    let session = backend.spawn(&args.command, args.size)?;
    let session = Arc::new(Mutex::new(session));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let shared = SupervisorShared {
        session: Arc::clone(&session),
        state: Arc::new(Mutex::new(SupervisorState::new(args.size))),
        shutdown_tx,
    };
    let mut listener = bind_supervisor_listener(&args.socket_path, true)?;

    if let Some(signal) = {
        let session = shared.session.lock().await;
        session.output_signal()
    } {
        tokio::spawn(supervisor_output_pump(shared.clone(), signal));
    }

    loop {
        if let Err(error) = ensure_supervisor_socket_bound(&args.socket_path, &mut listener) {
            // supervisor 的首要职责是保住 PTY；socket 修复失败只能降级为告警，
            // 不能让用户正在跑的 shell 因为一个控制面入口文件异常而退出。
            tracing::warn!(%error, socket_path = %args.socket_path.display(), "failed to repair session supervisor socket");
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
        }
    }

    let _ = fs::remove_file(&args.socket_path);
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
    let (mut reader, mut writer) = stream.into_split();
    let (controller_tx, mut controller_rx) = tokio_mpsc::unbounded_channel::<SupervisorFrame>();
    let mut controller_id = None;

    loop {
        tokio::select! {
            outbound = controller_rx.recv() => {
                let Some(frame) = outbound else {
                    break;
                };
                write_frame_async(&mut writer, &frame).await.map_err(PtyError::from)?;
            }
            inbound = read_frame_async::<SupervisorRequestEnvelope>(&mut reader) => {
                let envelope = match inbound {
                    Ok(envelope) => envelope,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(PtyError::from(error)),
                };
                let response = match envelope.request {
                    SupervisorRequest::Attach { session_id } => {
                        if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let process_id = shared.session.lock().await.process_id();
                            let snapshot = {
                                let mut state = shared.state.lock().await;
                                let id = state.next_controller_id;
                                state.next_controller_id = state.next_controller_id.saturating_add(1);
                                state.controller = Some(ControllerHandle {
                                    id,
                                    tx: controller_tx.clone(),
                                });
                                controller_id = Some(id);
                                SupervisorSnapshotPayload {
                                    size: state.size,
                                    process_id,
                                    retained_output: state.snapshot_output(),
                                }
                            };
                            SupervisorResponse::ok(SupervisorResponsePayload::Snapshot(snapshot))
                        }
                    }
                    SupervisorRequest::Input { data_base64 } => {
                        ensure_current_controller(&shared, controller_id).await?;
                        let bytes = general_purpose::STANDARD
                            .decode(data_base64)
                            .map_err(PtyError::backend)?;
                        shared.session.lock().await.write_all(&bytes)?;
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::Resize { size } => {
                        ensure_current_controller(&shared, controller_id).await?;
                        shared.session.lock().await.resize(size)?;
                        shared.state.lock().await.resize(size);
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::Snapshot => {
                        ensure_current_controller(&shared, controller_id).await?;
                        let process_id = shared.session.lock().await.process_id();
                        let state = shared.state.lock().await;
                        let payload = SupervisorSnapshotPayload {
                            size: state.size,
                            process_id,
                            retained_output: state.snapshot_output(),
                        };
                        SupervisorResponse::ok(SupervisorResponsePayload::Snapshot(payload))
                    }
                    SupervisorRequest::Close => {
                        ensure_current_controller(&shared, controller_id).await?;
                        {
                            let mut session = shared.session.lock().await;
                            session.terminate()?;
                            let _ = session.wait();
                        }
                        let _ = shared.shutdown_tx.send(true);
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::Ping => {
                        ensure_current_controller(&shared, controller_id).await?;
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                };

                let frame = SupervisorFrame::Response {
                    request_id: envelope.request_id,
                    response,
                };
                write_frame_async(&mut writer, &frame).await.map_err(PtyError::from)?;
            }
        }
    }

    if let Some(id) = controller_id {
        let mut state = shared.state.lock().await;
        if state.controller.as_ref().map(|controller| controller.id) == Some(id) {
            state.controller = None;
        }
    }

    Ok(())
}

async fn ensure_current_controller(
    shared: &SupervisorShared,
    controller_id: Option<u64>,
) -> PtyResult<()> {
    let Some(controller_id) = controller_id else {
        return Err(PtyError::Backend(
            "session supervisor connection is not attached".to_owned(),
        ));
    };
    let state = shared.state.lock().await;
    if state.controller.as_ref().map(|controller| controller.id) != Some(controller_id) {
        return Err(PtyError::Backend(
            "session supervisor controller was replaced".to_owned(),
        ));
    }
    Ok(())
}

async fn supervisor_output_pump(shared: SupervisorShared, mut output_signal: watch::Receiver<u64>) {
    drain_supervisor_output(&shared).await;

    while output_signal.changed().await.is_ok() {
        drain_supervisor_output(&shared).await;
    }
}

async fn drain_supervisor_output(shared: &SupervisorShared) {
    loop {
        let mut buffer = vec![0_u8; 16 * 1024];
        let read = match shared.session.lock().await.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%error, "session supervisor failed to read PTY output");
                return;
            }
        };
        if read == 0 {
            return;
        }

        buffer.truncate(read);
        let encoded = general_purpose::STANDARD.encode(&buffer);
        let mut state = shared.state.lock().await;
        state.record_output(&buffer);
        if let Some(controller) = state.controller.clone() {
            if controller
                .tx
                .send(SupervisorFrame::Output {
                    data_base64: encoded,
                })
                .is_err()
            {
                state.controller = None;
            }
        }
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorRequest {
    Attach { session_id: String },
    Input { data_base64: String },
    Resize { size: PtySize },
    Snapshot,
    Close,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorFrame {
    Response {
        request_id: u64,
        response: SupervisorResponse,
    },
    Output {
        data_base64: String,
    },
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
}

impl SupervisorResponsePayload {
    fn expect_empty(self) -> PtyResult<()> {
        match self {
            Self::Empty => Ok(()),
            Self::Snapshot(_) => Err(PtyError::Backend(
                "session supervisor returned unexpected snapshot payload".to_owned(),
            )),
        }
    }

    fn into_snapshot(self) -> PtyResult<SupervisorSnapshotPayload> {
        match self {
            Self::Snapshot(payload) => Ok(payload),
            Self::Empty => Err(PtyError::Backend(
                "session supervisor returned empty payload".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisorSnapshotPayload {
    size: PtySize,
    process_id: Option<u32>,
    #[serde(with = "base64_bytes")]
    retained_output: Vec<u8>,
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

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::env;
    use std::ffi::OsStr;

    #[test]
    fn supervisor_runtime_dir_uses_shared_base_directory_for_relative_state_paths() {
        let current_dir = env::current_dir().expect("current dir should exist");

        let runtime_dir = supervisor_runtime_dir(Path::new("daemon-state.json"));

        assert_eq!(runtime_dir.parent(), Some(current_dir.as_path()));
        assert_eq!(
            runtime_dir.file_name(),
            Some(OsStr::new("termd-supervisors"))
        );
    }

    #[test]
    fn supervisor_runtime_dir_uses_state_parent_for_absolute_state_paths() {
        let runtime_dir = supervisor_runtime_dir(Path::new("/var/lib/termd/daemon-state.json"));

        assert_eq!(runtime_dir.parent(), Some(Path::new("/var/lib/termd")));
        assert_eq!(runtime_dir, Path::new("/var/lib/termd/termd-supervisors"));
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
    fn supervisor_client_read_rearms_signal_when_pending_output_remains() {
        let socket_path = PathBuf::from("/tmp/termd-supervisor-test.sock");
        let restore_info = PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid: 42,
            supervisor_status: PtySupervisorStatus::Running,
        };
        let (writer, _peer) = StdUnixStream::pair().expect("test unix stream pair should open");
        let (output_signal_tx, output_signal_rx) = watch::channel(OUTPUT_SIGNAL_INIT);
        let mut session = SupervisorPtySession {
            restore_info,
            supervisor_child: StdMutex::new(None),
            writer: StdMutex::new(writer),
            pending_requests: Arc::new(StdMutex::new(HashMap::new())),
            pending_output: Arc::new(StdMutex::new(VecDeque::from([
                b"first".to_vec(),
                b"second".to_vec(),
            ]))),
            output_signal_tx,
            output_signal_rx,
            next_request_id: AtomicU64::new(1),
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
        };
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
