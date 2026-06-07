//! termd 的 PTY 抽象层。
//!
//! 这个模块只负责“如何启动并驱动一个伪终端进程”。认证、控制权、relay 路由、
//! WebSocket 协议和 E2EE 都在更外层处理，避免 PTY 后端意外承担控制权逻辑。

pub mod portable;
pub mod supervisor;
pub mod tmux;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display};
use std::io;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

/// PTY 模块统一使用的 Result 类型。
pub type PtyResult<T> = Result<T, PtyError>;

/// PTY 层错误。
///
/// 这里刻意保持轻量，不引入额外错误依赖；上层可以按需把它转换成 daemon 自己的错误类型。
#[derive(Debug)]
pub enum PtyError {
    /// 命令规格本身不合法，例如 program 为空。
    InvalidCommand(String),
    /// 标准 IO 读写或等待子进程时发生错误。
    Io(io::Error),
    /// 具体 PTY 后端返回的错误。后端细节不泄漏到 session/auth/relay 层。
    Backend(String),
}

impl PtyError {
    /// 将后端错误压平为字符串，保持公共抽象不绑定具体后端错误类型。
    pub fn backend(error: impl Display) -> Self {
        Self::Backend(error.to_string())
    }
}

impl Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand(message) => write!(f, "invalid PTY command: {message}"),
            Self::Io(error) => write!(f, "PTY IO error: {error}"),
            Self::Backend(message) => write!(f, "PTY backend error: {message}"),
        }
    }
}

impl Error for PtyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidCommand(_) | Self::Backend(_) => None,
        }
    }
}

impl From<io::Error> for PtyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 要在 PTY 中启动的命令。
///
/// `CommandSpec` 只描述进程启动参数，不决定默认 shell；默认 shell 应由 CLI 或集成层显式选择。
/// 这样可以避免 PTY 模块暗中读取用户环境并影响 session 状态机的可测试性。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(default)]
    removed_env: BTreeSet<String>,
    cwd: Option<PathBuf>,
}

impl CommandSpec {
    /// 创建命令规格。`program` 必须在真正启动前通过 `validate` 校验为非空。
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            removed_env: BTreeSet::new(),
            cwd: None,
        }
    }

    /// 追加一个命令行参数。
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// 批量追加命令行参数。
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// 设置或覆盖一个环境变量。
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        self.removed_env.remove(&key);
        self.env.insert(key, value.into());
        self
    }

    /// 从子进程环境中移除变量。
    ///
    /// 这主要用于 tmux/terminal bridge，避免继承 daemon 自身的 `TMUX` 或 `TERM=dumb`
    /// 之类运行环境后让子终端误判自己已经在另一个不可用终端里。
    pub fn remove_env(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.env.remove(&key);
        self.removed_env.insert(key);
        self
    }

    /// 设置进程工作目录。
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// 返回程序名。
    pub fn program(&self) -> &str {
        &self.program
    }

    /// 返回附加参数，不包含 argv[0]。
    pub fn args_slice(&self) -> &[String] {
        &self.args
    }

    /// 返回额外环境变量。
    pub fn env_map(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// 返回需要从子进程环境中移除的变量名。
    pub fn removed_env(&self) -> &BTreeSet<String> {
        &self.removed_env
    }

    /// 返回工作目录。
    pub fn cwd_path(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// 返回完整 argv 视图，便于测试和日志记录。
    pub fn argv(&self) -> Vec<&str> {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect()
    }

    /// 校验命令最小合法性。更复杂的路径解析交给具体后端或系统完成。
    pub fn validate(&self) -> PtyResult<()> {
        if self.program.trim().is_empty() {
            return Err(PtyError::InvalidCommand(
                "program must not be empty".to_string(),
            ));
        }

        Ok(())
    }
}

/// 终端尺寸。
///
/// rows/cols 是字符网格尺寸；pixel_width/pixel_height 仅作为终端渲染提示，
/// 有些平台会忽略它们。协议层和 UI 层可以传入这些值，但 PTY 模块不解释 UI 语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl PtySize {
    /// 常见终端默认行数。
    pub const DEFAULT_ROWS: u16 = 24;
    /// 常见终端默认列数。
    pub const DEFAULT_COLS: u16 = 80;

    /// 使用字符网格尺寸构造 PTY size，像素尺寸默认为 0。
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// 使用完整尺寸构造 PTY size。
    pub const fn with_pixels(rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width,
            pixel_height,
        }
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self::new(Self::DEFAULT_ROWS, Self::DEFAULT_COLS)
    }
}

/// session supervisor 的可持久化生命周期状态。
///
/// 该状态用于 daemon 重启后判断恢复记录是否仍指向一个预期存活的 supervisor，
/// 不是控制权状态，也不会进入 relay 或 UI 层做业务判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtySupervisorStatus {
    Running,
    Closing,
    Closed,
}

/// daemon 可持久化的 PTY 恢复信息。
///
/// 这个结构只保存“如何重新连回 session supervisor”的最小 IPC 路径和 supervisor
/// 进程事实，不保存任何终端明文、输入历史或密钥材料。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PtyRestoreInfo {
    UnixSocket {
        socket_path: PathBuf,
        supervisor_pid: u32,
        supervisor_status: PtySupervisorStatus,
    },
    Tmux {
        socket_path: PathBuf,
        session_name: String,
    },
}

/// supervisor 快照。
///
/// `retained_output` 是 daemon 重启后用于恢复终端显示的内存快照，不会被写入本地持久状态。
/// supervisor backend 会返回屏幕模型生成的快照；普通 backend 可以返回最近仍保留的原始输出。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtySnapshot {
    pub size: PtySize,
    pub process_id: Option<u32>,
    pub retained_output: Vec<u8>,
}

/// supervisor 暴露给 daemon 的 session 级终端帧。
///
/// 中文注释：`terminal_seq` 是 session 级终端事件序号，用于 snapshot 后补 tail；
/// 它和 `ProtocolPacket.seq` 的连接内传输序号不是同一个东西，不能混用。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PtyTerminalFrame {
    Snapshot {
        base_seq: u64,
        size: PtySize,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Output {
        terminal_seq: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    Resize {
        terminal_seq: u64,
        size: PtySize,
    },
    Exit {
        terminal_seq: u64,
        code: Option<i32>,
    },
}

impl PtyTerminalFrame {
    pub fn terminal_seq(&self) -> Option<u64> {
        match self {
            Self::Snapshot { .. } => None,
            Self::Output { terminal_seq, .. }
            | Self::Resize { terminal_seq, .. }
            | Self::Exit { terminal_seq, .. } => Some(*terminal_seq),
        }
    }

    pub fn bytes_for_legacy_read(&self) -> Option<&[u8]> {
        match self {
            Self::Snapshot { data, .. } | Self::Output { data, .. } => Some(data),
            Self::Resize { .. } | Self::Exit { .. } => None,
        }
    }
}

/// 子进程退出状态的后端无关表示。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtyExitStatus {
    pub exit_code: u32,
    pub signal: Option<String>,
}

impl PtyExitStatus {
    /// 构造正常退出状态。
    pub fn exited(exit_code: u32) -> Self {
        Self {
            exit_code,
            signal: None,
        }
    }

    /// 构造被信号终止的状态。
    pub fn signaled(signal: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            signal: Some(signal.into()),
        }
    }

    /// 判断进程是否成功退出。
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.exit_code == 0
    }
}

/// PTY 后端接口。
///
/// session manager 只依赖这个 trait，就可以在测试里替换成 fake backend，
/// 并把真实 `portable-pty` 的平台差异限制在适配层。
pub trait PtyBackend: Send + Sync {
    /// 启动一个 PTY session。
    fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>>;

    /// 使用调用方指定的 session id 启动一个 PTY session。
    ///
    /// 本地直接 PTY backend 可以忽略这个提示；supervisor backend 需要它来派生稳定 socket 路径。
    fn spawn_named(
        &self,
        _session_id: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        self.spawn(command, size)
    }

    /// 基于持久化的恢复信息重新连回一个仍存活的 session supervisor。
    ///
    /// 默认 backend 不支持重连；生产 supervisor backend 会覆盖实现。
    fn reconnect(
        &self,
        _session_id: &str,
        _restore_info: &PtyRestoreInfo,
        _size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        Err(PtyError::Backend(
            "PTY backend does not support reconnect".to_owned(),
        ))
    }

    /// 为一个已存在 session 创建连接级 attach client。
    ///
    /// 中文注释：这个 handle 只表达“当前 Web terminal watcher 有一个对应的 PTY/tmux
    /// attach 生命周期”，不表达设备权限，也不改变 session 级 terminal_seq 输出模型。
    /// 普通 backend 没有独立 attach client，因此默认返回 no-op handle；tmux backend 会
    /// 覆盖为真实的 control-mode tmux client。
    fn attach_client(
        &self,
        _session_id: &str,
        _restore_info: Option<&PtyRestoreInfo>,
        _size: PtySize,
        _attachment_id: &str,
    ) -> PtyResult<Box<dyn PtyAttachment>> {
        Ok(Box::new(NoopPtyAttachment))
    }
}

/// 连接级 PTY attach handle。
///
/// 中文注释：它是 runtime/protocol 的生命周期资源，不是 session host。正常路径会显式
/// 调用 `detach`；具体 backend 的 Drop 仍应做 best-effort 清理，防止异常路径泄漏 client。
pub trait PtyAttachment: Send {
    fn detach(&mut self) -> PtyResult<()> {
        Ok(())
    }
}

struct NoopPtyAttachment;

impl PtyAttachment for NoopPtyAttachment {}

/// 运行中的 PTY session。
///
/// 这里暴露 daemon 内核需要的最小操作：读输出、写输入、resize、终止和等待退出。
/// attach/operator 判断必须由 session/control 层在调用写入前完成，PTY 层不识别业务角色。
pub trait PtySession: Send {
    /// 从 PTY 输出流读取数据；调用方负责选择阻塞线程或异步桥接方式。
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize>;

    /// 返回输出就绪信号。真实 WebSocket daemon 用该信号主动推送终端输出，避免客户端轮询。
    fn output_signal(&self) -> Option<watch::Receiver<u64>> {
        None
    }

    /// 写入用户输入或控制序列。调用前应由 session 层确认当前设备具备控制权。
    fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()>;

    /// 调整 PTY 尺寸。
    fn resize(&mut self, size: PtySize) -> PtyResult<()>;

    /// 获取最近一次 supervisor 快照。
    ///
    /// 本地 backend 默认只返回当前尺寸和 pid，不提供 retained output。
    fn snapshot(&mut self) -> PtyResult<PtySnapshot>;

    /// 获取结构化 terminal snapshot/tail。
    ///
    /// 中文注释：普通 backend 没有 session 级 terminal_seq，只能退化为一个 base_seq=0
    /// 的 snapshot；supervisor backend 会覆盖为权威 1000 行热历史和 journal tail。
    fn terminal_snapshot(
        &mut self,
        _last_terminal_seq: Option<u64>,
    ) -> PtyResult<Vec<PtyTerminalFrame>> {
        let snapshot = self.snapshot()?;
        Ok(vec![PtyTerminalFrame::Snapshot {
            base_seq: 0,
            size: snapshot.size,
            data: snapshot.retained_output,
        }])
    }

    /// 读取一个结构化 terminal live frame。
    ///
    /// 中文注释：默认实现只能把裸 PTY 输出包装成 terminal_seq=0 的兼容输出帧；
    /// production supervisor backend 会返回真实 session 级 terminal_seq。
    fn read_terminal_frame(&mut self) -> PtyResult<Option<PtyTerminalFrame>> {
        let mut buffer = vec![0_u8; 16 * 1024];
        let read = self.read(&mut buffer)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some(PtyTerminalFrame::Output {
            terminal_seq: 0,
            data: buffer,
        }))
    }

    /// 心跳探测，供 daemon 重连或后台健康检查使用。
    fn ping(&mut self) -> PtyResult<()> {
        Ok(())
    }

    /// 返回本 session 的可持久恢复信息；不支持重连的 backend 返回 `None`。
    fn restore_info(&self) -> Option<PtyRestoreInfo> {
        None
    }

    /// 请求终止 PTY 子进程。
    fn terminate(&mut self) -> PtyResult<()>;

    /// 非阻塞检查子进程是否退出。
    fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>>;

    /// 阻塞等待子进程退出。
    fn wait(&mut self) -> PtyResult<PtyExitStatus>;

    /// 返回本地子进程 id；不适用的平台可以返回 None。
    fn process_id(&self) -> Option<u32>;

    /// 返回 PTY 主进程当前工作目录。
    ///
    /// 这个能力用于让文件面板跟随交互 shell 的 `cd`；不支持的平台或权限受限时返回 `None`，
    /// 上层会回退到已保存的文件面板路径。
    fn current_working_directory(&self) -> Option<PathBuf> {
        None
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

/// Linux 上通过 `/proc/<pid>/cwd` 读取 shell 当前目录；其他平台先降级为不可用。
pub(crate) fn current_working_directory_for_pid(pid: u32) -> Option<PathBuf> {
    platform_current_working_directory_for_pid(pid)
}

#[cfg(target_os = "linux")]
fn platform_current_working_directory_for_pid(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(target_os = "linux"))]
fn platform_current_working_directory_for_pid(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn command_spec_builds_program_args_env_and_cwd() {
        let command = CommandSpec::new("ssh")
            .args(["-tt", "example.com"])
            .arg("tmux")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .cwd("/tmp");

        assert_eq!(command.program(), "ssh");
        assert_eq!(command.args_slice(), ["-tt", "example.com", "tmux"]);
        assert_eq!(command.argv(), ["ssh", "-tt", "example.com", "tmux"]);
        assert_eq!(
            command.env_map().get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            command.env_map().get("LANG").map(String::as_str),
            Some("C.UTF-8")
        );
        assert!(command.removed_env().is_empty());
        assert_eq!(command.cwd_path(), Some(Path::new("/tmp")));
        assert!(command.validate().is_ok());
    }

    #[test]
    fn command_spec_can_remove_inherited_env() {
        let command = CommandSpec::new("tmux")
            .env("TERM", "xterm-256color")
            .remove_env("TMUX");

        assert_eq!(
            command.env_map().get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert!(command.removed_env().contains("TMUX"));
    }

    #[test]
    fn command_spec_rejects_empty_program() {
        let command = CommandSpec::new("   ");

        assert!(matches!(
            command.validate(),
            Err(PtyError::InvalidCommand(_))
        ));
    }

    #[test]
    fn pty_size_uses_reasonable_defaults() {
        let size = PtySize::default();

        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
        assert_eq!(size.pixel_width, 0);
        assert_eq!(size.pixel_height, 0);
    }

    #[test]
    fn pty_size_can_include_pixels_without_ui_coupling() {
        let size = PtySize::with_pixels(40, 120, 960, 640);

        assert_eq!(
            size,
            PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 960,
                pixel_height: 640,
            }
        );
    }
}
