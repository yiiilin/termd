use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use termd::auth::current_unix_timestamp_millis;
use termd::config::DaemonConfig;
use termd::net::protocol::{
    DaemonProtocol, JsonEnvelope, ProtocolConnection, decode_payload,
    encrypted_frame_from_envelope, envelope_value,
};
use termd::net::server::try_default_protocol;
use termd::net::signature::Ed25519SignatureVerifier;
use termd::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
use termd::pty::supervisor::SupervisorPtyBackend;
use termd::pty::{
    CommandSpec, PtyBackend, PtyRestoreInfo, PtySession, PtySize, PtySupervisorStatus,
    PtyTerminalFrame,
};
use termd::runtime::SessionRuntime;
use termd::session::TerminalSize;
use termd::state::{DaemonIdentitySnapshot, DaemonState, SessionStateRecord, StateStore};
use termd_proto::{
    DeviceId, E2eeKeyExchangePayload, MessageType, Nonce, PairAcceptPayload, PairRequestPayload,
    PublicKey, ServerId, SessionClosePayload, SessionClosedPayload, SessionId, SessionListPayload,
    SessionListResultPayload, SessionState, UnixTimestampMillis,
};

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

fn nonce() -> Nonce {
    Nonce(uuid::Uuid::new_v4().to_string())
}

fn ed25519_wire(bytes: &[u8]) -> String {
    format!("ed25519-v1:{}", general_purpose::STANDARD.encode(bytes))
}

fn open_e2ee(
    protocol: &mut DaemonProtocol<SupervisorPtyBackend, Ed25519SignatureVerifier>,
    connection: &mut ProtocolConnection,
    device_id: DeviceId,
) -> E2eeSession {
    let device_keypair = E2eeKeyPair::generate();
    let context = E2eeSessionContext::new(
        protocol.server_id(),
        device_id,
        protocol.e2ee_public_key(),
        device_keypair.public_key(),
    );
    let device_session = E2eeSession::new(
        E2eeSessionRole::Device,
        &device_keypair,
        protocol.e2ee_public_key(),
        context,
    )
    .unwrap();
    let handshake = envelope_value(
        MessageType::E2eeKeyExchange,
        E2eeKeyExchangePayload::new(
            protocol.server_id(),
            device_id,
            device_keypair.public_key_wire(),
            nonce(),
            current_unix_timestamp_millis(),
        ),
    )
    .unwrap();

    // 首次配对设备还不受信任，key exchange 只建立 E2EE，不会返回 auth challenge。
    let responses = connection.handle_wire_envelope(protocol, handshake);
    assert!(responses.is_empty());

    device_session
}

fn send_encrypted(
    protocol: &mut DaemonProtocol<SupervisorPtyBackend, Ed25519SignatureVerifier>,
    connection: &mut ProtocolConnection,
    device_session: &mut E2eeSession,
    inner: JsonEnvelope,
) -> Vec<JsonEnvelope> {
    let frame = device_session.encrypt_json_payload(&inner).unwrap();
    let outer = envelope_value(MessageType::EncryptedFrame, frame).unwrap();

    connection.handle_wire_envelope(protocol, outer)
}

fn decrypt_first(device_session: &mut E2eeSession, messages: Vec<JsonEnvelope>) -> JsonEnvelope {
    let frame = encrypted_frame_from_envelope(messages.into_iter().next().unwrap()).unwrap();
    device_session.decrypt_json_payload(&frame).unwrap()
}

fn pair_connection(
    protocol: &mut DaemonProtocol<SupervisorPtyBackend, Ed25519SignatureVerifier>,
    connection: &mut ProtocolConnection,
    device_session: &mut E2eeSession,
    device_id: DeviceId,
) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let token = protocol
        .issue_pairing_token(current_unix_timestamp_millis())
        .unwrap()
        .token()
        .clone();
    let pair_request = envelope_value(
        MessageType::PairRequest,
        PairRequestPayload {
            device_id,
            device_public_key: PublicKey(ed25519_wire(signing_key.verifying_key().as_bytes())),
            token,
            nonce: nonce(),
            timestamp_ms: current_unix_timestamp_millis(),
        },
    )
    .unwrap();

    let responses = send_encrypted(protocol, connection, device_session, pair_request);
    let response = decrypt_first(device_session, responses);
    let accepted: PairAcceptPayload = decode_payload(response.payload).unwrap();

    assert_eq!(response.kind, MessageType::PairAccept);
    assert_eq!(accepted.device_id, device_id);
    assert!(connection.is_authenticated());
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
fn daemon_session_list_shows_restored_supervisor_without_client_history_metadata() {
    let state_path = temp_state_path("protocol-restore-list.json");
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_termd"));
    let session_id = SessionId::new();
    let session_id_text = session_id.0.to_string();
    let default_root = std::env::temp_dir().canonicalize().unwrap();
    let default_name = format!("restored-{}", &session_id_text[..8]);

    let mut runtime = SessionRuntime::new(SupervisorPtyBackend::with_binary_and_state_path(
        &binary_path,
        &state_path,
    ));
    runtime
        .create_session_with_id(
            &session_id_text,
            CommandSpec::new("sh").args(["-lc", "printf listed-after-restore && cat"]),
            TerminalSize::cells(24, 80),
        )
        .unwrap();
    runtime.attach(&session_id_text, "test-device").unwrap();
    read_until_contains(&mut runtime, &session_id_text, b"listed-after-restore");

    let persisted_sessions = runtime.persisted_sessions();
    assert_eq!(persisted_sessions.len(), 1);
    StateStore::save(
        &state_path,
        &DaemonState {
            version: termd::state::STATE_SCHEMA_VERSION,
            daemon_identity: None,
            trusted_devices: Vec::new(),
            sessions: persisted_sessions,
        },
    )
    .unwrap();
    drop(runtime);

    let mut config = DaemonConfig::default_for_state_path(&state_path);
    config.default_working_directory = Some(default_root.clone());
    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let state = StateStore::load(&state_path).unwrap();
    let mut protocol =
        DaemonProtocol::from_state(config, backend, Ed25519SignatureVerifier, state).unwrap();
    let (mut connection, _) = protocol.start_connection();
    let device_id = DeviceId::new();
    let mut device_session = open_e2ee(&mut protocol, &mut connection, device_id);
    pair_connection(
        &mut protocol,
        &mut connection,
        &mut device_session,
        device_id,
    );

    let list_responses = send_encrypted(
        &mut protocol,
        &mut connection,
        &mut device_session,
        envelope_value(MessageType::SessionList, SessionListPayload {}).unwrap(),
    );
    let list = decrypt_first(&mut device_session, list_responses);
    let list_payload: SessionListResultPayload = decode_payload(list.payload).unwrap();

    assert_eq!(list.kind, MessageType::SessionListResult);
    assert_eq!(list_payload.sessions.len(), 1);
    assert_eq!(list_payload.sessions[0].session_id, session_id);
    assert_eq!(
        list_payload.sessions[0].name.as_deref(),
        Some(default_name.as_str())
    );
    assert_eq!(
        list_payload.sessions[0].files_path.as_deref(),
        Some(default_root.to_string_lossy().as_ref())
    );

    let close_responses = send_encrypted(
        &mut protocol,
        &mut connection,
        &mut device_session,
        envelope_value(
            MessageType::SessionClose,
            SessionClosePayload { session_id },
        )
        .unwrap(),
    );
    let closed = decrypt_first(&mut device_session, close_responses);
    let closed_payload: SessionClosedPayload = decode_payload(closed.payload).unwrap();
    assert_eq!(closed.kind, MessageType::SessionClosed);
    assert_eq!(closed_payload.session_id, session_id);
}

#[test]
#[cfg(target_os = "linux")]
fn daemon_startup_adopts_live_supervisor_when_runtime_row_is_missing() {
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
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .expect("test supervisor should spawn");
    let supervisor_pid = supervisor.id();
    assert!(
        linux_process_state(supervisor_pid).is_some(),
        "test supervisor should be alive before daemon startup"
    );
    let backend = SupervisorPtyBackend::with_binary_and_state_path(&binary_path, &state_path);
    let mut candidates = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        // 子进程刚 exec 时 `/proc` 扫描可能短暂看不到完整 argv，测试侧等待到可发现为止。
        candidates = backend.live_supervisor_restore_candidates().unwrap();
        if candidates
            .iter()
            .any(|candidate| candidate.session_id == session_id)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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

    let config = DaemonConfig::default_for_state_path(&state_path);
    let protocol = try_default_protocol(config).unwrap();

    let reloaded = StateStore::load(&state_path).unwrap();
    assert_eq!(reloaded.sessions.len(), 1);
    assert_eq!(reloaded.sessions[0].session_id.0.to_string(), session_id);
    assert_eq!(reloaded.sessions[0].state, SessionState::Running);
    assert!(reloaded.sessions[0].restore_info.is_some());
    assert!(
        linux_process_state(supervisor_pid).is_some(),
        "startup must adopt the live supervisor instead of cleaning it as orphan"
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
