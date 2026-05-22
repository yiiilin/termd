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
use tokio::sync::{Mutex, mpsc as tokio_mpsc, oneshot, watch};

use crate::net::pty_bridge::NonBlockingPortablePtyBackend;
use crate::net::screen::TerminalScreen;

use super::{
    CommandSpec, PtyBackend, PtyError, PtyRestoreInfo, PtyResult, PtySession, PtySize, PtySnapshot,
    PtySupervisorStatus, PtyTerminalFrame,
};

const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_SOCKET_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_SIGNAL_INIT: u64 = 0;
const RETAINED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const TERMINAL_JOURNAL_MAX_EVENTS: usize = 8192;
const TERMINAL_ATTACH_TAIL_MAX_BYTES: usize = 128 * 1024;
const TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO: usize = 2;
const SUPERVISOR_OUTPUT_PUMP_CHUNK_BYTES: usize = 16 * 1024;
const SUPERVISOR_OUTPUT_PUMP_MAX_CHUNKS_PER_TICK: usize = 32;
const SUPERVISOR_OUTPUT_PUMP_MAX_BYTES_PER_TICK: usize = 512 * 1024;

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
    session_id: String,
    restore_info: PtyRestoreInfo,
    supervisor_child: StdMutex<Option<Child>>,
    writer: StdMutex<StdUnixStream>,
    pending_requests:
        Arc<StdMutex<HashMap<u64, mpsc::Sender<PtyResult<SupervisorResponsePayload>>>>>,
    pending_output: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    pending_terminal_frames: Arc<StdMutex<VecDeque<PtyTerminalFrame>>>,
    terminal_mirror: Arc<StdMutex<SupervisorTerminalMirror>>,
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
            cached_size: StdMutex::new(PtySize::default()),
            cached_process_id: StdMutex::new(None),
        };

        let sync = session.attach_sync(None)?;
        session.seed_attach_sync(sync);

        Ok(session)
    }

    fn request(&self, request: SupervisorRequest) -> PtyResult<SupervisorResponsePayload> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self.request_once(request.clone()) {
                Ok(payload) => return Ok(payload),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        // supervisor 进程仍存活但 Unix IPC 连接可能因为 daemon 侧读写竞态、
                        // 旧连接 EOF 或热升级残留而断开；运行中请求失败时重连同一个 socket
                        // 再做有限重试，避免把活 session 误报为 runtime_failed。
                        if self.reconnect_ipc().is_ok() {
                            continue;
                        }
                    }
                }
            }
            break;
        }
        Err(last_error
            .unwrap_or_else(|| PtyError::Backend("session supervisor request failed".to_owned())))
    }

    fn request_once(&self, request: SupervisorRequest) -> PtyResult<SupervisorResponsePayload> {
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

    fn reconnect_ipc(&self) -> PtyResult<()> {
        let (socket_path, supervisor_pid, supervisor_status) = match &self.restore_info {
            PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                supervisor_status,
            } => (socket_path.clone(), *supervisor_pid, *supervisor_status),
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

        let sync = self.attach_sync_once(None)?;
        self.seed_attach_sync(sync);
        let _ = supervisor_pid;
        Ok(())
    }

    fn attach_sync(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<SupervisorAttachSyncPayload> {
        self.request(SupervisorRequest::AttachSync {
            session_id: self.session_id.clone(),
            last_terminal_seq,
        })?
        .into_attach_sync()
    }

    fn attach_sync_once(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<SupervisorAttachSyncPayload> {
        self.request_once(SupervisorRequest::AttachSync {
            session_id: self.session_id.clone(),
            last_terminal_seq,
        })?
        .into_attach_sync()
    }

    fn seed_attach_sync(&self, sync: SupervisorAttachSyncPayload) {
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

        if !snapshot.retained_output.is_empty() {
            self.pending_output
                .lock()
                .expect("pending output mutex poisoned")
                .push_back(snapshot.retained_output);
            let next = self.output_signal_tx.borrow().wrapping_add(1);
            let _ = self.output_signal_tx.send(next);
        }
        self.drop_pending_terminal_frames_through(sync.base_seq);
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

fn spawn_supervisor_reader_thread(
    session_id: &str,
    reader: StdUnixStream,
    pending_requests: Arc<
        StdMutex<HashMap<u64, mpsc::Sender<PtyResult<SupervisorResponsePayload>>>>,
    >,
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
            .retain(|frame| frame.terminal_seq().is_none_or(|seq| seq > base_seq));
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
                    let _ = sender.send(response.into_result());
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
    terminal: SupervisorTerminalCache,
}

/// supervisor 侧的权威终端缓存。
///
/// 中文注释：daemon 可以重启、浏览器可以断线，但这个结构跟真实 PTY 在同一个
/// session supervisor 进程里，因此它是恢复 screen snapshot 和 tail 的权威来源。
struct SupervisorTerminalCache {
    // 中文注释：session 级终端事件序号，output/resize/exit 共用；从 1 开始递增。
    next_terminal_seq: u64,
    // 中文注释：当前 journal 窗口中第一条事件的 terminal_seq；用于判断客户端 tail 是否过旧。
    journal_base_seq: u64,
    // 中文注释：最近原始终端事件，用于 snapshot 之后补 tail；snapshot 本身不进入 journal。
    journal: VecDeque<TerminalEvent>,
    // 中文注释：权威终端模拟状态，内部保留最近 1000 行热历史。
    screen: TerminalScreen,
    size: PtySize,
}

/// supervisor journal 中的原始终端事件。
///
/// 中文注释：这里保存 session 级 `terminal_seq`，不是 WebSocket packet seq。
/// snapshot 只是一张状态图，不进入 journal；tail 只由这些事件构成。
#[derive(Clone, Debug, Eq, PartialEq)]
enum TerminalEvent {
    Output { seq: u64, bytes: Vec<u8> },
    Resize { seq: u64, size: PtySize },
    Exit { seq: u64, code: Option<i32> },
}

impl TerminalEvent {
    fn terminal_seq(&self) -> u64 {
        match self {
            Self::Output { seq, .. } | Self::Resize { seq, .. } | Self::Exit { seq, .. } => *seq,
        }
    }

    fn to_terminal_frame(&self) -> PtyTerminalFrame {
        match self {
            Self::Output { seq, bytes } => PtyTerminalFrame::Output {
                terminal_seq: *seq,
                data: bytes.clone(),
            },
            Self::Resize { seq, size } => PtyTerminalFrame::Resize {
                terminal_seq: *seq,
                size: *size,
            },
            Self::Exit { seq, code } => PtyTerminalFrame::Exit {
                terminal_seq: *seq,
                code: *code,
            },
        }
    }

    fn replay_cost_bytes(&self) -> usize {
        match self {
            Self::Output { bytes, .. } => bytes.len(),
            Self::Resize { .. } | Self::Exit { .. } => 1,
        }
    }
}

impl SupervisorTerminalCache {
    fn new(size: PtySize) -> Self {
        Self {
            next_terminal_seq: 1,
            journal_base_seq: 1,
            journal: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
            size,
        }
    }

    fn record_output(&mut self, bytes: &[u8]) -> PtyTerminalFrame {
        // supervisor 是 PTY 真正存活的一侧；屏幕快照必须在这里维护，不能只放在 daemon 内存中。
        self.screen.apply(bytes);
        let event = TerminalEvent::Output {
            seq: self.allocate_terminal_seq(),
            bytes: bytes.to_vec(),
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    fn resize(&mut self, size: PtySize) -> PtyTerminalFrame {
        self.size = size;
        self.screen.resize(size.rows, size.cols);
        let event = TerminalEvent::Resize {
            seq: self.allocate_terminal_seq(),
            size,
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    fn record_exit(&mut self, code: Option<i32>) -> PtyTerminalFrame {
        let event = TerminalEvent::Exit {
            seq: self.allocate_terminal_seq(),
            code,
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    fn snapshot_output(&self) -> Vec<u8> {
        self.screen.snapshot_bytes()
    }

    fn terminal_snapshot_or_tail(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        let current_seq = self.current_terminal_seq();
        if let Some(last_terminal_seq) = last_terminal_seq {
            if last_terminal_seq == current_seq {
                return (current_seq, Vec::new());
            }
            if last_terminal_seq < current_seq
                && last_terminal_seq.saturating_add(1) >= self.journal_base_seq
            {
                let tail_events = self
                    .journal
                    .iter()
                    .filter(|event| event.terminal_seq() > last_terminal_seq)
                    .collect::<Vec<_>>();
                if self.should_replay_attach_tail(&tail_events) {
                    let frames = tail_events
                        .into_iter()
                        .map(TerminalEvent::to_terminal_frame)
                        .collect();
                    return (current_seq, frames);
                }
            }
        }

        (
            current_seq,
            vec![PtyTerminalFrame::Snapshot {
                base_seq: current_seq,
                size: self.size,
                data: self.snapshot_output(),
            }],
        )
    }

    fn should_replay_attach_tail(&self, tail_events: &[&TerminalEvent]) -> bool {
        let tail_bytes = tail_events
            .iter()
            .map(|event| event.replay_cost_bytes())
            .sum::<usize>();
        if tail_bytes <= TERMINAL_ATTACH_TAIL_MAX_BYTES {
            return true;
        }

        // 中文注释：客户端 last_terminal_seq 很旧但仍落在 journal 内时，逐事件 tail
        // 可能比当前 screen snapshot 大很多。此时返回权威 snapshot 更符合 attach 语义，
        // 也避免几千个小 output frame 在 WebSocket/E2EE 层膨胀成数百 KB 的单次发送。
        let snapshot_bytes = self.snapshot_output().len();
        tail_bytes <= snapshot_bytes.saturating_mul(TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO)
    }

    fn current_terminal_seq(&self) -> u64 {
        self.next_terminal_seq.saturating_sub(1)
    }

    fn allocate_terminal_seq(&mut self) -> u64 {
        let seq = self.next_terminal_seq;
        self.next_terminal_seq = self.next_terminal_seq.saturating_add(1).max(seq + 1);
        seq
    }
}

/// daemon 侧的 supervisor 终端镜像缓存。
///
/// 中文注释：它不是权威状态源，只是 supervisor 权威状态的 read replica。supervisor
/// IPC 重连时用 `AttachSync` 的 snapshot/base_seq 重置；live frame 到达 daemon 后必须先
/// 喂给这个 mirror，再进入 pending 队列和协议层 room fanout。
struct SupervisorTerminalMirror {
    current_terminal_seq: u64,
    journal_base_seq: u64,
    journal: VecDeque<TerminalEvent>,
    screen: TerminalScreen,
    size: PtySize,
}

impl SupervisorTerminalMirror {
    fn new(size: PtySize) -> Self {
        Self {
            current_terminal_seq: 0,
            journal_base_seq: 1,
            journal: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
            size,
        }
    }

    fn reset_from_snapshot(&mut self, size: PtySize, base_seq: u64, bytes: &[u8]) {
        if base_seq < self.current_terminal_seq {
            return;
        }
        self.current_terminal_seq = base_seq;
        self.journal_base_seq = base_seq.saturating_add(1);
        self.journal.clear();
        self.size = size;
        self.screen = TerminalScreen::new(size.rows, size.cols);
        self.screen.apply(bytes);
    }

    fn apply_snapshot_and_tail(
        &mut self,
        size: PtySize,
        base_seq: u64,
        bytes: &[u8],
        frames: &[PtyTerminalFrame],
    ) {
        self.reset_from_snapshot(size, base_seq, bytes);
        for frame in frames {
            self.apply_frame(frame);
        }
    }

    fn apply_frame(&mut self, frame: &PtyTerminalFrame) -> bool {
        match frame {
            PtyTerminalFrame::Snapshot {
                base_seq,
                size,
                data,
            } => {
                if *base_seq < self.current_terminal_seq {
                    return false;
                }
                self.reset_from_snapshot(*size, *base_seq, data);
                true
            }
            PtyTerminalFrame::Output { terminal_seq, data } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.screen.apply(data);
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Output {
                    seq: *terminal_seq,
                    bytes: data.clone(),
                });
                true
            }
            PtyTerminalFrame::Resize { terminal_seq, size } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.size = *size;
                self.screen.resize(size.rows, size.cols);
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Resize {
                    seq: *terminal_seq,
                    size: *size,
                });
                true
            }
            PtyTerminalFrame::Exit { terminal_seq, code } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Exit {
                    seq: *terminal_seq,
                    code: *code,
                });
                true
            }
        }
    }

    fn terminal_snapshot_or_tail(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        let current_seq = self.current_terminal_seq;
        if let Some(last_terminal_seq) = last_terminal_seq {
            if last_terminal_seq == current_seq {
                return (current_seq, Vec::new());
            }
            if last_terminal_seq < current_seq
                && last_terminal_seq.saturating_add(1) >= self.journal_base_seq
            {
                let tail_events = self
                    .journal
                    .iter()
                    .filter(|event| event.terminal_seq() > last_terminal_seq)
                    .collect::<Vec<_>>();
                if self.should_replay_attach_tail(&tail_events) {
                    return (
                        current_seq,
                        tail_events
                            .into_iter()
                            .map(TerminalEvent::to_terminal_frame)
                            .collect(),
                    );
                }
            }
        }

        (
            current_seq,
            vec![PtyTerminalFrame::Snapshot {
                base_seq: current_seq,
                size: self.size,
                data: self.screen.snapshot_bytes(),
            }],
        )
    }

    fn push_journal(&mut self, event: TerminalEvent) {
        self.journal.push_back(event);
        while self.journal.len() > TERMINAL_JOURNAL_MAX_EVENTS {
            self.journal.pop_front();
        }
        self.journal_base_seq = self
            .journal
            .front()
            .map(TerminalEvent::terminal_seq)
            .unwrap_or_else(|| self.current_terminal_seq.saturating_add(1));
    }

    fn should_replay_attach_tail(&self, tail_events: &[&TerminalEvent]) -> bool {
        let tail_bytes = tail_events
            .iter()
            .map(|event| event.replay_cost_bytes())
            .sum::<usize>();
        if tail_bytes <= TERMINAL_ATTACH_TAIL_MAX_BYTES {
            return true;
        }

        let snapshot_bytes = self.screen.snapshot_bytes().len();
        tail_bytes <= snapshot_bytes.saturating_mul(TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO)
    }
}

impl SupervisorState {
    fn new(size: PtySize) -> Self {
        Self {
            next_controller_id: 1,
            controller: None,
            retained_output: VecDeque::new(),
            terminal: SupervisorTerminalCache::new(size),
        }
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
        self.terminal.size
    }

    fn attach_sync(
        &mut self,
        controller_tx: tokio_mpsc::UnboundedSender<SupervisorFrame>,
        process_id: Option<u32>,
        last_terminal_seq: Option<u64>,
    ) -> (u64, SupervisorAttachSyncPayload) {
        let id = self.next_controller_id;
        self.next_controller_id = self.next_controller_id.saturating_add(1);
        self.controller = Some(ControllerHandle {
            id,
            tx: controller_tx,
        });
        let snapshot = SupervisorSnapshotPayload {
            size: self.size(),
            process_id,
            retained_output: self.snapshot_output(),
        };
        let (base_seq, frames) = self.terminal_snapshot_or_tail_with_base(last_terminal_seq);
        (
            id,
            SupervisorAttachSyncPayload {
                snapshot,
                base_seq,
                frames,
            },
        )
    }
}

impl SupervisorTerminalCache {
    fn push_journal(&mut self, event: TerminalEvent) {
        self.journal.push_back(event);
        while self.journal.len() > TERMINAL_JOURNAL_MAX_EVENTS {
            self.journal.pop_front();
        }
        self.journal_base_seq = self
            .journal
            .front()
            .map(TerminalEvent::terminal_seq)
            .unwrap_or(self.next_terminal_seq);
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
    tokio::spawn(supervisor_exit_watcher(shared.clone()));

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
    let mut controller_id = None;

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
                                let (id, sync) =
                                    state.attach_sync(controller_tx.clone(), process_id, None);
                                controller_id = Some(id);
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
                    } => {
                        if session_id != expected_session_id {
                            SupervisorResponse::err("session id mismatch")
                        } else {
                            let process_id = shared.session.lock().await.process_id();
                            let sync = {
                                let mut state = shared.state.lock().await;
                                let (id, sync) = state.attach_sync(
                                    controller_tx.clone(),
                                    process_id,
                                    last_terminal_seq,
                                );
                                controller_id = Some(id);
                                sync
                            };
                            suppress_live_through_base_seq = Some(sync.base_seq);
                            SupervisorResponse::ok(SupervisorResponsePayload::AttachSync(sync))
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
                        let (frame, controller) = {
                            let mut state = shared.state.lock().await;
                            let frame = state.resize(size);
                            (frame, state.controller.clone())
                        };
                        if let Some(controller) = controller {
                            let _ = controller.tx.send(SupervisorFrame::TerminalFrame { frame });
                        }
                        SupervisorResponse::ok(SupervisorResponsePayload::Empty)
                    }
                    SupervisorRequest::Snapshot => {
                        ensure_current_controller(&shared, controller_id).await?;
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
                        ensure_current_controller(&shared, controller_id).await?;
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
                let delayed_live_frames = suppress_live_through_base_seq
                    .map(|base_seq| drain_controller_frames_after_sync(&mut controller_rx, base_seq))
                    .unwrap_or_default();
                if writer_control_tx.send(frame).is_err() {
                    break Err(PtyError::Backend(
                        "session supervisor writer control channel closed".to_owned(),
                    ));
                }
                for frame in delayed_live_frames {
                    if !is_current_controller(&shared, controller_id).await {
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
                if !is_current_controller(&shared, controller_id).await {
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
    if let Some(id) = controller_id {
        let mut state = shared.state.lock().await;
        if state.controller.as_ref().map(|controller| controller.id) == Some(id) {
            state.controller = None;
        }
    }

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

async fn is_current_controller(shared: &SupervisorShared, controller_id: Option<u64>) -> bool {
    let Some(controller_id) = controller_id else {
        return false;
    };
    let state = shared.state.lock().await;
    state.controller.as_ref().map(|controller| controller.id) == Some(controller_id)
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
        if let Some(controller) = state.controller.clone() {
            if controller
                .tx
                .send(SupervisorFrame::TerminalFrame { frame })
                .is_err()
            {
                state.controller = None;
            }
        }
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
        if let Some(controller) = state.controller.clone() {
            let _ = controller.tx.send(SupervisorFrame::TerminalFrame { frame });
        }
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
    TerminalFrames {
        base_seq: u64,
        frames: Vec<PtyTerminalFrame>,
    },
}

impl SupervisorResponsePayload {
    fn expect_empty(self) -> PtyResult<()> {
        match self {
            Self::Empty => Ok(()),
            Self::Snapshot(_) | Self::AttachSync(_) | Self::TerminalFrames { .. } => {
                Err(PtyError::Backend(
                    "session supervisor returned unexpected snapshot payload".to_owned(),
                ))
            }
        }
    }

    fn into_snapshot(self) -> PtyResult<SupervisorSnapshotPayload> {
        match self {
            Self::Snapshot(payload) => Ok(payload),
            Self::Empty | Self::AttachSync(_) | Self::TerminalFrames { .. } => Err(
                PtyError::Backend("session supervisor returned empty payload".to_owned()),
            ),
        }
    }

    fn into_attach_sync(self) -> PtyResult<SupervisorAttachSyncPayload> {
        match self {
            Self::AttachSync(payload) => Ok(payload),
            Self::Empty | Self::Snapshot(_) | Self::TerminalFrames { .. } => {
                Err(PtyError::Backend(
                    "session supervisor returned unexpected attach sync payload".to_owned(),
                ))
            }
        }
    }

    #[allow(dead_code)]
    fn into_terminal_frames(self) -> PtyResult<(u64, Vec<PtyTerminalFrame>)> {
        match self {
            Self::TerminalFrames { base_seq, frames } => Ok((base_seq, frames)),
            Self::Empty | Self::Snapshot(_) | Self::AttachSync(_) => Err(PtyError::Backend(
                "session supervisor returned unexpected terminal frames payload".to_owned(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisorAttachSyncPayload {
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

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
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
            cached_size: StdMutex::new(PtySize::new(24, 80)),
            cached_process_id: StdMutex::new(Some(42)),
        }
    }

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
        assert!(matches!(
            sync.frames.as_slice(),
            [PtyTerminalFrame::Snapshot { base_seq: 1, .. }]
        ));

        let frame = state.record_output(b"after\n");
        if let Some(controller) = state.controller.clone() {
            controller
                .tx
                .send(SupervisorFrame::TerminalFrame { frame })
                .expect("new controller should receive live output");
        }
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
        assert_eq!(
            sync.frames,
            vec![PtyTerminalFrame::Output {
                terminal_seq: 2,
                data: b"beta\n".to_vec(),
            }]
        );
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
    fn attach_sync_replaces_old_controller_and_invalidates_old_controller_id() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (old_tx, mut old_rx) = tokio_mpsc::unbounded_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, mut new_rx) = tokio_mpsc::unbounded_channel();
        let (new_id, _new_sync) = state.attach_sync(new_tx, Some(42), Some(0));

        assert_ne!(old_id, new_id);
        assert_eq!(
            state.controller.as_ref().map(|controller| controller.id),
            Some(new_id)
        );

        let frame = state.record_output(b"new-controller-only\n");
        if let Some(controller) = state.controller.clone() {
            controller
                .tx
                .send(SupervisorFrame::TerminalFrame { frame })
                .expect("new controller should receive live output");
        }

        assert!(
            old_rx.try_recv().is_err(),
            "old controller must not receive live frames after replacement"
        );
        assert!(
            new_rx.try_recv().is_ok(),
            "new controller should receive live frames after replacement"
        );
    }

    #[tokio::test]
    async fn attach_sync_rejects_requests_from_replaced_controller_id() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let (old_tx, _old_rx) = tokio_mpsc::unbounded_channel();
        let (old_id, _old_sync) = state.attach_sync(old_tx, Some(42), None);
        let (new_tx, _new_rx) = tokio_mpsc::unbounded_channel();
        let (new_id, _new_sync) = state.attach_sync(new_tx, Some(42), Some(0));
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
            session: Arc::new(Mutex::new(Box::new(NoopPtySession))),
            state: Arc::new(Mutex::new(state)),
            shutdown_tx,
        };

        assert!(
            ensure_current_controller(&shared, Some(old_id))
                .await
                .is_err(),
            "old controller id must not be allowed to input or resize after replacement"
        );
        ensure_current_controller(&shared, Some(new_id))
            .await
            .expect("new controller id should remain valid");
    }

    #[tokio::test]
    async fn supervisor_output_drain_yields_after_budget_instead_of_reading_unbounded_backlog() {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let shared = SupervisorShared {
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
            state.terminal.journal.len(),
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
            .controller
            .clone()
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
    fn supervisor_terminal_resize_consumes_session_seq() {
        let mut state = SupervisorState::new(PtySize::new(4, 40));
        let output = state.record_output(b"before resize");
        let resize = state.resize(PtySize::new(10, 100));

        assert_eq!(output.terminal_seq(), Some(1));
        assert_eq!(resize.terminal_seq(), Some(2));
        assert_eq!(
            state.terminal_snapshot_or_tail_with_base(Some(1)).1,
            vec![resize]
        );
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
                .map(PtyTerminalFrame::terminal_seq)
                .collect::<Vec<_>>(),
            vec![Some(3)]
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
