//! daemon WebSocket 服务专用的 PTY 输出桥接。
//!
//! `portable-pty` 的 reader 是阻塞 `Read`。如果 server 在 async WebSocket 循环里直接
//! 调用它，某个没有输出的 session 会卡住整个连接处理。这里把真实 reader 放到后台线程，
//! 对 runtime 暴露的 `read` 只消费已经缓存好的输出；没有缓存时立即返回 `0`。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

use portable_pty::{Child, MasterPty, native_pty_system};
use tokio::sync::watch;

use crate::pty::{
    CommandSpec, PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize,
};

const READER_CHUNK_BYTES: usize = 16 * 1024;

/// 生产 daemon 使用的 PTY backend。
///
/// 它仍然只实现 `PtyBackend`，所以不会把 WebSocket、auth 或 E2EE 逻辑下沉到 PTY 层；
/// 唯一差异是输出读取由后台线程转成非阻塞缓存读取。
#[derive(Debug, Default)]
pub struct NonBlockingPortablePtyBackend;

impl NonBlockingPortablePtyBackend {
    pub fn new() -> Self {
        Self
    }
}

impl PtyBackend for NonBlockingPortablePtyBackend {
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
        let (output_tx, output_rx) = mpsc::channel();
        let (output_signal_tx, output_signal_rx) = watch::channel(0_u64);

        // 真实 PTY read 会阻塞，所以只能在专门线程中执行。WebSocket 线程只读 channel 缓存。
        let reader_thread = thread::Builder::new()
            .name("termd-pty-output-reader".to_owned())
            .spawn(move || read_pty_output(reader, output_tx, output_signal_tx))
            .map_err(PtyError::backend)?;

        Ok(Box::new(NonBlockingPortablePtySession {
            master,
            child,
            writer,
            output_rx,
            output_signal_rx,
            pending_output: VecDeque::new(),
            _reader_thread: reader_thread,
        }))
    }
}

type OutputMessage = PtyResult<Vec<u8>>;

fn read_pty_output(
    mut reader: Box<dyn Read + Send>,
    output_tx: mpsc::Sender<OutputMessage>,
    output_signal_tx: watch::Sender<u64>,
) {
    let mut buffer = vec![0_u8; READER_CHUNK_BYTES];
    let mut sequence = 0_u64;

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if output_tx.send(Ok(buffer[..read].to_vec())).is_err() {
                    break;
                }
                sequence = sequence.wrapping_add(1);
                // 信号只表示“有输出可读”，不携带终端明文；明文仍只经 E2EE session_data 发送。
                let _ = output_signal_tx.send(sequence);
            }
            Err(error) => {
                let _ = output_tx.send(Err(PtyError::from(error)));
                sequence = sequence.wrapping_add(1);
                let _ = output_signal_tx.send(sequence);
                break;
            }
        }
    }
}

/// reader 已经被后台线程消费；本对象的 `read` 只从 channel 中取已就绪输出。
struct NonBlockingPortablePtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<OutputMessage>,
    output_signal_rx: watch::Receiver<u64>,
    pending_output: VecDeque<Vec<u8>>,
    _reader_thread: JoinHandle<()>,
}

impl NonBlockingPortablePtySession {
    fn drain_ready_output(&mut self) -> PtyResult<()> {
        loop {
            match self.output_rx.try_recv() {
                Ok(Ok(chunk)) if !chunk.is_empty() => self.pending_output.push_back(chunk),
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => return Err(error),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }
}

impl PtySession for NonBlockingPortablePtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        self.drain_ready_output()?;

        let Some(mut chunk) = self.pending_output.pop_front() else {
            return Ok(0);
        };
        let read = chunk.len().min(buffer.len());
        buffer[..read].copy_from_slice(&chunk[..read]);

        if read < chunk.len() {
            let remaining = chunk.split_off(read);
            self.pending_output.push_front(remaining);
        }

        Ok(read)
    }

    fn output_signal(&self) -> Option<watch::Receiver<u64>> {
        Some(self.output_signal_rx.clone())
    }

    fn write_all(&mut self, bytes: &[u8]) -> PtyResult<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, size: PtySize) -> PtyResult<()> {
        self.master.resize(size.into()).map_err(PtyError::backend)
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    #[cfg(unix)]
    fn read_returns_immediately_when_no_output_is_ready() {
        let backend = NonBlockingPortablePtyBackend::new();
        let mut session = backend
            .spawn(
                &CommandSpec::new("sh").args(["-c", "sleep 1"]),
                PtySize::new(24, 80),
            )
            .expect("test PTY should spawn");
        let mut buffer = [0_u8; 64];

        let started = Instant::now();
        let read = session.read(&mut buffer).expect("read should not fail");

        assert_eq!(read, 0);
        assert!(started.elapsed() < Duration::from_millis(200));

        let _ = session.terminate();
    }

    #[test]
    #[cfg(unix)]
    fn output_is_delivered_through_cache() {
        let backend = NonBlockingPortablePtyBackend::new();
        let mut session = backend
            .spawn(
                &CommandSpec::new("sh").args(["-c", "printf termd-ready"]),
                PtySize::new(24, 80),
            )
            .expect("test PTY should spawn");
        let mut buffer = [0_u8; 64];
        let deadline = Instant::now() + Duration::from_secs(2);

        loop {
            let read = session.read(&mut buffer).expect("read should not fail");
            if read > 0 {
                assert_eq!(&buffer[..read], b"termd-ready");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for PTY output"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = session.terminate();
    }
}
