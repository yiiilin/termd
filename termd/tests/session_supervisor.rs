use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use termd::pty::supervisor::SupervisorPtyBackend;
use termd::pty::{
    CommandSpec, PtyBackend, PtyRestoreInfo, PtySession, PtySize, PtySupervisorStatus,
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

fn spawn_supervisor_from_short_lived_parent(
    binary_path: &Path,
    session_id: &str,
    socket_path: &Path,
    command: &CommandSpec,
    size: PtySize,
    pid_file: &Path,
) -> u32 {
    let command_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(command).expect("command serializes"));
    let size_base64 =
        general_purpose::STANDARD.encode(serde_json::to_vec(&size).expect("size serializes"));
    let script = r#"
"$TERMD_BIN" __session-supervisor \
  --session-id "$SESSION_ID" \
  --socket-path "$SOCKET_PATH" \
  --command-base64 "$COMMAND_BASE64" \
  --size-base64 "$SIZE_BASE64" \
  >/dev/null 2>&1 &
echo "$!" > "$PID_FILE"
"#;

    // 用短生命周期 shell 启动 supervisor，shell 退出后测试进程只持有 restore_info，
    // 不持有 Child 句柄，从而覆盖真实父进程退出后的重连路径。
    let mut helper = ProcessCommand::new("sh")
        .arg("-c")
        .arg(script)
        .env("TERMD_BIN", binary_path)
        .env("SESSION_ID", session_id)
        .env("SOCKET_PATH", socket_path)
        .env("COMMAND_BASE64", command_base64)
        .env("SIZE_BASE64", size_base64)
        .env("PID_FILE", pid_file)
        .spawn()
        .expect("short-lived supervisor launcher should spawn");
    let helper_pid = helper.id();
    let status = helper
        .wait()
        .expect("short-lived supervisor launcher should exit");
    assert!(status.success(), "short-lived launcher failed: {status}");

    let supervisor_pid = fs::read_to_string(pid_file)
        .expect("short-lived launcher should write supervisor pid")
        .trim()
        .parse::<u32>()
        .expect("supervisor pid should be numeric");

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
    let mut session = backend.reconnect(session_id, &restore_info).unwrap();

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
