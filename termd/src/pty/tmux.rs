//! tmux-backed PTY backend。
//!
//! tmux 是持久 session 真相源；daemon 侧保留一个 `tmux attach-session` PTY client
//! 作为 session 级 I/O bridge；watched Web attach 额外持有 control-mode tmux client
//! lifecycle handle。两者都不能把 auth、relay 或控制权逻辑下沉到 tmux 层。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::watch;

use crate::net::pty_bridge::NonBlockingPortablePtyBackend;

use super::{
    CommandSpec, PtyAttachment, PtyBackend, PtyError, PtyExitStatus, PtyRestoreInfo, PtyResult,
    PtySession, PtySize, PtySnapshot, PtyTerminalFrame,
};

const TMUX_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const TMUX_ATTACH_STARTUP_CHECK_INTERVAL: Duration = Duration::from_millis(20);
const TMUX_ATTACH_STARTUP_CHECK_POLLS: usize = 2;
const TMUX_ATTACHMENT_STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const TMUX_ATTACHMENT_STOP_POLLS: usize = 50;
// 中文注释：这是 daemon 持有的 tmux attach client 的外层 TERM，不是用户 shell 里的 TERM。
// 使用不声明 smcup/rmcup 的 ansi，避免 tmux attach 把浏览器 Ghostty 拉进 alternate screen；
// 否则 Ghostty 普通 scrollback 不会累积，侧边滚动条无法浏览 tmux pane 输出历史。
const TMUX_BRIDGE_TERM: &str = "ansi";
const TMUX_BRIDGE_ENV_REMOVE_KEYS: &[&str] = &["TMUX", "TMUX_PANE"];
const TMUX_HISTORY_LIMIT_LINES: u16 = 500;
const TMUX_STATUS: &str = "off";
const TMUX_CLEAR_HISTORY_SCAN_TAIL_MAX_BYTES: usize = 32;

/// 生产 daemon 的 tmux session host backend。
#[derive(Debug, Clone)]
pub struct TmuxPtyBackend {
    tmux_path: PathBuf,
    socket_path: PathBuf,
}

impl TmuxPtyBackend {
    /// 使用显式 tmux socket 构造 backend，主要供测试使用。
    pub fn with_socket_path(socket_path: impl AsRef<Path>) -> Self {
        Self {
            tmux_path: PathBuf::from("tmux"),
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    /// 基于 daemon state path 派生 tmux socket。
    pub fn for_state_path(state_path: impl AsRef<Path>) -> Self {
        Self::with_socket_path(tmux_socket_path_for_state_path(state_path.as_ref()))
    }

    fn session_name_for_id(session_id: &str) -> String {
        let suffix = session_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        format!("termd-{suffix}")
    }

    fn spawn_tmux_session(
        &self,
        session_name: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<()> {
        command.validate()?;
        tracing::debug!(
            program = %command.program(),
            arg_count = command.args_slice().len(),
            "spawning tmux-backed PTY session"
        );
        ensure_socket_parent(&self.socket_path)?;
        remove_stale_socket_if_needed(&self.tmux_path, &self.socket_path)?;
        self.write_tmux_startup_config()?;
        self.set_global_session_options_if_server_running()?;

        let mut tmux = self.tmux_command_with_startup_config();
        tmux.args([
            "new-session",
            "-d",
            "-s",
            session_name,
            "-x",
            &size.cols.to_string(),
            "-y",
            &size.rows.to_string(),
        ]);
        for (key, value) in command.env_map() {
            if key.is_empty() || key.contains('=') {
                return Err(PtyError::Backend(format!(
                    "tmux environment key is invalid: {key:?}"
                )));
            }
            tmux.args(["-e", &format!("{key}={value}")]);
        }
        if let Some(cwd) = command.cwd_path() {
            tmux.arg("-c").arg(cwd);
        }
        tmux.arg("--")
            .arg(command.program())
            .args(command.args_slice())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        run_tmux_status(tmux, "new-session")?;
        self.set_session_options_for_target(session_name)
    }

    fn attach_tmux_client(
        &self,
        session_name: &str,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        if !self.has_session(session_name)? {
            return Err(PtyError::Backend(format!(
                "tmux session {session_name} is not running"
            )));
        }
        self.set_global_session_options_if_server_running()?;
        self.set_session_options_for_target(session_name)?;
        let attach_command = self.attach_tmux_command(session_name);
        let mut attach_client =
            NonBlockingPortablePtyBackend::new().spawn(&attach_command, size)?;
        ensure_tmux_attach_client_stayed_alive(&mut *attach_client, session_name)?;
        Ok(attach_client)
    }

    fn attach_tmux_command(&self, session_name: &str) -> CommandSpec {
        let mut command = CommandSpec::new(self.tmux_path.to_string_lossy().to_string())
            .args([
                "-S".to_owned(),
                self.socket_path.to_string_lossy().to_string(),
                "attach-session".to_owned(),
                "-t".to_owned(),
                session_name.to_owned(),
            ])
            // 中文注释：daemon 经常由 systemd/测试脚本以 TERM=dumb 启动。tmux attach
            // 会继承环境并直接早退，所以 bridge 必须显式声明一个可用终端能力集。
            .env("TERM", TMUX_BRIDGE_TERM);
        for key in TMUX_BRIDGE_ENV_REMOVE_KEYS {
            command = command.remove_env(*key);
        }
        command
    }

    fn attach_control_client(
        &self,
        session_name: &str,
        attachment_id: &str,
    ) -> PtyResult<Box<dyn PtyAttachment>> {
        if !self.has_session(session_name)? {
            return Err(PtyError::Backend(format!(
                "tmux session {session_name} is not running"
            )));
        }
        self.set_global_session_options_if_server_running()?;
        self.set_session_options_for_target(session_name)?;

        let mut command = self.tmux_command();
        command
            .arg("-C")
            .args(["attach-session", "-t", session_name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(PtyError::from)?;
        let client_pid = child.id();
        let mut stdin = None;
        let mut stdout_thread = None;
        let mut stderr_thread = None;
        let setup_result = (|| -> PtyResult<()> {
            stdin = Some(child.stdin.take().ok_or_else(|| {
                PtyError::Backend("tmux attachment stdin pipe was not captured".to_owned())
            })?);
            let stdout = child.stdout.take().ok_or_else(|| {
                PtyError::Backend("tmux attachment stdout pipe was not captured".to_owned())
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                PtyError::Backend("tmux attachment stderr pipe was not captured".to_owned())
            })?;
            stdout_thread = Some(spawn_tmux_attachment_drain(
                attachment_id,
                "stdout",
                stdout,
            )?);
            stderr_thread = Some(spawn_tmux_attachment_drain(
                attachment_id,
                "stderr",
                stderr,
            )?);
            Ok(())
        })();
        if let Err(error) = setup_result {
            drop(stdin.take());
            stop_tmux_attachment_child(&mut child);
            join_tmux_attachment_drain(stdout_thread);
            join_tmux_attachment_drain(stderr_thread);
            return Err(error);
        }

        Ok(Box::new(TmuxPtyAttachment {
            backend: self.clone(),
            session_name: session_name.to_owned(),
            client_pid,
            child: Some(child),
            stdin,
            stdout_thread,
            stderr_thread,
            detached: false,
        }))
    }

    fn tmux_startup_config_path(&self) -> PathBuf {
        self.socket_path.with_extension("conf")
    }

    fn write_tmux_startup_config(&self) -> PtyResult<()> {
        // 中文注释：tmux 第一次启动 server 时只能通过 `-f` 提前设置全局 history-limit；
        // 后置 set-option 会太晚，快速输出的命令已经可能写满默认 2000 行历史。
        // remain-on-exit 同样必须预置；协议测试和 CLI 仍允许短生命周期命令，命令退出
        // 后保留 dead pane 才能让 daemon attach/capture 最后一屏输出。
        // status line 是 tmux 自己的 UI，会占掉 termd/Ghostty 的最后一行；termd
        // 已经有外层 session chrome，所以 tmux 管理的 session 默认关闭它。
        fs::write(
            self.tmux_startup_config_path(),
            format!(
                "set-option -g history-limit {TMUX_HISTORY_LIMIT_LINES}\nset-option -g remain-on-exit on\nset-option -g status {TMUX_STATUS}\n"
            ),
        )
        .map_err(PtyError::from)
    }

    fn tmux_command(&self) -> ProcessCommand {
        self.build_tmux_command(false)
    }

    fn tmux_command_with_startup_config(&self) -> ProcessCommand {
        self.build_tmux_command(true)
    }

    fn build_tmux_command(&self, include_startup_config: bool) -> ProcessCommand {
        let mut command = ProcessCommand::new(&self.tmux_path);
        command.arg("-S").arg(&self.socket_path);
        if include_startup_config {
            command.arg("-f").arg(self.tmux_startup_config_path());
        }
        command.env("TERM", TMUX_BRIDGE_TERM);
        for key in TMUX_BRIDGE_ENV_REMOVE_KEYS {
            command.env_remove(key);
        }
        command
    }

    fn set_global_session_options_if_server_running(&self) -> PtyResult<()> {
        if !self.socket_path.exists() || !tmux_server_responds(&self.tmux_path, &self.socket_path)?
        {
            return Ok(());
        }
        let history_limit = TMUX_HISTORY_LIMIT_LINES.to_string();
        let mut command = self.tmux_command();
        command
            .args(["set-option", "-g", "history-limit", &history_limit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "set-option history-limit")?;

        let mut command = self.tmux_command();
        command
            .args(["set-option", "-g", "remain-on-exit", "on"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "set-option remain-on-exit").and_then(|_| {
            let mut command = self.tmux_command();
            command
                .args(["set-option", "-g", "status", TMUX_STATUS])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            run_tmux_status(command, "set-option status")
        })
    }

    fn set_session_options_for_target(&self, target: &str) -> PtyResult<()> {
        let history_limit = TMUX_HISTORY_LIMIT_LINES.to_string();
        let mut command = self.tmux_command();
        command
            .args(["set-option", "-t", target, "history-limit", &history_limit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "set-option history-limit")?;

        let mut command = self.tmux_command();
        command
            .args(["set-option", "-t", target, "status", TMUX_STATUS])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "set-option status")
    }

    fn has_session(&self, session_name: &str) -> PtyResult<bool> {
        let status = self
            .tmux_command()
            .args(["has-session", "-t", session_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(PtyError::from)?;
        Ok(status.success())
    }

    fn kill_session_if_present(&self, session_name: &str) -> PtyResult<()> {
        if !self.has_session(session_name)? {
            return Ok(());
        }
        let mut command = self.tmux_command();
        command
            .args(["kill-session", "-t", session_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "kill-session")
    }

    fn clear_history_for_target(&self, target: &str) -> PtyResult<()> {
        if !self.has_session(target)? {
            return Ok(());
        }
        let mut command = self.tmux_command();
        command
            .args(["clear-history", "-t", target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "clear-history")
    }

    fn client_name_for_pid(
        &self,
        session_name: &str,
        client_pid: u32,
    ) -> PtyResult<Option<String>> {
        if !self.has_session(session_name)? {
            return Ok(None);
        }
        let output = self
            .tmux_command()
            .args([
                "list-clients",
                "-t",
                session_name,
                "-F",
                "#{client_pid}\t#{client_name}",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(PtyError::from)?;
        if !output.status.success() {
            return Err(PtyError::Backend(format!(
                "tmux list-clients failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((raw_pid, client_name)) = line.split_once('\t') else {
                continue;
            };
            if raw_pid.trim().parse::<u32>().ok() == Some(client_pid) && !client_name.is_empty() {
                return Ok(Some(client_name.to_owned()));
            }
        }
        Ok(None)
    }

    fn detach_client_pid_if_present(&self, session_name: &str, client_pid: u32) -> PtyResult<()> {
        let Some(client_name) = self.client_name_for_pid(session_name, client_pid)? else {
            return Ok(());
        };
        let mut command = self.tmux_command();
        command
            .args(["detach-client", "-t", &client_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "detach-client")
    }

    fn resize_session(&self, session_name: &str, size: PtySize) -> PtyResult<()> {
        let mut command = self.tmux_command();
        command
            .args([
                "resize-window",
                "-t",
                session_name,
                "-x",
                &size.cols.to_string(),
                "-y",
                &size.rows.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_tmux_status(command, "resize-window")
    }

    fn pane_current_path(&self, session_name: &str) -> Option<PathBuf> {
        let output = self
            .tmux_command()
            .args([
                "display-message",
                "-pt",
                session_name,
                "#{pane_current_path}",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!raw.is_empty()).then(|| PathBuf::from(raw))
    }

    fn pane_process_id(&self, session_name: &str) -> Option<u32> {
        let output = self
            .tmux_command()
            .args(["display-message", "-pt", session_name, "#{pane_pid}"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .ok()
    }

    fn pane_dead_status(&self, session_name: &str) -> PtyResult<Option<PtyExitStatus>> {
        if !self.has_session(session_name)? {
            return Ok(Some(PtyExitStatus::exited(0)));
        }
        let output = self
            .tmux_command()
            .args([
                "display-message",
                "-pt",
                session_name,
                "#{pane_dead}\t#{pane_dead_status}",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(PtyError::from)?;
        if !output.status.success() {
            return Err(PtyError::Backend(format!(
                "tmux display-message pane_dead failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let Some((dead, status)) = raw.trim().split_once('\t') else {
            return Ok(None);
        };
        if dead != "1" {
            return Ok(None);
        }
        let exit_code = status.trim().parse::<u32>().unwrap_or(0);
        Ok(Some(PtyExitStatus::exited(exit_code)))
    }

    fn capture_pane(&self, session_name: &str) -> PtyResult<Vec<u8>> {
        let history_start = format!("-{TMUX_HISTORY_LIMIT_LINES}");
        let output = self
            .tmux_command()
            .args([
                "capture-pane",
                "-p",
                "-e",
                "-J",
                "-S",
                &history_start,
                "-t",
                session_name,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(PtyError::from)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(PtyError::Backend(format!(
            "tmux capture-pane failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

impl Default for TmuxPtyBackend {
    fn default() -> Self {
        Self::for_state_path("daemon-state.json")
    }
}

impl PtyBackend for TmuxPtyBackend {
    fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let session_id = format!("tmux-session-{}-{nanos}", std::process::id());
        self.spawn_named(&session_id, command, size)
    }

    fn spawn_named(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        let session_name = Self::session_name_for_id(session_id);
        self.spawn_tmux_session(&session_name, command, size)?;
        match self.attach_tmux_client(&session_name, size) {
            Ok(attach_client) => Ok(Box::new(TmuxPtySession {
                backend: self.clone(),
                session_name,
                attach_client,
                size,
                next_terminal_seq: 0,
                clear_history_scan_tail: Vec::new(),
            })),
            Err(error) => {
                let _ = self.kill_session_if_present(&session_name);
                Err(error)
            }
        }
    }

    fn reconnect(
        &self,
        _session_id: &str,
        restore_info: &PtyRestoreInfo,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        match restore_info {
            PtyRestoreInfo::Tmux {
                socket_path,
                session_name,
            } => {
                let backend = Self {
                    tmux_path: self.tmux_path.clone(),
                    socket_path: socket_path.clone(),
                };
                if !backend.has_session(session_name)? {
                    return Err(PtyError::Backend(format!(
                        "tmux session {session_name} is not running"
                    )));
                }
                let attach_client = backend.attach_tmux_client(session_name, size)?;
                backend.resize_session(session_name, size)?;
                Ok(Box::new(TmuxPtySession {
                    backend,
                    session_name: session_name.clone(),
                    attach_client,
                    size,
                    next_terminal_seq: 0,
                    clear_history_scan_tail: Vec::new(),
                }))
            }
            PtyRestoreInfo::UnixSocket { .. } => Err(PtyError::Backend(
                "tmux backend cannot reconnect unix socket supervisor sessions".to_owned(),
            )),
        }
    }

    fn attach_client(
        &self,
        _session_id: &str,
        restore_info: Option<&PtyRestoreInfo>,
        _size: PtySize,
        attachment_id: &str,
    ) -> PtyResult<Box<dyn PtyAttachment>> {
        match restore_info {
            Some(PtyRestoreInfo::Tmux {
                socket_path,
                session_name,
            }) => {
                let backend = Self {
                    tmux_path: self.tmux_path.clone(),
                    socket_path: socket_path.clone(),
                };
                backend.attach_control_client(session_name, attachment_id)
            }
            Some(PtyRestoreInfo::UnixSocket { .. }) => Err(PtyError::Backend(
                "tmux backend cannot attach clients to unix socket supervisor sessions".to_owned(),
            )),
            None => Err(PtyError::Backend(
                "tmux attachment requires tmux restore metadata".to_owned(),
            )),
        }
    }
}

struct TmuxPtyAttachment {
    backend: TmuxPtyBackend,
    session_name: String,
    client_pid: u32,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    detached: bool,
}

impl TmuxPtyAttachment {
    fn detach_inner(&mut self) -> PtyResult<()> {
        let detach_result = if self.detached {
            Ok(())
        } else {
            self.detached = true;
            self.backend
                .detach_client_pid_if_present(&self.session_name, self.client_pid)
        };
        self.stop_child();
        detach_result
    }

    fn stop_child(&mut self) {
        // 中文注释：stdin 保持打开时 control-mode tmux client 会继续存活；释放 handle
        // 时先关闭 stdin，再等待 detach-client 让子进程自然退出，最后才 kill 兜底。
        drop(self.stdin.take());
        if let Some(child) = self.child.as_mut() {
            stop_tmux_attachment_child(child);
        }
        self.child.take();
        join_tmux_attachment_drain(self.stdout_thread.take());
        join_tmux_attachment_drain(self.stderr_thread.take());
    }
}

impl PtyAttachment for TmuxPtyAttachment {
    fn detach(&mut self) -> PtyResult<()> {
        self.detach_inner()
    }
}

impl Drop for TmuxPtyAttachment {
    fn drop(&mut self) {
        let _ = self.detach_inner();
    }
}

struct TmuxPtySession {
    backend: TmuxPtyBackend,
    session_name: String,
    attach_client: Box<dyn PtySession>,
    size: PtySize,
    next_terminal_seq: u64,
    clear_history_scan_tail: Vec<u8>,
}

impl Drop for TmuxPtySession {
    fn drop(&mut self) {
        // 中文注释：daemon 重启或 runtime 丢弃 attach bridge 时，只能 detach 当前 bridge
        // 对应的 tmux client，不能用 session 级 detach 误踢掉后续 per-Web attach client。
        if let Some(client_pid) = self.attach_client.process_id() {
            let _ = self
                .backend
                .detach_client_pid_if_present(&self.session_name, client_pid);
        }
    }
}

impl PtySession for TmuxPtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        let read = self.attach_client.read(buffer)?;
        if read > 0 {
            self.observe_output_for_history_clear(&buffer[..read]);
        }
        Ok(read)
    }

    fn output_signal(&self) -> Option<watch::Receiver<u64>> {
        self.attach_client.output_signal()
    }

    fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
        self.attach_client.write_all(bytes)
    }

    fn resize(&mut self, size: PtySize) -> PtyResult<()> {
        self.attach_client.resize(size)?;
        self.backend.resize_session(&self.session_name, size)?;
        self.size = size;
        Ok(())
    }

    fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
        Ok(PtySnapshot {
            size: self.size,
            process_id: self
                .backend
                .pane_process_id(&self.session_name)
                .or_else(|| self.attach_client.process_id()),
            retained_output: self.backend.capture_pane(&self.session_name)?,
        })
    }

    fn terminal_snapshot(
        &mut self,
        last_terminal_seq: Option<u64>,
    ) -> PtyResult<Vec<PtyTerminalFrame>> {
        let _ = last_terminal_seq;
        let snapshot = self.snapshot()?;
        Ok(vec![PtyTerminalFrame::Snapshot {
            base_seq: self.next_terminal_seq,
            size: snapshot.size,
            data: snapshot.retained_output,
        }])
    }

    fn read_terminal_frame(&mut self) -> PtyResult<Option<PtyTerminalFrame>> {
        let mut buffer = vec![0_u8; 16 * 1024];
        let read = self.read(&mut buffer)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        self.next_terminal_seq = self.next_terminal_seq.wrapping_add(1);
        Ok(Some(PtyTerminalFrame::Output {
            terminal_seq: self.next_terminal_seq,
            data: buffer,
        }))
    }

    fn ping(&mut self) -> PtyResult<()> {
        if let Some(status) = self.backend.pane_dead_status(&self.session_name)? {
            Err(PtyError::Backend(format!(
                "tmux session {} exited: {status:?}",
                self.session_name
            )))
        } else if self.backend.has_session(&self.session_name)? {
            Ok(())
        } else {
            Err(PtyError::Backend(format!(
                "tmux session {} is not running",
                self.session_name
            )))
        }
    }

    fn restore_info(&self) -> Option<PtyRestoreInfo> {
        Some(PtyRestoreInfo::Tmux {
            socket_path: self.backend.socket_path.clone(),
            session_name: self.session_name.clone(),
        })
    }

    fn terminate(&mut self) -> PtyResult<()> {
        self.backend.kill_session_if_present(&self.session_name)?;
        if self.attach_client.try_wait()?.is_none() {
            let _ = self.attach_client.terminate();
        }
        Ok(())
    }

    fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>> {
        if let Some(status) = self.backend.pane_dead_status(&self.session_name)? {
            return Ok(Some(status));
        }
        if self.backend.has_session(&self.session_name)? {
            Ok(None)
        } else {
            Ok(Some(PtyExitStatus::exited(0)))
        }
    }

    fn wait(&mut self) -> PtyResult<PtyExitStatus> {
        loop {
            if let Some(status) = self.backend.pane_dead_status(&self.session_name)? {
                return Ok(status);
            }
            if !self.backend.has_session(&self.session_name)? {
                return Ok(PtyExitStatus::exited(0));
            }
            thread::sleep(TMUX_WAIT_POLL_INTERVAL);
        }
    }

    fn process_id(&self) -> Option<u32> {
        self.attach_client.process_id()
    }

    fn current_working_directory(&self) -> Option<PathBuf> {
        self.backend.pane_current_path(&self.session_name)
    }
}

impl TmuxPtySession {
    fn observe_output_for_history_clear(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let prefix_len = self.clear_history_scan_tail.len();
        let mut combined = Vec::with_capacity(prefix_len.saturating_add(bytes.len()));
        combined.extend_from_slice(&self.clear_history_scan_tail);
        combined.extend_from_slice(bytes);
        if tmux_output_requests_history_clear(&combined, prefix_len) {
            if let Err(error) = self.backend.clear_history_for_target(&self.session_name) {
                tracing::debug!(
                    session_name = %self.session_name,
                    %error,
                    "tmux clear-history after terminal clear sequence failed"
                );
            }
        }
        let keep = combined.len().min(TMUX_CLEAR_HISTORY_SCAN_TAIL_MAX_BYTES);
        self.clear_history_scan_tail = combined.split_off(combined.len().saturating_sub(keep));
    }
}

fn tmux_socket_path_for_state_path(state_path: &Path) -> PathBuf {
    let socket_name = state_path
        .file_stem()
        .map(|stem| sanitize_tmux_socket_component(&stem.to_string_lossy()))
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "termd".to_owned());
    let socket_path = state_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{socket_name}.tmux.sock"));
    absolute_path_without_canonicalize(&socket_path)
}

fn absolute_path_without_canonicalize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    // 中文注释：tmux attach bridge 是一个独立 PTY 子进程。socket/config 必须是绝对路径，
    // 否则子进程 cwd 与 daemon cwd 不一致时会连到另一个同名 socket。
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

fn sanitize_tmux_socket_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn ensure_socket_parent(socket_path: &Path) -> PtyResult<()> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(PtyError::from)?;
    }
    Ok(())
}

fn remove_stale_socket_if_needed(tmux_path: &Path, socket_path: &Path) -> PtyResult<()> {
    if !socket_path.exists() || tmux_server_responds(tmux_path, socket_path)? {
        return Ok(());
    }
    fs::remove_file(socket_path).map_err(PtyError::from)
}

fn tmux_server_responds(tmux_path: &Path, socket_path: &Path) -> PtyResult<bool> {
    let status = ProcessCommand::new(tmux_path)
        .arg("-S")
        .arg(socket_path)
        .arg("list-sessions")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(PtyError::from)?;
    Ok(status.success())
}

fn ensure_tmux_attach_client_stayed_alive(
    attach_client: &mut dyn PtySession,
    session_name: &str,
) -> PtyResult<()> {
    for _ in 0..TMUX_ATTACH_STARTUP_CHECK_POLLS {
        if let Some(status) = attach_client.try_wait()? {
            let startup_output = drain_tmux_attach_startup_output(attach_client)?;
            return Err(PtyError::Backend(format!(
                "tmux attach bridge for session {session_name} exited during startup: {status:?}{startup_output}"
            )));
        }
        thread::sleep(TMUX_ATTACH_STARTUP_CHECK_INTERVAL);
    }
    Ok(())
}

fn drain_tmux_attach_startup_output(attach_client: &mut dyn PtySession) -> PtyResult<String> {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = attach_client.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        collected.extend_from_slice(&buffer[..read]);
        if collected.len() >= 16 * 1024 {
            break;
        }
    }
    if collected.is_empty() {
        return Ok(String::new());
    }
    // 中文注释：tmux 把 attach 失败原因写到 PTY 本身，不 drain 的话现场日志只有 exit code。
    let lossy = String::from_utf8_lossy(&collected);
    let sanitized = lossy.replace(['\r', '\n'], " ").trim().to_owned();
    if sanitized.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("; startup output: {sanitized}"))
    }
}

fn spawn_tmux_attachment_drain<R>(
    attachment_id: &str,
    stream_name: &'static str,
    mut reader: R,
) -> PtyResult<JoinHandle<()>>
where
    R: Read + Send + 'static,
{
    let thread_name = format!("termd-tmux-attachment-{stream_name}");
    let attachment_label = attachment_id.to_owned();
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::debug!(
                            %attachment_label,
                            %stream_name,
                            %error,
                            "tmux attachment drain stopped"
                        );
                        break;
                    }
                }
            }
        })
        .map_err(PtyError::backend)
}

fn stop_tmux_attachment_child(child: &mut Child) {
    for _ in 0..TMUX_ATTACHMENT_STOP_POLLS {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(TMUX_ATTACHMENT_STOP_POLL_INTERVAL),
            Err(_) => break,
        }
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn join_tmux_attachment_drain(thread: Option<JoinHandle<()>>) {
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

fn run_tmux_status(mut command: ProcessCommand, action: &'static str) -> PtyResult<()> {
    let output = command.output().map_err(PtyError::from)?;
    if output.status.success() {
        return Ok(());
    }
    Err(PtyError::Backend(format!(
        "tmux {action} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn tmux_output_requests_history_clear(bytes: &[u8], prefix_len: usize) -> bool {
    const PATTERNS: [&[u8]; 3] = [b"\x1b[3J", b"\x1b[H\x1b[2J", b"\x1b[1;1H\x1b[2J"];

    PATTERNS.iter().any(|pattern| {
        if bytes.len() < pattern.len() {
            return false;
        }
        let overlap_start = prefix_len.saturating_sub(pattern.len().saturating_sub(1));
        bytes
            .windows(pattern.len())
            .enumerate()
            .any(|(index, window)| index >= overlap_start && window == *pattern)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_bridge_command_overrides_unsafe_inherited_tmux_environment() {
        let backend = TmuxPtyBackend::with_socket_path("/tmp/termd-test.sock");
        let command = backend.attach_tmux_command("termd-test-session");

        assert_eq!(
            command.env_map().get("TERM").map(String::as_str),
            Some(TMUX_BRIDGE_TERM),
            "tmux attach bridge 不能继承 daemon 的 TERM=dumb"
        );
        for key in TMUX_BRIDGE_ENV_REMOVE_KEYS {
            assert!(
                command.removed_env().contains(*key),
                "tmux attach bridge 必须清理继承的 {key}"
            );
        }
    }

    #[test]
    fn relative_state_path_uses_absolute_tmux_socket_path() {
        let socket_path = tmux_socket_path_for_state_path(Path::new("daemon-state.json"));

        assert!(
            socket_path.is_absolute(),
            "tmux bridge 子进程不能依赖相对 socket 路径"
        );
        assert_eq!(
            socket_path.file_name().and_then(|name| name.to_str()),
            Some("daemon-state.tmux.sock")
        );
    }

    #[test]
    fn clear_history_detector_matches_clear_and_clear_scrollback_sequences() {
        assert!(tmux_output_requests_history_clear(b"\x1b[H\x1b[2J", 0));
        assert!(tmux_output_requests_history_clear(b"\x1b[1;1H\x1b[2J", 0));
        assert!(tmux_output_requests_history_clear(b"\x1b[3J", 0));
        assert!(!tmux_output_requests_history_clear(b"\x1b[2J", 0));
        assert!(!tmux_output_requests_history_clear(b"\x1b[2J\x1b[H", 0));
    }

    #[test]
    fn clear_history_detector_matches_sequences_split_across_reads_once_new_bytes_arrive() {
        let prefix = b"\x1b[H\x1b[";
        let suffix = b"2Jafter-clear";
        let mut combined = prefix.to_vec();
        combined.extend_from_slice(suffix);

        assert!(
            tmux_output_requests_history_clear(&combined, prefix.len()),
            "跨 read 的 clear 序列也必须触发 tmux clear-history"
        );
    }
}
