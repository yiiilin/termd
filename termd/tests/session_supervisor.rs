use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use sha2::{Digest, Sha256};
use termd::auth::current_unix_timestamp_millis;
use termd::config::DaemonConfig;
use termd::net::protocol::DaemonProtocol;
use termd::net::server::try_default_protocol;
use termd::net::signature::Ed25519SignatureVerifier;
use termd::pty::supervisor::SupervisorPtyBackend;
use termd::pty::{
    CommandSpec, PtyBackend, PtyError, PtyRestoreInfo, PtyResult, PtySession, PtySize,
    PtySupervisorStatus, PtyTerminalFrame,
};
use termd::runtime::SessionRuntime;
use termd::session::TerminalSize;
use termd::state::{DaemonIdentitySnapshot, DaemonState, SessionStateRecord, StateStore};
use termd_proto::{PublicKey, ServerId, SessionId, SessionState, UnixTimestampMillis};

fn temp_state_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "termd-session-supervisor-test-{}-{}-{name}",
        std::process::id(),
        nanos
    ))
}

#[derive(Clone)]
struct FailFirstReconnectBackend {
    inner: SupervisorPtyBackend,
    reconnect_attempts: Arc<AtomicUsize>,
}

impl FailFirstReconnectBackend {
    fn new(inner: SupervisorPtyBackend) -> Self {
        Self {
            inner,
            reconnect_attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reconnect_attempts(&self) -> usize {
        self.reconnect_attempts.load(Ordering::SeqCst)
    }
}

impl PtyBackend for FailFirstReconnectBackend {
    fn spawn(&self, command: &CommandSpec, size: PtySize) -> PtyResult<Box<dyn PtySession>> {
        self.inner.spawn(command, size)
    }

    fn spawn_named(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        self.inner.spawn_named(session_id, command, size)
    }

    fn expected_socket_path(&self, session_id: &str) -> PtyResult<Option<PathBuf>> {
        self.inner.expected_socket_path(session_id)
    }

    fn spawn_named_gated(
        &self,
        session_id: &str,
        command: &CommandSpec,
        size: PtySize,
        grant: &termd::pty::PtyStartupGrant,
        evidence_committed: &mut dyn FnMut(&PtyRestoreInfo) -> PtyResult<()>,
    ) -> PtyResult<Box<dyn PtySession>> {
        self.inner
            .spawn_named_gated(session_id, command, size, grant, evidence_committed)
    }

    fn reconcile_owned_cleanup(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        capability: &[u8],
        operation_id: u64,
    ) -> PtyResult<bool> {
        self.inner
            .reconcile_owned_cleanup(session_id, restore_info, capability, operation_id)
    }

    fn owned_natural_exit_status(&self, restore_info: &PtyRestoreInfo) -> PtyResult<bool> {
        self.inner.owned_natural_exit_status(restore_info)
    }

    fn install_legacy_cleanup_capability(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        capability: &[u8],
    ) -> PtyResult<bool> {
        self.inner
            .install_legacy_cleanup_capability(session_id, restore_info, capability)
    }

    fn reconcile_legacy_owned_cleanup(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
    ) -> PtyResult<bool> {
        self.inner
            .reconcile_legacy_owned_cleanup(session_id, restore_info)
    }

    fn reconnect(
        &self,
        session_id: &str,
        restore_info: &PtyRestoreInfo,
        size: PtySize,
    ) -> PtyResult<Box<dyn PtySession>> {
        if self.reconnect_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(PtyError::Backend(
                "injected transient supervisor reconnect failure".to_owned(),
            ));
        }
        self.inner.reconnect(session_id, restore_info, size)
    }
}

fn read_until_contains(
    runtime: &mut SessionRuntime<SupervisorPtyBackend>,
    session_id: &str,
    needle: &[u8],
) -> Vec<u8> {
    read_with(needle, |buffer| runtime.read_output(session_id, buffer))
}

fn read_session_until_contains(session: &mut dyn PtySession, needle: &[u8]) -> Vec<u8> {
    read_with(needle, |buffer| session.read(buffer))
}

fn read_terminal_frame_until_contains(
    runtime: &mut SessionRuntime<SupervisorPtyBackend>,
    session_id: &str,
    needle: &[u8],
) -> PtyTerminalFrame {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut skipped = Vec::new();

    while Instant::now() < deadline {
        if let Some(frame) = runtime.read_terminal_frame(session_id).unwrap() {
            if frame_contains(&frame, needle) {
                return frame;
            }
            skipped.push(format!("{frame:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!(
        "timed out waiting for terminal frame containing {:?}; skipped frames: {skipped:?}",
        String::from_utf8_lossy(needle)
    );
}

fn frame_contains(frame: &PtyTerminalFrame, needle: &[u8]) -> bool {
    match frame {
        PtyTerminalFrame::Snapshot { data, .. } | PtyTerminalFrame::Output { data, .. } => {
            data.windows(needle.len()).any(|window| window == needle)
        }
        PtyTerminalFrame::Resize { .. } | PtyTerminalFrame::Exit { .. } => false,
    }
}

fn snapshot_base_seq_containing(frames: &[PtyTerminalFrame], needle: &[u8]) -> u64 {
    frames
        .iter()
        .find_map(|frame| match frame {
            PtyTerminalFrame::Snapshot { base_seq, data, .. }
                if data.windows(needle.len()).any(|window| window == needle) =>
            {
                Some(*base_seq)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "expected snapshot containing {:?}, got {frames:?}",
                String::from_utf8_lossy(needle)
            )
        })
}

fn terminal_snapshot_until_contains(
    runtime: &mut SessionRuntime<SupervisorPtyBackend>,
    session_id: &str,
    last_terminal_seq: Option<u64>,
    needle: &[u8],
) -> Vec<PtyTerminalFrame> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_frames = Vec::new();

    while Instant::now() < deadline {
        let frames = runtime
            .terminal_snapshot(session_id, last_terminal_seq)
            .expect("AttachSync should return terminal frames");
        if frames.iter().any(|frame| frame_contains(frame, needle)) {
            return frames;
        }
        last_frames = frames;
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!(
        "timed out waiting for terminal snapshot containing {:?}; last frames: {last_frames:?}",
        String::from_utf8_lossy(needle)
    );
}

fn read_with<E>(needle: &[u8], mut read_once: impl FnMut(&mut [u8]) -> Result<usize, E>) -> Vec<u8>
where
    E: std::fmt::Debug,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 1024];

    while Instant::now() < deadline {
        let read = read_once(&mut buffer).unwrap();
        if read > 0 {
            collected.extend_from_slice(&buffer[..read]);
            if collected
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return collected;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!(
        "timed out waiting for {:?} in {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&collected)
    );
}

fn restore_supervisor_pid(restore_info: &PtyRestoreInfo) -> u32 {
    match restore_info {
        PtyRestoreInfo::UnixSocket { supervisor_pid, .. } => *supervisor_pid,
        PtyRestoreInfo::Tmux { .. } => {
            panic!("supervisor tests should only pass UnixSocket restore info")
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_process_state(pid: u32) -> Option<char> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("State:"))
        .and_then(|value| value.trim().chars().next())
}

#[cfg(target_os = "linux")]
fn linux_process_parent_pid(pid: u32) -> Option<u32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(target_os = "linux")]
fn wait_until_linux_process_is_reaped(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline {
        match linux_process_state(pid) {
            None => return,
            Some('Z') => panic!("supervisor pid {pid} became a zombie after close"),
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    panic!("timed out waiting for supervisor pid {pid} to exit after close");
}

#[cfg(target_os = "linux")]
struct TestSupervisorGuard {
    pid: u32,
    socket_path: PathBuf,
    ownership_confirmed: bool,
}

#[cfg(target_os = "linux")]
impl TestSupervisorGuard {
    fn new(pid: u32, socket_path: PathBuf) -> Self {
        let mut guard = Self {
            pid,
            socket_path,
            ownership_confirmed: false,
        };
        guard.ownership_confirmed = guard.owns_live_process();
        guard
    }

    fn owns_live_process(&self) -> bool {
        let Ok(relative) = self.socket_path.strip_prefix(std::env::temp_dir()) else {
            return false;
        };
        let Some(first) = relative.components().next() else {
            return false;
        };
        let std::path::Component::Normal(first) = first else {
            return false;
        };
        if !first
            .to_str()
            .is_some_and(|name| name.starts_with("termd-session-supervisor-test-"))
        {
            return false;
        }
        let Ok(cmdline) = fs::read(format!("/proc/{}/cmdline", self.pid)) else {
            return false;
        };
        let arguments: Vec<&[u8]> = cmdline
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .collect();
        arguments
            .iter()
            .any(|argument| *argument == b"__session-supervisor")
            && arguments.windows(2).any(|pair| {
                pair[0] == b"--socket-path" && pair[1] == self.socket_path.as_os_str().as_bytes()
            })
    }
}

#[cfg(target_os = "linux")]
impl Drop for TestSupervisorGuard {
    fn drop(&mut self) {
        if !self.ownership_confirmed {
            return;
        }
        if self.owns_live_process() {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut status = 0;
            let waited =
                unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
            if waited == self.pid as libc::pid_t || linux_process_state(self.pid).is_none() {
                break;
            }
            std::thread::yield_now();
        }
        if linux_process_state(self.pid).is_none() {
            let _ = fs::remove_file(&self.socket_path);
            let _ = fs::remove_file(self.socket_path.with_extension("attach.sock"));
        }
    }
}

struct TestStateDirectory {
    path: PathBuf,
}

impl TestStateDirectory {
    fn new(name: &str) -> Self {
        let path = temp_state_path(name);
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestStateDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn write_raw_supervisor_request(stream: &mut StdUnixStream, request: serde_json::Value) {
    let payload = serde_json::to_vec(&request).unwrap();
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .unwrap();
    stream.write_all(&payload).unwrap();
    stream.flush().unwrap();
}

#[cfg(unix)]
fn read_raw_supervisor_response(stream: &mut StdUnixStream) -> serde_json::Value {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut payload = vec![0_u8; u32::from_le_bytes(length) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

#[cfg(unix)]
fn connect_raw_supervisor(socket_path: &Path) -> StdUnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match StdUnixStream::connect(socket_path) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("failed to connect supervisor socket: {error}"),
        }
    }
}

fn test_cleanup_capability_base64() -> String {
    general_purpose::STANDARD.encode([0x5a_u8; 32])
}

fn test_cleanup_auth_proof(
    session_id: &str,
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
) -> [u8; 32] {
    let capability = [0x5a_u8; 32];
    let mut inner_key = [0x36_u8; 64];
    let mut outer_key = [0x5c_u8; 64];
    for (index, byte) in capability.iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(b"termd-supervisor-cleanup-v1\0");
    inner.update(session_id.as_bytes());
    inner.update([0]);
    inner.update(client_nonce);
    inner.update(server_nonce);
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn authenticate_raw_cleanup(stream: &mut StdUnixStream, session_id: &str, request_id: u64) {
    let client_nonce = [0x31_u8; 32];
    write_raw_supervisor_request(
        stream,
        serde_json::json!({
            "request_id": request_id,
            "request": {
                "type": "cleanup_auth_challenge",
                "session_id": session_id,
                "client_nonce_base64": general_purpose::STANDARD.encode(client_nonce)
            }
        }),
    );
    let challenge = read_raw_supervisor_response(stream);
    let server_nonce = general_purpose::STANDARD
        .decode(
            challenge["response"]["payload"]["server_nonce_base64"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
        .try_into()
        .unwrap();
    let server_proof = general_purpose::STANDARD
        .decode(
            challenge["response"]["payload"]["server_proof_base64"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        server_proof,
        test_cleanup_auth_proof(session_id, &client_nonce, &server_nonce)
    );
    write_raw_supervisor_request(
        stream,
        serde_json::json!({
            "request_id": request_id + 1,
            "request": {
                "type": "cleanup_authenticate",
                "session_id": session_id,
                "client_nonce_base64": general_purpose::STANDARD.encode(client_nonce),
                "server_nonce_base64": general_purpose::STANDARD.encode(server_nonce),
                "capability_base64": test_cleanup_capability_base64()
            }
        }),
    );
    assert_eq!(
        read_raw_supervisor_response(stream)["response"]["status"],
        "ok"
    );
}

fn persist_test_ownership(
    state_path: &Path,
    session_id: SessionId,
    restore_info: &PtyRestoreInfo,
    phase: &str,
    close_operation_id: Option<u64>,
) {
    let PtyRestoreInfo::UnixSocket {
        socket_path,
        supervisor_pid,
        ..
    } = restore_info
    else {
        panic!("test ownership fixture requires Unix supervisor evidence");
    };
    let sqlite_path = state_path.with_extension("sqlite");
    let state_db = rusqlite::Connection::open(sqlite_path).unwrap();
    state_db
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_ownership (
                session_id TEXT PRIMARY KEY NOT NULL,
                phase TEXT NOT NULL,
                create_operation_id BLOB NOT NULL,
                close_operation_id TEXT,
                capability BLOB NOT NULL,
                expected_socket TEXT,
                supervisor_pid INTEGER,
                socket_path TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                diagnostic TEXT,
                legacy_protocol INTEGER NOT NULL DEFAULT 0,
                owner_generation BLOB
            ) STRICT;",
        )
        .unwrap();
    let now_ms = current_unix_timestamp_millis().0 as i64;
    state_db
        .execute(
            "INSERT INTO session_ownership (
                session_id, phase, create_operation_id, close_operation_id, capability,
                expected_socket, supervisor_pid, socket_path, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, ?8)",
            rusqlite::params![
                session_id.0.to_string(),
                phase,
                [7_u8; 16].as_slice(),
                close_operation_id.map(|value| value.to_string()),
                [0x5a_u8; 32].as_slice(),
                socket_path.to_string_lossy(),
                i64::from(*supervisor_pid),
                now_ms,
            ],
        )
        .unwrap();
}

fn spawn_supervisor_from_short_lived_parent(
    binary_path: &Path,
    session_id: &str,
    socket_path: &Path,
    command: &CommandSpec,
    size: PtySize,
    pid_file: &Path,
) -> u32 {
    spawn_supervisor_from_short_lived_parent_with_capability(
        binary_path,
        session_id,
        socket_path,
        command,
        size,
        pid_file,
        Some(test_cleanup_capability_base64()),
    )
}

fn spawn_legacy_supervisor_from_short_lived_parent(
    _binary_path: &Path,
    session_id: &str,
    socket_path: &Path,
    command: &CommandSpec,
    size: PtySize,
    pid_file: &Path,
) -> u32 {
    spawn_supervisor_from_short_lived_parent_with_capability(
        real_064_supervisor_binary(),
        session_id,
        socket_path,
        command,
        size,
        pid_file,
        None,
    )
}

#[test]
#[cfg(target_os = "linux")]
fn test_supervisor_guard_reaps_owned_real_064_supervisor_on_early_exit() {
    let state_path = temp_state_path("guard-real-064.sock");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let session_id = SessionId::new().0.to_string();
    let supervisor_pid = spawn_legacy_supervisor_from_short_lived_parent(
        Path::new("unused"),
        &session_id,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );

    let guard = TestSupervisorGuard::new(supervisor_pid, socket_path.clone());
    drop(guard);

    wait_until_linux_process_is_reaped(supervisor_pid);
    assert!(!socket_path.exists());
    assert!(!socket_path.with_extension("attach.sock").exists());
    let _ = fs::remove_file(pid_file);
}

fn find_supervisor_pid_for_socket(socket_path: &str) -> Option<u32> {
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let arguments: Vec<&[u8]> = cmdline
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .collect();
        if arguments
            .iter()
            .any(|argument| *argument == b"__session-supervisor")
            && arguments
                .windows(2)
                .any(|pair| pair[0] == b"--socket-path" && pair[1] == socket_path.as_bytes())
        {
            return Some(pid);
        }
    }
    None
}

fn real_064_supervisor_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let root = std::env::temp_dir().join("termd-real-0ccd03f-build");
        let binary = root.join("target/debug/termd");
        if binary.is_file() {
            return binary;
        }
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("source.tar");
        let repository = ProcessCommand::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .expect("git rev-parse should run");
        assert!(repository.status.success());
        let repository = PathBuf::from(String::from_utf8(repository.stdout).unwrap().trim());
        let status = ProcessCommand::new("git")
            .args(["archive", "--format=tar", "-o"])
            .arg(&archive)
            .arg("0ccd03f")
            .current_dir(repository)
            .status()
            .expect("git archive should run");
        assert!(status.success(), "git archive 0ccd03f failed");
        let status = ProcessCommand::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&root)
            .status()
            .expect("tar should extract baseline");
        assert!(status.success(), "baseline archive extraction failed");
        let status = ProcessCommand::new("cargo")
            .args([
                "build",
                "--offline",
                "--locked",
                "-p",
                "termd",
                "--bin",
                "termd",
            ])
            .current_dir(&root)
            .status()
            .expect("baseline cargo build should run");
        assert!(status.success(), "real 0ccd03f termd build failed");
        binary
    })
}

fn startup_grant_frame(
    session_id: &str,
    socket_path: &Path,
    supervisor_pid: u32,
    cleanup_capability: &str,
) -> Vec<u8> {
    let capability = general_purpose::STANDARD
        .decode(cleanup_capability)
        .expect("test cleanup capability is base64");
    assert_eq!(capability.len(), 32);
    let mut frame = Vec::with_capacity(124);
    frame.extend_from_slice(b"TMDGRT01");
    frame.extend_from_slice(&[7_u8; 16]);
    frame.extend_from_slice(&capability);
    frame.extend_from_slice(&Sha256::digest(session_id.as_bytes()));
    frame.extend_from_slice(&Sha256::digest(socket_path.as_os_str().as_bytes()));
    frame.extend_from_slice(&supervisor_pid.to_be_bytes());
    frame
}

fn spawn_supervisor_from_short_lived_parent_with_capability(
    binary_path: &Path,
    session_id: &str,
    socket_path: &Path,
    command: &CommandSpec,
    size: PtySize,
    pid_file: &Path,
    cleanup_capability: Option<String>,
) -> u32 {
    let secret_probe = cleanup_capability.clone();
    let command_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(command).expect("command serializes"));
    let size_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(&size).expect("size serializes"));
    let script = r#"
	trap '' HUP
	exec 3<&0
	nohup "$TERMD_BIN" __session-supervisor \
  --session-id "$SESSION_ID" \
  --socket-path "$SOCKET_PATH" \
  --command-base64 "$COMMAND_BASE64" \
  --size-base64 "$SIZE_BASE64" \
  <&3 >/dev/null 2>&1 &
echo "$!" > "$PID_FILE"
"#;

    // 用短生命周期 shell 启动 supervisor，shell 退出后测试进程只持有 restore_info，
    // 不持有 Child 句柄，从而覆盖真实父进程退出后的重连路径。
    let mut helper_command = ProcessCommand::new("sh");
    helper_command
        .arg("-c")
        .arg(script)
        .env("TERMD_BIN", binary_path)
        .env("SESSION_ID", session_id)
        .env("SOCKET_PATH", socket_path)
        .env("COMMAND_BASE64", command_base64)
        .env("SIZE_BASE64", size_base64)
        .env("PID_FILE", pid_file)
        .stdin(Stdio::piped())
        .env_remove("TERMD_SUPERVISOR_CLEANUP_CAPABILITY");
    let mut helper = helper_command
        .spawn()
        .expect("short-lived supervisor launcher should spawn");
    let helper_pid = helper.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let supervisor_pid = loop {
        if let Ok(pid) = fs::read_to_string(pid_file)
            && let Ok(pid) = pid.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "short-lived launcher should write supervisor pid"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    if let Some(cleanup_capability) = cleanup_capability {
        helper
            .stdin
            .take()
            .expect("launcher startup pipe exists")
            .write_all(&startup_grant_frame(
                session_id,
                socket_path,
                supervisor_pid,
                &cleanup_capability,
            ))
            .unwrap();
    } else {
        drop(helper.stdin.take());
    }
    let status = helper.wait().expect("short-lived launcher should exit");
    assert!(status.success(), "short-lived launcher failed: {status}");

    #[cfg(target_os = "linux")]
    if let Some(secret) = secret_probe {
        for proc_file in ["cmdline", "environ"] {
            let bytes = fs::read(format!("/proc/{supervisor_pid}/{proc_file}")).unwrap();
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "startup capability leaked through supervisor {proc_file}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    assert_ne!(
        linux_process_parent_pid(supervisor_pid),
        Some(helper_pid),
        "supervisor should outlive and detach from its launcher parent"
    );

    supervisor_pid
}

#[test]
fn runtime_reconnects_to_live_supervisor_and_replays_snapshot_output() {
    let state_path = temp_state_path("runtime-reconnect.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000111";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf booted && cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();

    let first_boot = read_until_contains(&mut runtime, session_id, b"booted");
    assert!(
        first_boot
            .windows(b"booted".len())
            .any(|window| window == b"booted")
    );

    let persisted = runtime.persisted_sessions();
    assert_eq!(persisted.len(), 1);
    drop(runtime);

    let mut restarted = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    restarted.reconnect_session(&persisted[0]).unwrap();
    restarted.attach(session_id, "dev-b").unwrap();

    let replayed = read_until_contains(&mut restarted, session_id, b"booted");
    assert!(
        replayed
            .windows(b"booted".len())
            .any(|window| window == b"booted")
    );

    restarted
        .write_input(session_id, "dev-b", b"after-restart\n")
        .unwrap();
    let echoed = read_until_contains(&mut restarted, session_id, b"after-restart");
    assert!(
        echoed
            .windows(b"after-restart".len())
            .any(|window| window == b"after-restart")
    );

    restarted.close(session_id).unwrap();
}

#[test]
fn reconnect_takeover_clears_stale_attached_devices_before_new_attach() {
    let state_path = temp_state_path("runtime-reconnect-clears-attached.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000117";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf authority-ready; cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();
    read_until_contains(&mut runtime, session_id, b"authority-ready");

    let persisted = runtime.persisted_sessions();
    assert_eq!(persisted.len(), 1);

    let mut restarted = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    restarted.reconnect_session(&persisted[0]).unwrap();

    // 中文注释：新的 daemon controller 接管后，supervisor 内旧 attached 集合必须被清空；
    // 否则掉线设备会在重启后继续被误判为活跃 operator。
    assert_eq!(restarted.role(session_id, "dev-a").unwrap(), None);

    restarted.attach(session_id, "dev-b").unwrap();
    restarted
        .write_input(session_id, "dev-b", b"after-reconnect\n")
        .unwrap();
    let echoed = read_until_contains(&mut restarted, session_id, b"after-reconnect");
    assert!(
        echoed
            .windows(b"after-reconnect".len())
            .any(|window| window == b"after-reconnect")
    );

    restarted.close(session_id).unwrap();
}

#[test]
fn stale_runtime_request_does_not_reconnect_and_steal_active_controller() {
    let state_path = temp_state_path("runtime-stale-controller.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000118";

    let mut first = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    first
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf stale-owner-ready; cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    first.attach(session_id, "dev-a").unwrap();
    read_until_contains(&mut first, session_id, b"stale-owner-ready");

    let persisted = first.persisted_sessions();
    assert_eq!(persisted.len(), 1);

    let mut restarted = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    restarted.reconnect_session(&persisted[0]).unwrap();
    restarted.attach(session_id, "dev-b").unwrap();

    let error = first
        .role(session_id, "dev-a")
        .expect_err("stale controller request should be rejected instead of auto-reconnecting");
    assert!(
        error.to_string().contains("active controller"),
        "stale controller should surface authority rejection, got {error}"
    );

    restarted
        .write_input(session_id, "dev-b", b"active-controller-still-works\n")
        .unwrap();
    let echoed = read_until_contains(&mut restarted, session_id, b"active-controller-still-works");
    assert!(
        echoed
            .windows(b"active-controller-still-works".len())
            .any(|window| window == b"active-controller-still-works")
    );

    restarted.close(session_id).unwrap();
}

#[test]
fn supervisor_terminal_snapshot_survives_last_client_detach() {
    let state_path = temp_state_path("runtime-detach-snapshot.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000113";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf booted && cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();
    read_until_contains(&mut runtime, session_id, b"booted");
    runtime.detach(session_id, "dev-a").unwrap();

    runtime.attach(session_id, "dev-b").unwrap();
    let frames = runtime.terminal_snapshot(session_id, None).unwrap();
    assert!(
        frames.iter().any(|frame| matches!(frame, PtyTerminalFrame::Snapshot { data, .. } if data.windows(b"booted".len()).any(|window| window == b"booted"))),
        "snapshot after detach should still come from live supervisor"
    );

    runtime.close(session_id).unwrap();
}

#[test]
fn supervisor_terminal_snapshot_preserves_active_sgr_style_after_reattach() {
    let state_path = temp_state_path("runtime-style-snapshot.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000114";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf '\\033[31mred-still-open'; cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();
    read_until_contains(&mut runtime, session_id, b"red-still-open");
    runtime.detach(session_id, "dev-a").unwrap();

    runtime.attach(session_id, "dev-b").unwrap();
    let frames = runtime.terminal_snapshot(session_id, None).unwrap();
    let snapshot = frames
        .iter()
        .find_map(|frame| {
            if let PtyTerminalFrame::Snapshot { data, .. } = frame {
                Some(String::from_utf8_lossy(data).into_owned())
            } else {
                None
            }
        })
        .expect("terminal snapshot should be returned after reattach");

    // 中文注释：样式未 reset 的命令会依赖终端当前 SGR 状态；snapshot 末尾要恢复该状态，
    // 后续 tail 才会继续按红色渲染，而不是回到默认颜色。
    assert!(
        snapshot.contains("\x1b[31mred-still-open\x1b[0m"),
        "snapshot should preserve styled cells: {snapshot:?}"
    );
    assert!(
        snapshot.ends_with("\x1b[1;15H\x1b[31m"),
        "snapshot should restore cursor and active SGR style for subsequent tail: {snapshot:?}"
    );

    runtime.close(session_id).unwrap();
}

#[test]
fn supervisor_attach_sync_returns_tail_from_journal_without_snapshot() {
    let state_path = temp_state_path("runtime-tail-sync.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000115";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf first-sync; cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();

    let first_snapshot =
        terminal_snapshot_until_contains(&mut runtime, session_id, None, b"first-sync");
    let first_seq = snapshot_base_seq_containing(&first_snapshot, b"first-sync");
    runtime
        .write_input(session_id, "dev-a", b"second-sync\n")
        .unwrap();
    let second = read_terminal_frame_until_contains(&mut runtime, session_id, b"second-sync");
    let second_seq = second
        .terminal_seq()
        .expect("tail output should have a session terminal seq");

    let tail = runtime
        .terminal_snapshot(session_id, Some(first_seq))
        .expect("AttachSync should return terminal tail");

    assert!(
        tail.iter()
            .all(|frame| !matches!(frame, PtyTerminalFrame::Snapshot { .. })),
        "journal-covered AttachSync must return tail frames, not a snapshot: {tail:?}"
    );
    assert!(
        tail.iter()
            .all(|frame| frame.terminal_seq().is_none_or(|seq| seq > first_seq)),
        "tail must not replay frames already covered by last_terminal_seq={first_seq}: {tail:?}"
    );
    assert!(
        tail.iter()
            .any(|frame| frame.terminal_seq() == Some(second_seq)
                && frame_contains(frame, b"second-sync")),
        "tail should include the output after the rendered seq: {tail:?}"
    );

    runtime.close(session_id).unwrap();
}

#[test]
fn supervisor_reconnect_attach_sync_does_not_replay_rendered_terminal_frame() {
    let state_path = temp_state_path("runtime-reconnect-terminal-sync.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000116";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf rendered-before-reconnect; sleep 60"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();
    let rendered_snapshot = terminal_snapshot_until_contains(
        &mut runtime,
        session_id,
        None,
        b"rendered-before-reconnect",
    );
    let rendered_seq =
        snapshot_base_seq_containing(&rendered_snapshot, b"rendered-before-reconnect");
    let persisted = runtime.persisted_sessions();
    assert_eq!(persisted.len(), 1);
    drop(runtime);

    let mut restarted = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    restarted.reconnect_session(&persisted[0]).unwrap();
    restarted.attach(session_id, "dev-b").unwrap();

    let sync = restarted
        .terminal_snapshot(session_id, Some(rendered_seq))
        .expect("reconnected daemon should AttachSync with last rendered seq");
    assert!(
        sync.is_empty(),
        "reconnect AttachSync should not replay already rendered seq {rendered_seq}: {sync:?}"
    );

    restarted.close(session_id).unwrap();
}

#[test]
#[cfg(target_os = "linux")]
fn runtime_reports_supervisor_shell_current_working_directory() {
    let state_path = temp_state_path("runtime-cwd.json");
    let root = temp_state_path("runtime-cwd-root");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000112";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").cwd(&root),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();
    runtime
        .write_input(
            session_id,
            "dev-a",
            format!("cd {}\nprintf cwd-ready\\n", work.display()).as_bytes(),
        )
        .unwrap();
    read_until_contains(&mut runtime, session_id, b"cwd-ready");

    let expected = work.canonicalize().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if runtime
            .current_working_directory(session_id)
            .unwrap()
            .as_ref()
            == Some(&expected)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor shell cwd did not update to {}",
            expected.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    runtime.close(session_id).unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(unix)]
fn closing_restore_fast_path_never_creates_a_regular_controller() {
    let state_path = temp_state_path("closing-reconnect-retry.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let supervisor_pid = spawn_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let socket_deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() {
        assert!(
            Instant::now() < socket_deadline,
            "supervisor socket was not created"
        );
        std::thread::yield_now();
    }
    let now_ms = current_unix_timestamp_millis();
    let record = SessionStateRecord {
        session_id,
        state: SessionState::Running,
        size: termd_proto::TerminalSize::new(24, 80),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        restore_info: Some(PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid,
            supervisor_status: PtySupervisorStatus::Closing,
        }),
    };
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![record.clone()],
        },
    )
    .unwrap();
    persist_test_ownership(
        &state_path,
        session_id,
        record.restore_info.as_ref().unwrap(),
        "cleaning",
        None,
    );

    let retry_backend = FailFirstReconnectBackend::new(backend.clone());
    let state = StateStore::load(&state_path).unwrap();
    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        retry_backend.clone(),
        Ed25519SignatureVerifier,
        state,
    )
    .unwrap();
    let mut protocol = Some(protocol);
    assert_eq!(retry_backend.reconnect_attempts(), 0);
    assert!(linux_process_state(supervisor_pid).is_some());
    assert_eq!(StateStore::load(&state_path).unwrap().sessions.len(), 1);

    let deadline = Instant::now() + Duration::from_secs(15);
    while !StateStore::load(&state_path).unwrap().sessions.is_empty() {
        if Instant::now() >= deadline {
            drop(protocol.take());
            let reconnect_info = PtyRestoreInfo::UnixSocket {
                socket_path: socket_path.clone(),
                supervisor_pid,
                supervisor_status: PtySupervisorStatus::Running,
            };
            if let Ok(mut cleanup) =
                backend.reconnect(&session_id_text, &reconnect_info, PtySize::new(24, 80))
            {
                let _ = cleanup.terminate();
            }
            panic!("background cleanup did not retry the transient reconnect failure");
        }
        std::thread::yield_now();
    }

    assert_eq!(retry_backend.reconnect_attempts(), 0);
    drop(protocol.take());
    wait_until_linux_process_is_reaped(supervisor_pid);
    let _ = fs::remove_file(pid_file);
}

#[test]
#[cfg(target_os = "linux")]
fn legacy_closing_double_null_restore_atomically_installs_cleanup_journal_and_cleans_up() {
    run_legacy_closing_migration(false);
}

#[test]
#[cfg(target_os = "linux")]
fn legacy_closing_with_residual_controller_converges_without_expanding_authority() {
    run_legacy_closing_migration(true);
}

#[cfg(target_os = "linux")]
fn run_legacy_closing_migration(keep_residual_controller: bool) {
    let state_path = temp_state_path(if keep_residual_controller {
        "legacy-closing-residual-controller.json"
    } else {
        "legacy-closing-capability.json"
    });
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let sqlite_path = state_path.with_extension("sqlite");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let supervisor_pid = spawn_legacy_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let _supervisor_guard = TestSupervisorGuard::new(supervisor_pid, socket_path.clone());
    let now_ms = current_unix_timestamp_millis();
    let record = SessionStateRecord {
        session_id,
        state: SessionState::Running,
        size: termd_proto::TerminalSize::new(24, 80),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        restore_info: Some(PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid,
            supervisor_status: PtySupervisorStatus::Closing,
        }),
    };
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![record.clone()],
        },
    )
    .unwrap();
    let state_db = rusqlite::Connection::open(&sqlite_path).unwrap();
    let columns = {
        let mut statement = state_db
            .prepare("PRAGMA table_info(runtime_sessions)")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert!(
        !columns.iter().any(|column| {
            matches!(column.as_str(), "close_operation_id" | "cleanup_capability")
        })
    );
    drop(state_db);

    let mut residual_runtime = if keep_residual_controller {
        let mut running_record = record;
        running_record.restore_info = Some(PtyRestoreInfo::UnixSocket {
            socket_path: socket_path.clone(),
            supervisor_pid,
            supervisor_status: PtySupervisorStatus::Running,
        });
        let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
            &binary_path,
            &state_path,
        ));
        runtime.reconnect_session(&running_record).unwrap();
        runtime
            .attach(&session_id_text, "legacy-closing-residual")
            .unwrap();
        Some(runtime)
    } else {
        None
    };

    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let state = StateStore::load(&state_path).unwrap();
    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        backend,
        Ed25519SignatureVerifier,
        state,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !StateStore::load(&state_path).unwrap().sessions.is_empty() {
        assert!(
            Instant::now() < deadline,
            "legacy Closing cleanup timed out"
        );
        std::thread::yield_now();
    }
    if let Some(runtime) = residual_runtime.as_mut() {
        assert!(
            runtime
                .write_input(
                    &session_id_text,
                    "legacy-closing-residual",
                    b"must-not-write\n"
                )
                .is_err()
        );
    }
    drop(protocol);
    wait_until_linux_process_is_reaped(supervisor_pid);
    let _ = fs::remove_file(pid_file);
}

#[test]
#[cfg(target_os = "linux")]
fn real_064_closing_lost_close_response_converges_after_restart() {
    let state_path = temp_state_path("legacy-closing-lost-close-response.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let supervisor_pid = spawn_legacy_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let _supervisor_guard = TestSupervisorGuard::new(supervisor_pid, socket_path.clone());
    let now_ms = current_unix_timestamp_millis();
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                restore_info: Some(PtyRestoreInfo::UnixSocket {
                    socket_path: socket_path.clone(),
                    supervisor_pid,
                    supervisor_status: PtySupervisorStatus::Closing,
                }),
            }],
        },
    )
    .unwrap();

    let mut controller = connect_raw_supervisor(&socket_path);
    write_raw_supervisor_request(
        &mut controller,
        serde_json::json!({
            "request_id": 1,
            "request": {
                "type": "attach_sync",
                "session_id": session_id_text,
                "last_terminal_seq": null,
                "resume_controller_id": null
            }
        }),
    );
    assert_eq!(
        read_raw_supervisor_response(&mut controller)["response"]["status"],
        "ok"
    );
    write_raw_supervisor_request(
        &mut controller,
        serde_json::json!({"request_id": 2, "request": {"type": "close"}}),
    );
    drop(controller);
    wait_until_linux_process_is_reaped(supervisor_pid);

    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path),
        Ed25519SignatureVerifier,
        StateStore::load(&state_path).unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !StateStore::load(&state_path).unwrap().sessions.is_empty() {
        assert!(
            Instant::now() < deadline,
            "lost legacy Close response did not converge"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(protocol);
    let _ = fs::remove_file(pid_file);
}

#[test]
#[ignore = "spawned only by real_064_closing_repeated_migration_after_install_response_loss"]
#[cfg(target_os = "linux")]
fn legacy_closing_migration_crash_child() {
    let state_path = PathBuf::from(std::env::var_os("TERMD_TEST_CRASH_STATE").unwrap());
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let _protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path),
        Ed25519SignatureVerifier,
        StateStore::load(&state_path).unwrap(),
    )
    .unwrap();
}

#[test]
#[cfg(target_os = "linux")]
fn real_064_closing_repeated_migration_after_install_response_loss() {
    let state_path = temp_state_path("legacy-closing-repeat-migration.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let sqlite_path = state_path.with_extension("sqlite");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let supervisor_pid = spawn_legacy_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let _supervisor_guard = TestSupervisorGuard::new(supervisor_pid, socket_path.clone());
    let now_ms = current_unix_timestamp_millis();
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                restore_info: Some(PtyRestoreInfo::UnixSocket {
                    socket_path,
                    supervisor_pid,
                    supervisor_status: PtySupervisorStatus::Closing,
                }),
            }],
        },
    )
    .unwrap();
    let checkpoint_dir = temp_state_path("legacy-closing-repeat-checkpoints");
    fs::create_dir_all(&checkpoint_dir).unwrap();
    let marker = checkpoint_dir.join("after_legacy_cleaning_commit.reached");
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "legacy_closing_migration_crash_child",
            "--nocapture",
        ])
        .env("TERMD_TEST_CRASH_STATE", &state_path)
        .env("TERMD_TEST_OWNERSHIP_CHECKPOINT_DIR", &checkpoint_dir)
        .env(
            "TERMD_TEST_OWNERSHIP_CHECKPOINT",
            "after_legacy_cleaning_commit",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "legacy migration child exited before cleaning checkpoint"
        );
        assert!(
            Instant::now() < deadline,
            "legacy cleaning checkpoint timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let connection = rusqlite::Connection::open(&sqlite_path).unwrap();
    let (rows, phase, operation_id, legacy_protocol): (i64, String, String, i64) = connection
        .query_row(
            "SELECT COUNT(*), phase, close_operation_id, legacy_protocol
             FROM session_ownership WHERE session_id = ?1",
            [&session_id_text],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(phase, "cleaning");
    assert!(!operation_id.is_empty());
    assert_eq!(legacy_protocol, 1);
    drop(connection);
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));

    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path),
        Ed25519SignatureVerifier,
        StateStore::load(&state_path).unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !StateStore::load(&state_path).unwrap().sessions.is_empty() {
        assert!(
            Instant::now() < deadline,
            "repeated legacy migration did not converge"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(protocol);
    wait_until_linux_process_is_reaped(supervisor_pid);
    fs::remove_dir_all(checkpoint_dir).unwrap();
    let _ = fs::remove_file(pid_file);
}

#[test]
#[cfg(target_os = "linux")]
fn daemon_startup_warns_but_does_not_adopt_unrecorded_live_supervisor() {
    let state_dir = std::env::temp_dir().join(format!(
        "td-a-{}-{}",
        std::process::id(),
        current_unix_timestamp_millis().0
    ));
    fs::create_dir_all(&state_dir).unwrap();
    let state_path = state_dir.join("daemon-state.json");
    let socket_dir = state_dir.join("termd-supervisors");
    fs::create_dir_all(&socket_dir).unwrap();
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000777";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    let pid_file = state_dir.join("session-supervisor.pid");
    let log_file = state_dir.join("session-supervisor.log");
    let command = CommandSpec::new("sh").args(["-lc", "printf adopted-live; sleep 60"]);
    let size = PtySize::with_pixels(28, 90, 1440, 900);

    let command_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(&command).expect("command serializes"));
    let size_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(&size).expect("size serializes"));
    let stdout_log = fs::File::create(&log_file).unwrap();
    let stderr_log = stdout_log.try_clone().unwrap();
    let mut supervisor = ProcessCommand::new(&binary_path)
        .args([
            "__session-supervisor",
            "--session-id",
            session_id,
            "--socket-path",
            socket_path.to_string_lossy().as_ref(),
            "--command-base64",
            &command_base64,
            "--size-base64",
            &size_base64,
        ])
        .env_remove("TERMD_SUPERVISOR_CLEANUP_CAPABILITY")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .expect("test supervisor should spawn");
    let supervisor_pid = supervisor.id();
    supervisor
        .stdin
        .take()
        .expect("test supervisor startup pipe should exist")
        .write_all(&startup_grant_frame(
            session_id,
            &socket_path,
            supervisor_pid,
            &test_cleanup_capability_base64(),
        ))
        .expect("test supervisor startup grant should write");
    assert!(
        linux_process_state(supervisor_pid).is_some(),
        "test supervisor should be alive before daemon startup"
    );
    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let wait_for_target = |supervisor: &mut std::process::Child| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut candidates = Vec::new();
        while Instant::now() < deadline {
            candidates = backend.live_supervisor_restore_candidates().unwrap();
            if candidates.iter().any(|candidate| {
                candidate.supervisor_pid == supervisor_pid && candidate.session_id == session_id
            }) {
                return candidates;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let cmdline = fs::read(format!("/proc/{supervisor_pid}/cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
            .unwrap_or_else(|error| format!("cmdline read failed: {error}"));
        panic!(
            "target supervisor was not discovered: pid={supervisor_pid} candidates={candidates:?} state={:?} status={:?} cmdline={cmdline}",
            linux_process_state(supervisor_pid),
            supervisor.try_wait().unwrap(),
        );
    };
    let candidates = wait_for_target(&mut supervisor);
    let debug_cmdline = fs::read(format!("/proc/{supervisor_pid}/cmdline"))
        .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
        .unwrap_or_else(|error| format!("cmdline read failed: {error}"));
    let supervisor_status = supervisor.try_wait().unwrap();
    let supervisor_log = fs::read_to_string(&log_file).unwrap_or_default();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.session_id == session_id),
        "live supervisor should be discoverable before daemon startup: candidates={candidates:?} state={:?} status={supervisor_status:?} cmdline={debug_cmdline} log={supervisor_log}",
        linux_process_state(supervisor_pid),
    );
    assert!(candidates.iter().any(|candidate| {
        candidate.supervisor_pid == supervisor_pid && candidate.session_id == session_id
    }));

    let config = DaemonConfig::default_for_state_path(&state_path);
    let protocol = try_default_protocol(config).unwrap();

    let reloaded = StateStore::load(&state_path).unwrap();
    assert!(
        reloaded.sessions.is_empty(),
        "startup must not silently adopt old supervisor runtime rows after the tmux schema bump"
    );
    let candidates_after_startup = wait_for_target(&mut supervisor);
    assert!(candidates_after_startup.iter().any(|candidate| {
        candidate.supervisor_pid == supervisor_pid && candidate.session_id == session_id
    }));
    assert!(
        linux_process_state(supervisor_pid).is_some(),
        "startup must leave unrecorded live supervisor running for manual reset/cleanup"
    );

    drop(protocol);
    let _ = supervisor.kill();
    let _ = supervisor.wait();
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_file);
    let _ = fs::remove_file(log_file);
}

#[test]
#[cfg(target_os = "linux")]
fn close_reaps_spawned_supervisor_child_process() {
    let state_path = temp_state_path("close-reaps.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000444";

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf close-ready && cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    read_until_contains(&mut runtime, session_id, b"close-ready");

    let restore_info = runtime.restore_info(session_id).unwrap().unwrap();
    let supervisor_pid = restore_supervisor_pid(&restore_info);
    assert!(
        linux_process_state(supervisor_pid).is_some(),
        "supervisor process should exist before close"
    );

    runtime.close(session_id).unwrap();

    wait_until_linux_process_is_reaped(supervisor_pid);
}

#[test]
#[cfg(target_os = "linux")]
fn lost_close_response_is_finalized_on_new_connection_and_pid_disappears() {
    let state_path = temp_state_path("close-response-loss-reaps.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000446";
    let supervisor_pid = spawn_supervisor_from_short_lived_parent(
        &binary_path,
        session_id,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let operation_id = 91_u64;

    let mut first = connect_raw_supervisor(&socket_path);
    write_raw_supervisor_request(
        &mut first,
        serde_json::json!({
            "request_id": 1,
            "request": {
                "type": "attach_sync",
                "session_id": session_id,
                "last_terminal_seq": null,
                "resume_controller_id": null
            }
        }),
    );
    let _ = read_raw_supervisor_response(&mut first);
    write_raw_supervisor_request(
        &mut first,
        serde_json::json!({
            "request_id": 2,
            "request": { "type": "close_idempotent", "operation_id": operation_id }
        }),
    );
    drop(first);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut recovery = connect_raw_supervisor(&socket_path);
        authenticate_raw_cleanup(&mut recovery, session_id, 3);
        write_raw_supervisor_request(
            &mut recovery,
            serde_json::json!({
                "request_id": 5,
                "request": { "type": "close_status", "operation_id": operation_id }
            }),
        );
        let status = read_raw_supervisor_response(&mut recovery);
        if status["response"]["payload"]["confirmed_dead"] == true {
            write_raw_supervisor_request(
                &mut recovery,
                serde_json::json!({
                    "request_id": 6,
                    "request": { "type": "finalize_close", "operation_id": operation_id }
                }),
            );
            assert_eq!(
                read_raw_supervisor_response(&mut recovery)["response"]["status"],
                "ok"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "close status was never confirmed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    wait_until_linux_process_is_reaped(supervisor_pid);
}

#[test]
#[cfg(target_os = "linux")]
fn restart_between_close_and_finalize_reuses_durable_operation_id() {
    let state_path = temp_state_path("restart-between-close-finalize.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let sqlite_path = state_path.with_extension("sqlite");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let supervisor_pid = spawn_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let operation_id = u64::MAX - 73;
    let now_ms = current_unix_timestamp_millis();
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                restore_info: Some(PtyRestoreInfo::UnixSocket {
                    socket_path: socket_path.clone(),
                    supervisor_pid,
                    supervisor_status: PtySupervisorStatus::Closing,
                }),
            }],
        },
    )
    .unwrap();
    let restore_info = StateStore::load(&state_path).unwrap().sessions[0]
        .restore_info
        .clone()
        .unwrap();
    persist_test_ownership(
        &state_path,
        session_id,
        &restore_info,
        "cleaning",
        Some(operation_id),
    );

    let mut first = connect_raw_supervisor(&socket_path);
    write_raw_supervisor_request(
        &mut first,
        serde_json::json!({
            "request_id": 1,
            "request": {
                "type": "attach_sync",
                "session_id": session_id_text,
                "last_terminal_seq": null,
                "resume_controller_id": null
            }
        }),
    );
    let _ = read_raw_supervisor_response(&mut first);
    write_raw_supervisor_request(
        &mut first,
        serde_json::json!({
            "request_id": 2,
            "request": { "type": "close_idempotent", "operation_id": operation_id }
        }),
    );
    let close = read_raw_supervisor_response(&mut first);
    assert_eq!(close["response"]["status"], "ok");
    assert_eq!(close["response"]["payload"]["confirmed_dead"], true);
    drop(first);

    let exited_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut status = connect_raw_supervisor(&socket_path);
        write_raw_supervisor_request(
            &mut status,
            serde_json::json!({
                "request_id": 3,
                "request": { "type": "natural_exit_status" }
            }),
        );
        let status = read_raw_supervisor_response(&mut status);
        if status["response"]["payload"]["confirmed_dead"] == true {
            break;
        }
        assert!(
            Instant::now() < exited_deadline,
            "supervisor exit watcher did not publish exited state"
        );
        std::thread::yield_now();
    }

    let mut rejected_attach = connect_raw_supervisor(&socket_path);
    write_raw_supervisor_request(
        &mut rejected_attach,
        serde_json::json!({
            "request_id": 4,
            "request": {
                "type": "attach_sync",
                "session_id": session_id_text,
                "last_terminal_seq": null,
                "resume_controller_id": null
            }
        }),
    );
    assert_eq!(
        read_raw_supervisor_response(&mut rejected_attach)["response"]["status"],
        "err"
    );
    drop(rejected_attach);

    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let state = StateStore::load(&state_path).unwrap();
    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        backend,
        Ed25519SignatureVerifier,
        state,
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state_db = rusqlite::Connection::open(&sqlite_path).unwrap();
        let remaining: i64 = state_db
            .query_row("SELECT COUNT(*) FROM runtime_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        if remaining == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restart did not finalize the durable close operation"
        );
        std::thread::yield_now();
    }

    drop(protocol);
    wait_until_linux_process_is_reaped(supervisor_pid);
    let _ = fs::remove_file(pid_file);
}

#[test]
#[cfg(target_os = "linux")]
fn lost_finalize_response_converges_from_durable_closing_record() {
    let state_path = temp_state_path("lost-finalize-response.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let supervisor_pid = spawn_supervisor_from_short_lived_parent(
        &binary_path,
        &session_id_text,
        &socket_path,
        &CommandSpec::new("sh").args(["-c", "cat"]),
        PtySize::new(24, 80),
        &pid_file,
    );
    let operation_id = u64::MAX - 79;
    let now_ms = current_unix_timestamp_millis();
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                restore_info: Some(PtyRestoreInfo::UnixSocket {
                    socket_path: socket_path.clone(),
                    supervisor_pid,
                    supervisor_status: PtySupervisorStatus::Closing,
                }),
            }],
        },
    )
    .unwrap();
    let restore_info = StateStore::load(&state_path).unwrap().sessions[0]
        .restore_info
        .clone()
        .unwrap();
    persist_test_ownership(
        &state_path,
        session_id,
        &restore_info,
        "cleaning",
        Some(operation_id),
    );

    let mut controller = connect_raw_supervisor(&socket_path);
    write_raw_supervisor_request(
        &mut controller,
        serde_json::json!({
            "request_id": 1,
            "request": {
                "type": "attach_sync",
                "session_id": session_id_text,
                "last_terminal_seq": null,
                "resume_controller_id": null
            }
        }),
    );
    let _ = read_raw_supervisor_response(&mut controller);
    write_raw_supervisor_request(
        &mut controller,
        serde_json::json!({
            "request_id": 2,
            "request": { "type": "close_idempotent", "operation_id": operation_id }
        }),
    );
    assert_eq!(
        read_raw_supervisor_response(&mut controller)["response"]["payload"]["confirmed_dead"],
        true
    );
    drop(controller);

    let mut finalize = connect_raw_supervisor(&socket_path);
    authenticate_raw_cleanup(&mut finalize, &session_id_text, 3);
    write_raw_supervisor_request(
        &mut finalize,
        serde_json::json!({
            "request_id": 5,
            "request": { "type": "finalize_close", "operation_id": operation_id }
        }),
    );
    drop(finalize);
    wait_until_linux_process_is_reaped(supervisor_pid);

    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let retry_backend = FailFirstReconnectBackend::new(backend);
    let state = StateStore::load(&state_path).unwrap();
    let protocol = DaemonProtocol::from_state(
        DaemonConfig::default_for_state_path(&state_path),
        retry_backend.clone(),
        Ed25519SignatureVerifier,
        state,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !StateStore::load(&state_path).unwrap().sessions.is_empty() {
        assert!(
            Instant::now() < deadline,
            "lost FinalizeClose response did not converge from durable state"
        );
        std::thread::yield_now();
    }
    assert_eq!(retry_backend.reconnect_attempts(), 0);
    drop(protocol);
    let _ = fs::remove_file(pid_file);
}

#[test]
fn naturally_exited_supervisor_is_closed_and_not_reconnectable() {
    let state_dir = TestStateDirectory::new("natural-exit");
    let state_path = state_dir.join("daemon-state.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000445";
    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let mut runtime = SessionRuntime::new(backend.clone());
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-c", "exit 7"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    #[cfg(target_os = "linux")]
    let _supervisor_guard = {
        let restore_info = runtime.restore_info(session_id).unwrap().unwrap();
        let PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            ..
        } = restore_info
        else {
            panic!("natural exit test requires Unix supervisor restore info");
        };
        TestSupervisorGuard::new(supervisor_pid, socket_path)
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime.snapshot(session_id).unwrap();
        let frames = runtime.terminal_snapshot(session_id, Some(0)).unwrap();
        if frames
            .iter()
            .any(|frame| matches!(frame, PtyTerminalFrame::Exit { code: Some(7), .. }))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for natural exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        runtime.attach(session_id, "dev-after-exit").unwrap_err(),
        termd::runtime::RuntimeError::SessionClosed
    );
    assert_eq!(
        runtime
            .write_input(session_id, "dev-after-exit", b"rejected")
            .unwrap_err(),
        termd::runtime::RuntimeError::SessionClosed
    );
    let persisted = runtime.persisted_sessions();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].state, SessionState::Closed);

    let mut restarted = SessionRuntime::new(backend);
    assert_eq!(
        restarted.reconnect_session(&persisted[0]).unwrap_err(),
        termd::runtime::RuntimeError::SessionClosed
    );
}

#[test]
fn reconnects_to_supervisor_after_launcher_parent_process_exits() {
    let state_path = temp_state_path("parent-exit.json");
    let socket_path = state_path.with_extension("sock");
    let pid_file = state_path.with_extension("pid");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = "00000000-0000-0000-0000-000000000555";
    let command = CommandSpec::new("sh").args(["-lc", "printf parent-exited && cat"]);
    let size = PtySize::new(24, 80);

    let supervisor_pid = spawn_supervisor_from_short_lived_parent(
        &binary_path,
        session_id,
        &socket_path,
        &command,
        size,
        &pid_file,
    );

    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let restore_info = PtyRestoreInfo::UnixSocket {
        socket_path: socket_path.clone(),
        supervisor_pid,
        supervisor_status: PtySupervisorStatus::Running,
    };
    let restored_session_id = SessionId(uuid::Uuid::parse_str(session_id).unwrap());
    let now_ms = current_unix_timestamp_millis();
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: vec![SessionStateRecord {
                session_id: restored_session_id,
                state: SessionState::Running,
                size: termd_proto::TerminalSize::new(24, 80),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                restore_info: Some(restore_info.clone()),
            }],
        },
    )
    .unwrap();
    StateStore::load(&state_path).unwrap();
    let mut session = backend
        .reconnect(session_id, &restore_info, PtySize::new(24, 80))
        .unwrap();

    let replayed = read_session_until_contains(&mut *session, b"parent-exited");
    assert!(
        replayed
            .windows(b"parent-exited".len())
            .any(|window| window == b"parent-exited")
    );

    session.write_all(b"after-parent-exit\n").unwrap();
    let echoed = read_session_until_contains(&mut *session, b"after-parent-exit");
    assert!(
        echoed
            .windows(b"after-parent-exit".len())
            .any(|window| window == b"after-parent-exit")
    );

    session.terminate().unwrap();
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_file);
}

#[test]
fn state_store_roundtrips_supervisor_restore_info() {
    let state_path = temp_state_path("restore-info.json");
    let statuses = [
        PtySupervisorStatus::Running,
        PtySupervisorStatus::Closing,
        PtySupervisorStatus::Closed,
    ];
    let sessions = statuses
        .iter()
        .enumerate()
        .map(|(index, supervisor_status)| SessionStateRecord {
            session_id: SessionId(
                uuid::Uuid::parse_str(&format!("00000000-0000-0000-0000-{:012}", 222 + index))
                    .unwrap(),
            ),
            state: SessionState::Running,
            size: termd_proto::TerminalSize::new(30 + index as u16, 100),
            created_at_ms: UnixTimestampMillis(1000 + index as u64),
            updated_at_ms: UnixTimestampMillis(2000 + index as u64),
            restore_info: Some(PtyRestoreInfo::UnixSocket {
                socket_path: std::env::temp_dir()
                    .join(format!("termd-test-supervisor-{index}.sock")),
                supervisor_pid: 10_000 + index as u32,
                supervisor_status: *supervisor_status,
            }),
        })
        .collect();
    let state = DaemonState {
        version: termd::state::STATE_SCHEMA_VERSION,
        daemon_identity: Some(DaemonIdentitySnapshot {
            server_id: ServerId(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000333").unwrap(),
            ),
            public_key: PublicKey("daemon-public".to_owned()),
            private_key: Some("ed25519-v1:daemon-private".to_owned()),
        }),
        trusted_devices: Vec::new(),
        sessions,
    };

    StateStore::save(&state_path, &state).unwrap();
    let loaded = StateStore::load(&state_path).unwrap();

    assert_eq!(loaded.sessions, state.sessions);
}
