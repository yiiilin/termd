#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use termd::pty::supervisor::SupervisorPtyBackend;
use termd::pty::{CommandSpec, PtyRestoreInfo};
use termd::runtime::SessionRuntime;
use termd::session::TerminalSize;
use termd::state::{DaemonState, SessionStateRecord, StateStore};
use termd_proto::SessionId;

const DAEMON_STATE_FILE: &str = "daemon-state.json";

#[test]
fn second_daemon_with_same_state_exits_before_recovery_even_on_a_different_port() {
    let state_dir = unique_test_dir("daemon-single-instance");
    fs::create_dir(&state_dir).expect("test state directory should be created");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
        .expect("test state directory should be private");
    let state_path = state_dir.join(DAEMON_STATE_FILE);
    let session = seed_running_session(&state_path);
    let original_supervisor_pid = supervisor_pid(&session);
    let mut cleanup = DaemonCleanup::new(state_dir.clone(), state_path.clone(), session.clone());

    let first_port = unused_local_port();
    let first = spawn_daemon(&state_dir, first_port);
    cleanup.first = Some(first);
    wait_for_healthz(first_port);

    let second_port = unused_local_port();
    let second = spawn_daemon(&state_dir, second_port);
    cleanup.second = Some(second);
    let second_output = wait_for_exit(
        cleanup.second.as_mut().expect("second daemon should exist"),
        Duration::from_secs(5),
    );
    assert!(
        !second_output.status.success(),
        "second daemon unexpectedly started with the same state; stdout={} stderr={}",
        String::from_utf8_lossy(&second_output.stdout),
        String::from_utf8_lossy(&second_output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("state lock"),
        "second daemon must identify state ownership rather than report a listen failure; stderr={}",
        String::from_utf8_lossy(&second_output.stderr),
    );

    wait_for_healthz(first_port);
    assert!(
        Path::new(&format!("/proc/{original_supervisor_pid}")).exists(),
        "the first daemon's recovered session supervisor must remain running"
    );
    let reloaded =
        StateStore::load(&state_path).expect("first daemon state should remain readable");
    let restored = reloaded
        .sessions
        .iter()
        .find(|record| record.session_id == session.session_id)
        .expect("first daemon should retain the restored session record");
    assert!(
        supervisor_pid(restored) == original_supervisor_pid,
        "duplicate startup must not replace the first daemon's session supervisor"
    );
}

#[test]
fn second_daemon_still_fails_when_holder_state_sqlite_is_atomically_replaced() {
    let state_dir = unique_test_dir("daemon-lock-survives-sqlite-replace");
    fs::create_dir(&state_dir).expect("test state directory should be created");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
        .expect("test state directory should be private");
    let state_path = state_dir.join(DAEMON_STATE_FILE);
    let session = seed_running_session(&state_path);
    let mut cleanup = DaemonCleanup::new(state_dir.clone(), state_path.clone(), session);

    let first_port = unused_local_port();
    cleanup.first = Some(spawn_daemon(&state_dir, first_port));
    wait_for_healthz(first_port);

    let sqlite_path = state_dir.join("daemon-state.sqlite");
    let replacement = state_dir.join("replacement.sqlite");
    fs::write(&replacement, b"replacement sqlite inode").expect("replacement should write");
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600))
        .expect("replacement should be private");
    fs::rename(&replacement, &sqlite_path).expect("SQLite inode should be atomically replaced");

    cleanup.second = Some(spawn_daemon(&state_dir, unused_local_port()));
    let output = wait_for_exit(
        cleanup.second.as_mut().expect("second daemon should exist"),
        Duration::from_secs(5),
    );
    assert!(
        !output.status.success(),
        "replacement SQLite inode must not allow a second daemon; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("state lock"),
        "second daemon must fail on the stable lock file before opening replacement state; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_healthz(first_port);
}

#[test]
fn daemon_state_lock_is_released_after_holder_is_killed() {
    let state_dir = unique_test_dir("daemon-lock-released-after-sigkill");
    fs::create_dir(&state_dir).expect("test state directory should be created");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
        .expect("test state directory should be private");
    let state_path = state_dir.join(DAEMON_STATE_FILE);
    let session = seed_running_session(&state_path);
    let mut cleanup = DaemonCleanup::new(state_dir.clone(), state_path, session);

    let holder_port = unused_local_port();
    cleanup.first = Some(spawn_daemon(&state_dir, holder_port));
    wait_for_healthz(holder_port);
    let holder = cleanup.first.as_mut().expect("holder should exist");
    let result = unsafe { libc::kill(holder.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(result, 0, "holder should receive SIGKILL");
    let status = holder.wait().expect("killed holder should be reaped");
    assert!(
        !status.success(),
        "SIGKILL holder should not exit successfully"
    );

    let successor_port = unused_local_port();
    cleanup.second = Some(spawn_daemon(&state_dir, successor_port));
    wait_for_healthz(successor_port);
}

#[test]
fn daemon_rejects_state_sqlite_symlink_after_lock_acquisition_before_protocol_recovery() {
    let state_dir = unique_test_dir("daemon-state-symlink");
    fs::create_dir(&state_dir).expect("test state directory should be created");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
        .expect("test state directory should be private");
    let target = state_dir.join("target.sqlite");
    fs::write(&target, b"must not be opened").expect("target should exist");
    std::os::unix::fs::symlink(&target, state_dir.join("daemon-state.sqlite"))
        .expect("state symlink should be created");

    let mut daemon = spawn_daemon(&state_dir, unused_local_port());
    let output = wait_for_exit(&mut daemon, Duration::from_secs(5));
    assert!(!output.status.success(), "daemon must reject state symlink");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to open sqlite symlink"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&target).expect("target should remain readable"),
        b"must not be opened"
    );
    fs::remove_dir_all(state_dir).expect("test state directory should be removed");
}

#[test]
fn daemon_rejects_replaceable_state_ancestor_before_state_recovery() {
    let state_dir = unique_test_dir("daemon-state-unsafe-ancestor");
    fs::create_dir(&state_dir).expect("test state directory should be created");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o777))
        .expect("test state directory should be replaceable");

    let mut daemon = spawn_daemon(&state_dir, unused_local_port());
    let output = wait_for_exit(&mut daemon, Duration::from_secs(5));
    assert!(
        !output.status.success(),
        "daemon must reject unsafe state parent"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("state lock path is unsafe"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !state_dir.join("daemon-state.sqlite").exists(),
        "unsafe state parent must not receive a state file"
    );
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
        .expect("test state directory should become removable");
    fs::remove_dir_all(state_dir).expect("test state directory should be removed");
}

fn seed_running_session(state_path: &Path) -> SessionStateRecord {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        binary, state_path,
    ));
    runtime
        .create_session_with_id(
            &session_id_text,
            CommandSpec::new("sh").args(["-lc", "printf single-instance-ready; cat"]),
            TerminalSize::cells(24, 80),
        )
        .expect("seed supervisor should start");
    runtime
        .attach(&session_id_text, "seed-device")
        .expect("seed device should attach");
    wait_for_supervisor_output(&mut runtime, &session_id_text, b"single-instance-ready");

    let sessions = runtime.persisted_sessions();
    assert_eq!(sessions.len(), 1, "seed session should be persistent");
    StateStore::save(
        state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: sessions.clone(),
        },
    )
    .expect("seed state should persist");
    sessions.into_iter().next().expect("one seed session")
}

fn supervisor_pid(session: &SessionStateRecord) -> u32 {
    match session.restore_info.as_ref() {
        Some(PtyRestoreInfo::UnixSocket { supervisor_pid, .. }) => *supervisor_pid,
        _ => panic!("seed session must use a supervisor socket"),
    }
}

fn spawn_daemon(state_dir: &Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_termd"))
        .args(["--listen", &format!("127.0.0.1:{port}")])
        .current_dir(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon should spawn")
}

fn unused_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("temporary listener should bind")
        .local_addr()
        .expect("temporary listener should have an address")
        .port()
}

fn wait_for_healthz(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream
                    .write_all(
                        b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .expect("healthz request should write");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("healthz response should read");
                assert!(response.starts_with("HTTP/1.1 200"), "response={response}");
                return;
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("first daemon did not become healthy: {error}"),
        }
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> DaemonExit {
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .expect("second daemon status should be readable")
            .is_some()
        {
            let status = child.wait().expect("second daemon should be reaped");
            return DaemonExit {
                status,
                stdout: read_child_pipe(&mut child.stdout),
                stderr: read_child_pipe(&mut child.stderr),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("timed out daemon should be reaped");
            let output = DaemonExit {
                status,
                stdout: read_child_pipe(&mut child.stdout),
                stderr: read_child_pipe(&mut child.stderr),
            };
            panic!(
                "second daemon did not fail before recovery; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct DaemonExit {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn read_child_pipe(pipe: &mut Option<impl Read>) -> Vec<u8> {
    let mut output = Vec::new();
    if let Some(pipe) = pipe {
        pipe.read_to_end(&mut output)
            .expect("daemon output pipe should read");
    }
    output
}

fn wait_for_supervisor_output(
    runtime: &mut SessionRuntime<SupervisorPtyBackend>,
    session_id: &str,
    needle: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut buffer = [0_u8; 1024];
    while Instant::now() < deadline {
        let read = runtime
            .read_output(session_id, &mut buffer)
            .expect("seed supervisor output should read");
        output.extend_from_slice(&buffer[..read]);
        if output.windows(needle.len()).any(|window| window == needle) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "seed supervisor did not produce {:?}: {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&output)
    );
}

struct DaemonCleanup {
    first: Option<Child>,
    second: Option<Child>,
    state_dir: PathBuf,
    state_path: PathBuf,
    session: SessionStateRecord,
}

impl DaemonCleanup {
    fn new(state_dir: PathBuf, state_path: PathBuf, session: SessionStateRecord) -> Self {
        Self {
            first: None,
            second: None,
            state_dir,
            state_path,
            session,
        }
    }
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        for child in [&mut self.second, &mut self.first].into_iter().flatten() {
            stop_child(child);
        }

        let binary = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
        let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
            binary,
            &self.state_path,
        ));
        let closed = if runtime.reconnect_session(&self.session).is_ok() {
            let session_id = self.session.session_id.0.to_string();
            runtime.attach(&session_id, "cleanup-device").is_ok()
                && runtime.close(&session_id).is_ok()
        } else {
            false
        };
        if !closed
            && let Some(PtyRestoreInfo::UnixSocket { supervisor_pid, .. }) =
                self.session.restore_info.as_ref()
        {
            let _ = unsafe { libc::kill(*supervisor_pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

fn unique_test_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}
