//! daemon WebSocket 服务专用的 PTY 输出桥接。
//!
//! `portable-pty` 的 reader 是阻塞 `Read`。如果 server 在 async WebSocket 循环里直接
//! 调用它，某个没有输出的 session 会卡住整个连接处理。这里把真实 reader 放到后台线程，
//! 对 runtime 暴露的 `read` 只消费已经缓存好的输出；没有缓存时立即返回 `0`。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

use portable_pty::{Child, MasterPty, native_pty_system};
use tokio::sync::watch;

use crate::pty::{
    CommandSpec, PtyBackend, PtyError, PtyExitStatus, PtyResult, PtySession, PtySize, PtySnapshot,
};

const READER_CHUNK_BYTES: usize = 16 * 1024;
const READY_OUTPUT_DRAIN_MAX_CHUNKS: usize = 64;
const READY_OUTPUT_DRAIN_MAX_BYTES: usize = 1024 * 1024;
const OUTPUT_QUEUE_MAX_MESSAGES: usize = 128;
const OUTPUT_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 生产 daemon 使用的 PTY backend。
///
/// 它仍然只实现 `PtyBackend`，所以不会把 WebSocket 或 auth 逻辑下沉到 PTY 层；
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
        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_QUEUE_MAX_MESSAGES);
        let (output_signal_tx, output_signal_rx) = watch::channel(0_u64);

        // 真实 PTY read 会阻塞，所以只能在专门线程中执行。WebSocket 线程只读 channel 缓存。
        let reader_signal_tx = output_signal_tx.clone();
        let reader_thread = thread::Builder::new()
            .name("termd-pty-output-reader".to_owned())
            .spawn(move || read_pty_output(reader, output_tx, reader_signal_tx))
            .map_err(PtyError::backend)?;

        Ok(Box::new(NonBlockingPortablePtySession {
            master,
            child,
            writer,
            output_rx,
            output_signal_tx: output_signal_tx.clone(),
            output_signal_rx,
            pending_output: VecDeque::new(),
            pending_output_bytes: 0,
            pending_error: None,
            _reader_thread: reader_thread,
            size,
        }))
    }
}

type OutputMessage = PtyResult<Vec<u8>>;

fn read_pty_output(
    mut reader: Box<dyn Read + Send>,
    output_tx: SyncSender<OutputMessage>,
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
                // 信号只表示“有输出可读”，不携带终端内容。上层通过 terminal WebSocket
                // frame 发送 PTY 数据。
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
    output_signal_tx: watch::Sender<u64>,
    output_signal_rx: watch::Receiver<u64>,
    pending_output: VecDeque<Vec<u8>>,
    pending_output_bytes: usize,
    pending_error: Option<PtyError>,
    _reader_thread: JoinHandle<()>,
    size: PtySize,
}

impl NonBlockingPortablePtySession {
    fn drain_ready_output(&mut self) -> PtyResult<()> {
        if self.pending_error.is_some() {
            return Ok(());
        }
        let mut chunks = 0_usize;
        let mut bytes = 0_usize;
        loop {
            if chunks >= READY_OUTPUT_DRAIN_MAX_CHUNKS || bytes >= READY_OUTPUT_DRAIN_MAX_BYTES {
                // 中文注释：这里仍在 supervisor 的 session 锁内。持续刷屏时不能把
                // mpsc backlog 一次搬空，否则 input/resize/snapshot 会等这个同步函数。
                return Ok(());
            }
            if self.pending_output.len() >= OUTPUT_QUEUE_MAX_MESSAGES
                || self.pending_output_bytes > OUTPUT_QUEUE_MAX_BYTES - READER_CHUNK_BYTES
            {
                return Ok(());
            }
            match self.output_rx.try_recv() {
                Ok(Ok(chunk)) if !chunk.is_empty() => {
                    bytes = bytes.saturating_add(chunk.len());
                    chunks = chunks.saturating_add(1);
                    self.pending_output_bytes =
                        self.pending_output_bytes.saturating_add(chunk.len());
                    self.pending_output.push_back(chunk);
                }
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => {
                    self.pending_error = Some(error);
                    return Ok(());
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn signal_pending_output(&self) {
        let next_sequence = self.output_signal_tx.borrow().wrapping_add(1);
        // 这个信号只表示“本地缓存里还有输出没被 WebSocket 层取走”。
        // watcher 使用 watch，会自动合并过密通知；这里允许忽略没有接收者的情况。
        let _ = self.output_signal_tx.send(next_sequence);
    }
}

impl PtySession for NonBlockingPortablePtySession {
    fn read(&mut self, buffer: &mut [u8]) -> PtyResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        self.drain_ready_output()?;

        let Some(mut chunk) = self.pending_output.pop_front() else {
            return match self.pending_error.take() {
                Some(error) => Err(error),
                None => Ok(0),
            };
        };
        self.pending_output_bytes = self.pending_output_bytes.saturating_sub(chunk.len());
        let read = chunk.len().min(buffer.len());
        buffer[..read].copy_from_slice(&chunk[..read]);

        if read < chunk.len() {
            let remaining = chunk.split_off(read);
            self.pending_output_bytes = self.pending_output_bytes.saturating_add(remaining.len());
            self.pending_output.push_front(remaining);
        }

        if !self.pending_output.is_empty() || self.pending_error.is_some() {
            // watch 信号可能把多个 PTY read 合并成一次唤醒；如果本次 read 后仍有缓存，
            // 主动再唤醒一次 WebSocket 推送路径，避免画面停住直到下一次用户输入。
            self.signal_pending_output();
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
        self.master.resize(size.into()).map_err(PtyError::backend)?;
        self.size = size;
        Ok(())
    }

    fn snapshot(&mut self) -> PtyResult<PtySnapshot> {
        self.drain_ready_output()?;
        if self.pending_output.is_empty()
            && let Some(error) = self.pending_error.take()
        {
            return Err(error);
        }
        let retained_output = self.pending_output.iter().flatten().copied().collect();

        Ok(PtySnapshot {
            size: self.size,
            process_id: self.process_id(),
            retained_output,
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

    fn current_working_directory(&self) -> Option<std::path::PathBuf> {
        self.process_id()
            .and_then(crate::pty::current_working_directory_for_pid)
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

    #[test]
    #[cfg(unix)]
    fn queued_output_is_read_before_deferred_reader_error() {
        let size = PtySize::new(24, 80);
        let pair = native_pty_system()
            .openpty(size.into())
            .expect("test PTY should open");
        let portable_pty::PtyPair { master, slave } = pair;
        let child = slave
            .spawn_command(
                CommandSpec::new("sh")
                    .args(["-c", "sleep 1"])
                    .to_portable_command()
                    .expect("test command should convert"),
            )
            .expect("test child should spawn");
        drop(slave);
        let writer = master.take_writer().expect("test writer should open");
        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_QUEUE_MAX_MESSAGES);
        output_tx
            .send(Ok(b"before-error".to_vec()))
            .expect("test chunk should queue");
        output_tx
            .send(Err(PtyError::from(std::io::Error::other(
                "test reader failure",
            ))))
            .expect("test error should queue");
        drop(output_tx);
        let (output_signal_tx, output_signal_rx) = watch::channel(0_u64);
        let reader_thread = thread::spawn(|| {});
        let mut session = NonBlockingPortablePtySession {
            master,
            child,
            writer,
            output_rx,
            output_signal_tx,
            output_signal_rx,
            pending_output: VecDeque::new(),
            pending_output_bytes: 0,
            pending_error: None,
            _reader_thread: reader_thread,
            size,
        };
        let mut buffer = [0_u8; 64];

        let read = session
            .read(&mut buffer)
            .expect("queued output must be returned before the terminal reader error");

        assert_eq!(&buffer[..read], b"before-error");
        let error = session
            .read(&mut buffer)
            .expect_err("reader error should surface after queued output is drained");
        assert!(error.to_string().contains("test reader failure"));
        assert_eq!(session.pending_output_bytes, 0);
        let _ = session.terminate();
    }

    #[test]
    #[cfg(unix)]
    fn read_rearms_output_signal_when_cached_output_remains() {
        let backend = NonBlockingPortablePtyBackend::new();
        let mut session = backend
            .spawn(
                &CommandSpec::new("sh").args(["-c", "head -c 40000 /dev/zero | tr '\\000' x"]),
                PtySize::new(24, 80),
            )
            .expect("test PTY should spawn");
        let mut signal = session
            .output_signal()
            .expect("nonblocking PTY should expose output signal");
        let deadline = Instant::now() + Duration::from_secs(2);

        loop {
            let snapshot = session.snapshot().expect("snapshot should not fail");
            if snapshot.retained_output.len() > READER_CHUNK_BYTES {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for cached PTY output"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        signal.borrow_and_update();
        let mut buffer = vec![0_u8; READER_CHUNK_BYTES];
        let read = session.read(&mut buffer).expect("read should not fail");

        assert!(read > 0);
        assert!(
            signal.has_changed().unwrap_or(false),
            "remaining cached output should rearm the watcher"
        );

        let _ = session.terminate();
    }

    #[test]
    #[cfg(unix)]
    fn slow_consumer_retained_output_stays_within_message_and_byte_budget() {
        const TEST_OUTPUT_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

        let backend = NonBlockingPortablePtyBackend::new();
        let mut session = backend
            .spawn(
                &CommandSpec::new("sh").args(["-c", "head -c 20971520 /dev/zero | tr '\\000' x"]),
                PtySize::new(24, 80),
            )
            .expect("test PTY should spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut largest_retained = 0_usize;

        loop {
            let snapshot = session.snapshot().expect("snapshot should not fail");
            largest_retained = largest_retained.max(snapshot.retained_output.len());
            if largest_retained > TEST_OUTPUT_QUEUE_MAX_BYTES || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            largest_retained <= TEST_OUTPUT_QUEUE_MAX_BYTES,
            "slow consumer retained {largest_retained} bytes"
        );
        let _ = session.terminate();
    }
}
