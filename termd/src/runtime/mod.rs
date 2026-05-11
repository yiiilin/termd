//! termd daemon 内核 runtime glue。
//!
//! runtime 只把 `SessionManager` 的 attach 状态和 `PtyBackend` 的进程句柄接起来，
//! 负责 daemon 本地的持久会话生命周期与 I/O 桥接。认证、配对、E2EE、WebSocket
//! 和 relay 路由都必须留在更外层，避免这里变成协议层或控制权系统。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::pty::{
    CommandSpec, PtyBackend, PtyError, PtyRestoreInfo, PtySession, PtySize, PtySnapshot,
};
use crate::session::{AttachRole, SessionError, SessionManager, SessionState, TerminalSize};
use crate::state::SessionStateRecord;
use termd_proto::{
    SessionId, SessionState as ProtoSessionState, TerminalSize as ProtoTerminalSize,
    UnixTimestampMillis,
};
use uuid::Uuid;

/// runtime 层统一 Result 类型。
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// runtime glue 的错误类型。
///
/// 这里把 session 状态错误压成 runtime 语义，调用方不需要知道错误来自
/// session manager 还是 PTY 句柄；但 PTY 原始错误仍保留在 `Pty` 变体里便于诊断。
#[derive(Debug)]
pub enum RuntimeError {
    SessionAlreadyExists,
    SessionNotFound,
    SessionClosed,
    DeviceNotAttached,
    InvalidSize,
    NotReconnectable,
    Pty(PtyError),
}

impl PartialEq for RuntimeError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::SessionAlreadyExists, Self::SessionAlreadyExists)
            | (Self::SessionNotFound, Self::SessionNotFound)
            | (Self::SessionClosed, Self::SessionClosed)
            | (Self::DeviceNotAttached, Self::DeviceNotAttached)
            | (Self::InvalidSize, Self::InvalidSize)
            | (Self::NotReconnectable, Self::NotReconnectable) => true,
            (Self::Pty(_), Self::Pty(_)) => true,
            _ => false,
        }
    }
}

impl Eq for RuntimeError {}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionAlreadyExists => write!(f, "session already exists"),
            Self::SessionNotFound => write!(f, "session not found"),
            Self::SessionClosed => write!(f, "session is closed"),
            Self::DeviceNotAttached => write!(f, "device is not attached"),
            Self::InvalidSize => write!(f, "terminal size must have non-zero rows and cols"),
            Self::NotReconnectable => write!(f, "session does not contain reconnect metadata"),
            Self::Pty(error) => write!(f, "{error}"),
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Pty(error) => Some(error),
            Self::SessionAlreadyExists
            | Self::SessionNotFound
            | Self::SessionClosed
            | Self::DeviceNotAttached
            | Self::NotReconnectable
            | Self::InvalidSize => None,
        }
    }
}

impl From<SessionError> for RuntimeError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::SessionAlreadyExists => Self::SessionAlreadyExists,
            SessionError::SessionNotFound => Self::SessionNotFound,
            SessionError::SessionClosed => Self::SessionClosed,
            SessionError::DeviceNotAttached => Self::DeviceNotAttached,
            SessionError::InvalidSize => Self::InvalidSize,
        }
    }
}

impl From<PtyError> for RuntimeError {
    fn from(error: PtyError) -> Self {
        Self::Pty(error)
    }
}

struct RuntimeSession {
    pty: Box<dyn PtySession>,
    created_at_ms: UnixTimestampMillis,
    updated_at_ms: UnixTimestampMillis,
}

/// daemon 内核 runtime。
///
/// `SessionRuntime` 接收的 device id 默认已经由 auth 层验证；本类型只执行
/// shared-control 对应的本地 I/O 规则，不判断设备是否配对，也不解析网络消息。
pub struct SessionRuntime<B: PtyBackend> {
    backend: B,
    sessions: SessionManager,
    runtime_sessions: HashMap<String, RuntimeSession>,
    next_session_number: u64,
}

impl<B: PtyBackend> SessionRuntime<B> {
    /// 创建 runtime，并注入具体 PTY backend。
    ///
    /// 测试可以传入 fake backend；生产 daemon 后续接入 `PortablePtyBackend`。
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            sessions: SessionManager::default(),
            runtime_sessions: HashMap::new(),
            next_session_number: 1,
        }
    }

    /// 创建一个持久 runtime session，并启动对应 PTY 进程。
    ///
    /// 这里不会自动 attach 任何设备；后续 attach 的设备都以 operator 身份共享输入。
    pub fn create_session(
        &mut self,
        command: CommandSpec,
        size: TerminalSize,
    ) -> RuntimeResult<String> {
        let session_id = self.allocate_session_id();
        self.create_session_with_id(&session_id, command, size)?;
        Ok(session_id)
    }

    /// 用调用方提供的稳定 session id 创建 runtime session。
    ///
    /// protocol 层用它把 wire session id 直接映射到 supervisor socket 路径，便于 daemon
    /// 重启后按持久状态重连。
    pub fn create_session_with_id(
        &mut self,
        session_id: &str,
        command: CommandSpec,
        size: TerminalSize,
    ) -> RuntimeResult<()> {
        let pty_size = terminal_size_to_pty_size(size)?;
        if self.runtime_sessions.contains_key(session_id) {
            return Err(RuntimeError::SessionAlreadyExists);
        }

        // 先启动 PTY，只有成功后才写入 SessionManager，避免留下没有进程句柄的半成品 session。
        let pty = self.backend.spawn_named(session_id, &command, pty_size)?;
        self.sessions.create_session(session_id.to_owned())?;
        self.sessions.resize(session_id, size)?;
        let now_ms = current_unix_timestamp_millis();
        self.runtime_sessions.insert(
            session_id.to_owned(),
            RuntimeSession {
                pty,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            },
        );

        Ok(())
    }

    /// 用持久化的 supervisor IPC 元数据重新接回存活的 session。
    pub fn reconnect_session(&mut self, record: &SessionStateRecord) -> RuntimeResult<()> {
        if self
            .runtime_sessions
            .contains_key(&record.session_id.0.to_string())
        {
            return Err(RuntimeError::SessionAlreadyExists);
        }
        if record.state == ProtoSessionState::Closed {
            return Err(RuntimeError::SessionClosed);
        }
        let restore_info = record
            .restore_info
            .as_ref()
            .ok_or(RuntimeError::NotReconnectable)?;
        let session_id = record.session_id.0.to_string();
        let pty = self.backend.reconnect(&session_id, restore_info)?;
        let size = proto_size_to_runtime(record.size);

        self.sessions.create_session(session_id.clone())?;
        self.sessions.resize(&session_id, size)?;
        if record.state == ProtoSessionState::Running {
            // SessionManager 没有“直接恢复 Running”接口；用内部恢复 attach/detach 把状态推到 Running。
            let recovery_device = format!("__runtime-recovery-{session_id}");
            let _ = self.sessions.attach(&session_id, recovery_device.clone())?;
            let _ = self.sessions.detach(&session_id, &recovery_device);
        }
        self.runtime_sessions.insert(
            session_id,
            RuntimeSession {
                pty,
                created_at_ms: record.created_at_ms,
                updated_at_ms: record.updated_at_ms,
            },
        );
        Ok(())
    }

    /// 将已认证设备 attach 到 runtime session。
    ///
    /// runtime 不做 auth 判定；它只返回 session/control 状态机分配出的角色。
    pub fn attach(
        &mut self,
        session_id: &str,
        device_id: impl Into<String>,
    ) -> RuntimeResult<AttachRole> {
        self.ensure_open_session(session_id)?;
        Ok(self.sessions.attach(session_id, device_id)?)
    }

    /// shared-control 模式没有夺权概念；旧 control 命令只确认设备已经 attach。
    pub fn steal_control(&mut self, session_id: &str, device_id: &str) -> RuntimeResult<()> {
        self.ensure_open_session(session_id)?;
        Ok(self.sessions.steal_control(session_id, device_id)?)
    }

    /// 任意已 attach 设备的输入都会写入 PTY；未 attach 设备会被拒绝。
    ///
    /// 这是 runtime 的核心 I/O 桥接点。网络层应在 E2EE 解包后调用本方法，
    /// 但这里不识别 WebSocket frame、设备密钥或 relay 信息。
    pub fn write_input(
        &mut self,
        session_id: &str,
        device_id: &str,
        bytes: &[u8],
    ) -> RuntimeResult<()> {
        self.ensure_open_session(session_id)?;

        match self.sessions.role(session_id, device_id)? {
            Some(AttachRole::Operator) => {
                self.runtime_session_mut(session_id)?.pty.write_all(bytes)?;
                Ok(())
            }
            None => Err(RuntimeError::DeviceNotAttached),
        }
    }

    /// 从 PTY 输出读取数据，供后续 WebSocket/terminal fanout 层广播。
    ///
    /// 输出读取不绑定具体 device；多客户端输出分发策略属于网络层。
    pub fn read_output(&mut self, session_id: &str, buffer: &mut [u8]) -> RuntimeResult<usize> {
        self.ensure_open_session(session_id)?;
        Ok(self.runtime_session_mut(session_id)?.pty.read(buffer)?)
    }

    /// 返回 PTY 输出就绪信号。网络层监听该信号后主动推送输出，不需要客户端轮询。
    pub fn output_signal(&self, session_id: &str) -> RuntimeResult<Option<watch::Receiver<u64>>> {
        self.ensure_open_session(session_id)?;
        Ok(self.runtime_session(session_id)?.pty.output_signal())
    }

    /// 同步更新 SessionManager 元数据与底层 PTY 尺寸。
    pub fn resize(&mut self, session_id: &str, size: TerminalSize) -> RuntimeResult<()> {
        let pty_size = terminal_size_to_pty_size(size)?;
        self.ensure_open_session(session_id)?;

        // 先调整真实 PTY，成功后再更新 session 元数据，避免状态显示已 resize 但进程未更新。
        self.runtime_session_mut(session_id)?.pty.resize(pty_size)?;
        self.sessions.resize(session_id, size)?;
        self.runtime_session_mut(session_id)?.updated_at_ms = current_unix_timestamp_millis();
        Ok(())
    }

    /// detach 只移除连接状态，不终止 PTY。
    ///
    /// 这保证了“client 断开不会杀 session”的核心不变量。
    pub fn detach(&mut self, session_id: &str, device_id: &str) -> RuntimeResult<()> {
        self.ensure_open_session(session_id)?;
        Ok(self.sessions.detach(session_id, device_id)?)
    }

    /// 显式关闭 runtime session，并终止对应 PTY 进程。
    ///
    /// PTY 生命周期只由 close 管理；普通 detach 不会调用 terminate。
    pub fn close(&mut self, session_id: &str) -> RuntimeResult<()> {
        self.ensure_open_session(session_id)?;
        self.runtime_session_mut(session_id)?.pty.terminate()?;
        self.runtime_sessions.remove(session_id);
        self.sessions.close(session_id)?;
        Ok(())
    }

    /// 丢弃 runtime session 的本地句柄，但不再尝试终止 PTY。
    ///
    /// 这个兜底路径只在显式 close 的终止步骤失败时使用，用来确保 daemon 不再保留
    /// 不可见的 runtime 句柄；真正的 PTY 终止仍然优先走 `close`。
    pub fn discard(&mut self, session_id: &str) -> RuntimeResult<()> {
        if self.runtime_sessions.remove(session_id).is_none() {
            return Err(RuntimeError::SessionNotFound);
        }

        self.sessions.close(session_id)?;
        Ok(())
    }

    /// 查询 session 当前状态，便于上层做只读展示或测试断言。
    pub fn state(&self, session_id: &str) -> RuntimeResult<SessionState> {
        Ok(self.sessions.state(session_id)?)
    }

    /// 查询设备在 session 中的角色。
    pub fn role(&self, session_id: &str, device_id: &str) -> RuntimeResult<Option<AttachRole>> {
        Ok(self.sessions.role(session_id, device_id)?)
    }

    /// 查询当前记录的终端尺寸。
    pub fn size(&self, session_id: &str) -> RuntimeResult<TerminalSize> {
        Ok(self.sessions.size(session_id)?)
    }

    /// 查询底层进程 id；fake backend 或不支持的平台可以返回 None。
    pub fn process_id(&self, session_id: &str) -> RuntimeResult<Option<u32>> {
        Ok(self.runtime_session(session_id)?.pty.process_id())
    }

    /// 读取 supervisor 的最近快照。
    pub fn snapshot(&mut self, session_id: &str) -> RuntimeResult<PtySnapshot> {
        self.ensure_open_session(session_id)?;
        Ok(self.runtime_session_mut(session_id)?.pty.snapshot()?)
    }

    /// 查询 session 对应的 supervisor 恢复信息。
    pub fn restore_info(&self, session_id: &str) -> RuntimeResult<Option<PtyRestoreInfo>> {
        self.ensure_open_session(session_id)?;
        Ok(self.runtime_session(session_id)?.pty.restore_info())
    }

    /// 导出当前 runtime 中可重连的 session 持久记录。
    pub fn persisted_sessions(&self) -> Vec<SessionStateRecord> {
        self.runtime_sessions
            .iter()
            .filter_map(|(session_id, runtime_session)| {
                let restore_info = runtime_session.pty.restore_info()?;
                let wire_session_id = SessionId(Uuid::parse_str(session_id).ok()?);
                let state = self.sessions.state(session_id).ok()?;
                let size = self.sessions.size(session_id).ok()?;

                Some(SessionStateRecord {
                    session_id: wire_session_id,
                    state: runtime_state_to_proto(state),
                    size: runtime_size_to_proto(size),
                    created_at_ms: runtime_session.created_at_ms,
                    updated_at_ms: runtime_session.updated_at_ms,
                    restore_info: Some(restore_info),
                })
            })
            .collect()
    }

    fn allocate_session_id(&mut self) -> String {
        loop {
            let session_id = format!("session-{}", self.next_session_number);
            self.next_session_number += 1;

            if !self.runtime_sessions.contains_key(&session_id) {
                return session_id;
            }
        }
    }

    fn ensure_runtime_session(&self, session_id: &str) -> RuntimeResult<()> {
        self.runtime_session(session_id).map(|_| ())
    }

    fn ensure_open_session(&self, session_id: &str) -> RuntimeResult<()> {
        match self.sessions.state(session_id)? {
            SessionState::Closed => Err(RuntimeError::SessionClosed),
            SessionState::Created | SessionState::Running => {
                self.ensure_runtime_session(session_id)
            }
        }
    }

    fn runtime_session(&self, session_id: &str) -> RuntimeResult<&RuntimeSession> {
        self.runtime_sessions
            .get(session_id)
            .ok_or(RuntimeError::SessionNotFound)
    }

    fn runtime_session_mut(&mut self, session_id: &str) -> RuntimeResult<&mut RuntimeSession> {
        self.runtime_sessions
            .get_mut(session_id)
            .ok_or(RuntimeError::SessionNotFound)
    }
}

impl<B: PtyBackend> fmt::Debug for SessionRuntime<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionRuntime")
            .field("sessions", &self.sessions)
            .field("runtime_session_count", &self.runtime_sessions.len())
            .field("next_session_number", &self.next_session_number)
            .finish_non_exhaustive()
    }
}

impl<B: PtyBackend + Default> Default for SessionRuntime<B> {
    fn default() -> Self {
        Self::new(B::default())
    }
}

fn terminal_size_to_pty_size(size: TerminalSize) -> RuntimeResult<PtySize> {
    if size.rows == 0 || size.cols == 0 {
        return Err(RuntimeError::InvalidSize);
    }

    Ok(PtySize::with_pixels(
        size.rows,
        size.cols,
        size.pixel_width,
        size.pixel_height,
    ))
}

fn proto_size_to_runtime(size: ProtoTerminalSize) -> TerminalSize {
    TerminalSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn runtime_state_to_proto(state: SessionState) -> ProtoSessionState {
    match state {
        SessionState::Created => ProtoSessionState::Created,
        SessionState::Running => ProtoSessionState::Running,
        SessionState::Closed => ProtoSessionState::Closed,
    }
}

fn runtime_size_to_proto(size: TerminalSize) -> ProtoTerminalSize {
    ProtoTerminalSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn current_unix_timestamp_millis() -> UnixTimestampMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    UnixTimestampMillis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{
        CommandSpec, PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize,
        PtySnapshot,
    };
    use crate::session::{AttachRole, TerminalSize};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakePtyBackend {
        state: Arc<Mutex<FakePtyState>>,
    }

    #[derive(Debug, Default)]
    struct FakePtyState {
        spawns: Vec<(CommandSpec, PtySize)>,
        writes: Vec<Vec<u8>>,
        resizes: Vec<PtySize>,
        terminate_count: usize,
    }

    impl FakePtyBackend {
        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        fn resizes(&self) -> Vec<PtySize> {
            self.state.lock().unwrap().resizes.clone()
        }

        fn terminate_count(&self) -> usize {
            self.state.lock().unwrap().terminate_count
        }
    }

    impl PtyBackend for FakePtyBackend {
        fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>> {
            self.state
                .lock()
                .unwrap()
                .spawns
                .push((command.clone(), size));

            Ok(Box::new(FakePtySession {
                state: Arc::clone(&self.state),
            }))
        }
    }

    struct FakePtySession {
        state: Arc<Mutex<FakePtyState>>,
    }

    impl PtySession for FakePtySession {
        fn read(&mut self, _buffer: &mut [u8]) -> PtyResult<usize> {
            Ok(0)
        }

        fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
            self.state.lock().unwrap().writes.push(bytes.to_vec());
            Ok(())
        }

        fn resize(&mut self, size: PtySize) -> PtyResult<()> {
            self.state.lock().unwrap().resizes.push(size);
            Ok(())
        }

        fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
            Ok(PtySnapshot {
                size: self
                    .state
                    .lock()
                    .unwrap()
                    .resizes
                    .last()
                    .copied()
                    .unwrap_or_else(|| PtySize::new(24, 80)),
                process_id: Some(42),
                retained_output: Vec::new(),
            })
        }

        fn terminate(&mut self) -> PtyResult<()> {
            self.state.lock().unwrap().terminate_count += 1;
            Ok(())
        }

        fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> PtyResult<PtyExitStatus> {
            Err(PtyError::Backend(
                "fake wait is not used by runtime tests".into(),
            ))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    #[test]
    fn first_attach_after_create_becomes_operator() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend);

        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();

        let role = runtime.attach(&session_id, "dev-a").unwrap();

        assert_eq!(role, AttachRole::Operator);
    }

    #[test]
    fn operator_input_is_written_to_pty() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend.clone());
        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();
        runtime.attach(&session_id, "dev-a").unwrap();

        runtime
            .write_input(&session_id, "dev-a", b"echo ok\n")
            .unwrap();

        assert_eq!(backend.writes(), vec![b"echo ok\n".to_vec()]);
    }

    #[test]
    fn additional_attached_device_input_is_written_to_shared_pty() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend.clone());
        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();
        runtime.attach(&session_id, "dev-a").unwrap();
        runtime.attach(&session_id, "dev-b").unwrap();

        runtime
            .write_input(&session_id, "dev-b", b"whoami\n")
            .unwrap();

        assert_eq!(backend.writes(), vec![b"whoami\n".to_vec()]);
    }

    #[test]
    fn detach_does_not_close_runtime_session() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend.clone());
        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();
        runtime.attach(&session_id, "dev-a").unwrap();

        runtime.detach(&session_id, "dev-a").unwrap();
        let role = runtime.attach(&session_id, "dev-b").unwrap();

        assert_eq!(role, AttachRole::Operator);
        assert_eq!(backend.terminate_count(), 0);
    }

    #[test]
    fn resize_updates_pty_and_session_size() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend.clone());
        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();
        let new_size = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 800,
            pixel_height: 600,
        };

        runtime.resize(&session_id, new_size).unwrap();

        assert_eq!(
            backend.resizes(),
            vec![PtySize::with_pixels(40, 120, 800, 600)]
        );
        assert_eq!(runtime.size(&session_id).unwrap(), new_size);
    }

    #[test]
    fn close_terminates_pty_and_closes_session() {
        let backend = FakePtyBackend::default();
        let mut runtime = SessionRuntime::new(backend.clone());
        let session_id = runtime
            .create_session(CommandSpec::new("sh"), TerminalSize::cells(24, 80))
            .unwrap();
        runtime.attach(&session_id, "dev-a").unwrap();

        runtime.close(&session_id).unwrap();

        assert_eq!(backend.terminate_count(), 1);
        let error = runtime.attach(&session_id, "dev-b").unwrap_err();
        assert_eq!(error, RuntimeError::SessionClosed);
    }
}
