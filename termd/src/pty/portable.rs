//! `portable-pty` 后端适配。
//!
//! 本文件是唯一直接接触 `portable-pty` crate 的地方。上层 session/control 代码应该只依赖
//! `PtyBackend` 和 `PtySession`，这样认证、控制权、relay 和协议逻辑不会和平台 PTY 细节耦合。

use std::io::{Read, Write};

use portable_pty::{
    Child, CommandBuilder, MasterPty, PtySize as PortablePtySize, native_pty_system,
};

use super::{
    CommandSpec, PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize, PtySnapshot,
};

impl From<PtySize> for PortablePtySize {
    fn from(size: PtySize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        }
    }
}

impl From<portable_pty::ExitStatus> for PtyExitStatus {
    fn from(status: portable_pty::ExitStatus) -> Self {
        match status.signal() {
            Some(signal) => Self::signaled(signal.to_string()),
            None => Self::exited(status.exit_code()),
        }
    }
}

impl CommandSpec {
    /// 转换为 `portable-pty` 的命令构造器。
    ///
    /// 该转换只搬运 program/args/env/cwd，不补默认 shell，也不做鉴权或控制权判断。
    pub(crate) fn to_portable_command(&self) -> PtyResult<CommandBuilder> {
        self.validate()?;

        let mut command = CommandBuilder::new(self.program());
        command.args(self.args_slice().iter().map(String::as_str));

        for (key, value) in self.env_map() {
            command.env(key, value);
        }

        if let Some(cwd) = self.cwd_path() {
            command.cwd(cwd.as_os_str());
        }

        Ok(command)
    }
}

/// 使用系统原生 PTY 实现的后端。
#[derive(Debug, Default)]
pub struct PortablePtyBackend;

impl PortablePtyBackend {
    /// 创建 portable-pty 后端实例。
    pub fn new() -> Self {
        Self
    }
}

impl PtyBackend for PortablePtyBackend {
    fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size.into()).map_err(PtyError::backend)?;

        let portable_pty::PtyPair { master, slave } = pair;
        let child = slave
            .spawn_command(command.to_portable_command()?)
            .map_err(PtyError::backend)?;

        // 子进程已经持有 slave 端；daemon 只保留 master 端用于读写和 resize。
        drop(slave);

        let reader = master.try_clone_reader().map_err(PtyError::backend)?;
        let writer = master.take_writer().map_err(PtyError::backend)?;

        Ok(Box::new(PortablePtySession {
            master,
            child,
            reader,
            writer,
            size,
            history: Vec::new(),
        }))
    }
}

/// `portable-pty` session 句柄。
///
/// master 用于 resize，reader/writer 用于 daemon I/O 桥接，child 用于终止和等待退出。
pub struct PortablePtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    size: PtySize,
    history: Vec<u8>,
}

impl PtySession for PortablePtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        let read = self.reader.read(buffer).map_err(PtyError::from)?;
        if read > 0 {
            self.history.extend_from_slice(&buffer[..read]);
        }
        Ok(read)
    }

    fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, size: PtySize) -> PtyResult<()> {
        self.master.resize(size.into()).map_err(PtyError::backend)?;
        self.size = size;
        Ok(())
    }

    fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
        Ok(PtySnapshot {
            size: self.size,
            process_id: self.process_id(),
            retained_output: self.history.clone(),
        })
    }

    fn terminate(&mut self) -> PtyResult<()> {
        self.child.kill().map_err(Into::into)
    }

    fn try_wait(&mut self) -> PtyResult<Option<PtyExitStatus>> {
        self.child
            .try_wait()
            .map(|status| status.map(Into::into))
            .map_err(Into::into)
    }

    fn wait(&mut self) -> PtyResult<PtyExitStatus> {
        self.child.wait().map(Into::into).map_err(Into::into)
    }

    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}
