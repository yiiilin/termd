//! termd 的内存版 session/control 状态核心。
//!
//! 这个模块只维护 daemon 内部的 session 生命周期和 attach 状态。
//! 认证、配对、PTY I/O、网络协议和持久化都不在这里实现，避免把 MVP 过早做成复杂平台。

use std::collections::HashMap;
use std::fmt;

/// session 在内核中的生命周期。
///
/// 状态机固定为：`Created -> Running -> Closed`。
/// `Closed` 是终态，进入后禁止再次 attach、resize 或切换控制权。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Created,
    Running,
    Closed,
}

/// attach 后设备在 session 中获得的角色。
///
/// shared-control 模式下，所有已 attach 设备都是 operator，都可以向同一个 PTY 输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    Operator,
}

/// 终端窗口尺寸。
///
/// pixel 字段允许 UI 后续传递像素尺寸；MVP 中只要求 rows/cols 非零。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalSize {
    /// 构造字符单元尺寸，像素尺寸留空为 0。
    pub fn cells(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn is_valid(self) -> bool {
        self.rows > 0 && self.cols > 0
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::cells(24, 80)
    }
}

/// session 状态核心的错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    SessionAlreadyExists,
    SessionNotFound,
    SessionClosed,
    DeviceNotAttached,
    InvalidSize,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionAlreadyExists => write!(f, "session already exists"),
            Self::SessionNotFound => write!(f, "session not found"),
            Self::SessionClosed => write!(f, "session is closed"),
            Self::DeviceNotAttached => write!(f, "device is not attached"),
            Self::InvalidSize => write!(f, "terminal size must have non-zero rows and cols"),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug)]
struct SessionRecord {
    state: SessionState,
    attached_devices: HashMap<String, AttachRole>,
    size: TerminalSize,
}

impl SessionRecord {
    fn new() -> Self {
        Self {
            state: SessionState::Created,
            attached_devices: HashMap::new(),
            size: TerminalSize::default(),
        }
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.state == SessionState::Closed {
            return Err(SessionError::SessionClosed);
        }

        Ok(())
    }
}

/// 单进程内存版 session 管理器。
///
/// 本类型假定调用方已经完成配对和 device key 验证；这里接收到的 device id 都被视为可信。
/// 这样可以保持 auth 与 session 两个边界清晰，后续接入网络层时也不会把控制权逻辑混入 relay。
#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: HashMap<String, SessionRecord>,
}

impl SessionManager {
    /// 创建一个处于 `Created` 状态的 session。
    pub fn create_session(&mut self, session_id: impl Into<String>) -> Result<(), SessionError> {
        let session_id = session_id.into();

        if self.sessions.contains_key(&session_id) {
            return Err(SessionError::SessionAlreadyExists);
        }

        self.sessions.insert(session_id, SessionRecord::new());
        Ok(())
    }

    /// 将可信设备 attach 到 session。
    ///
    /// shared-control 规则：
    /// - session 首次从 `Created` attach 时转为 `Running`。
    /// - 所有已 attach 设备都是 operator，可以共同向 PTY 输入。
    pub fn attach(
        &mut self,
        session_id: &str,
        device_id: impl Into<String>,
    ) -> Result<AttachRole, SessionError> {
        let device_id = device_id.into();
        let session = self.session_mut(session_id)?;
        session.ensure_open()?;

        if session.state == SessionState::Created {
            session.state = SessionState::Running;
        }

        if let Some(role) = session.attached_devices.get(&device_id).copied() {
            return Ok(role);
        }

        let role = AttachRole::Operator;

        session.attached_devices.insert(device_id, role);
        Ok(role)
    }

    /// shared-control 模式没有夺权概念；该方法只保留为旧 CLI 命令的无害确认路径。
    pub fn steal_control(&mut self, session_id: &str, device_id: &str) -> Result<(), SessionError> {
        let session = self.session_mut(session_id)?;
        session.ensure_open()?;

        if !session.attached_devices.contains_key(device_id) {
            return Err(SessionError::DeviceNotAttached);
        }

        Ok(())
    }

    /// 设备 detach 只断开连接状态，不关闭 session。
    pub fn detach(&mut self, session_id: &str, device_id: &str) -> Result<(), SessionError> {
        let session = self.session_mut(session_id)?;
        session.ensure_open()?;

        if session.attached_devices.remove(device_id).is_some() {
            Ok(())
        } else {
            Err(SessionError::DeviceNotAttached)
        }
    }

    /// 显式关闭 session。
    ///
    /// close 是终态转换；关闭后保留 session 记录用于状态查询，但清空连接和控制权。
    pub fn close(&mut self, session_id: &str) -> Result<(), SessionError> {
        let session = self.session_mut(session_id)?;

        session.state = SessionState::Closed;
        session.attached_devices.clear();
        Ok(())
    }

    /// 更新终端尺寸。
    ///
    /// resize 属于 RUNNING/CREATED session 的元数据变更；关闭后禁止执行。
    pub fn resize(&mut self, session_id: &str, size: TerminalSize) -> Result<(), SessionError> {
        if !size.is_valid() {
            return Err(SessionError::InvalidSize);
        }

        let session = self.session_mut(session_id)?;
        session.ensure_open()?;
        session.size = size;
        Ok(())
    }

    pub fn state(&self, session_id: &str) -> Result<SessionState, SessionError> {
        Ok(self.session(session_id)?.state)
    }

    pub fn role(
        &self,
        session_id: &str,
        device_id: &str,
    ) -> Result<Option<AttachRole>, SessionError> {
        Ok(self
            .session(session_id)?
            .attached_devices
            .get(device_id)
            .copied())
    }

    pub fn size(&self, session_id: &str) -> Result<TerminalSize, SessionError> {
        Ok(self.session(session_id)?.size)
    }

    fn session(&self, session_id: &str) -> Result<&SessionRecord, SessionError> {
        self.sessions
            .get(session_id)
            .ok_or(SessionError::SessionNotFound)
    }

    fn session_mut(&mut self, session_id: &str) -> Result<&mut SessionRecord, SessionError> {
        self.sessions
            .get_mut(session_id)
            .ok_or(SessionError::SessionNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_attach_becomes_operator() {
        let mut manager = SessionManager::default();
        manager.create_session("s1").unwrap();

        let role = manager.attach("s1", "dev-a").unwrap();

        assert_eq!(role, AttachRole::Operator);
        assert_eq!(manager.state("s1").unwrap(), SessionState::Running);
    }

    #[test]
    fn second_attach_is_also_operator() {
        let mut manager = SessionManager::default();
        manager.create_session("s1").unwrap();
        manager.attach("s1", "dev-a").unwrap();

        let role = manager.attach("s1", "dev-b").unwrap();

        assert_eq!(role, AttachRole::Operator);
        assert_eq!(
            manager.role("s1", "dev-b").unwrap(),
            Some(AttachRole::Operator)
        );
    }

    #[test]
    fn control_request_is_noop_for_attached_operator() {
        let mut manager = SessionManager::default();
        manager.create_session("s1").unwrap();
        manager.attach("s1", "dev-a").unwrap();
        manager.attach("s1", "dev-b").unwrap();

        manager.steal_control("s1", "dev-b").unwrap();

        assert_eq!(
            manager.role("s1", "dev-a").unwrap(),
            Some(AttachRole::Operator)
        );
        assert_eq!(
            manager.role("s1", "dev-b").unwrap(),
            Some(AttachRole::Operator)
        );
    }

    #[test]
    fn detach_does_not_close_running_session() {
        let mut manager = SessionManager::default();
        manager.create_session("s1").unwrap();
        manager.attach("s1", "dev-a").unwrap();

        manager.detach("s1", "dev-a").unwrap();

        assert_eq!(manager.state("s1").unwrap(), SessionState::Running);
        assert_eq!(manager.role("s1", "dev-a").unwrap(), None);
    }

    #[test]
    fn closed_session_rejects_attach_control_and_resize() {
        let mut manager = SessionManager::default();
        manager.create_session("s1").unwrap();
        manager.attach("s1", "dev-a").unwrap();
        manager.close("s1").unwrap();

        let attach_error = manager.attach("s1", "dev-b").unwrap_err();
        let control_error = manager.steal_control("s1", "dev-a").unwrap_err();
        let resize_error = manager
            .resize("s1", TerminalSize::cells(40, 120))
            .unwrap_err();

        assert_eq!(attach_error, SessionError::SessionClosed);
        assert_eq!(control_error, SessionError::SessionClosed);
        assert_eq!(resize_error, SessionError::SessionClosed);
    }
}
