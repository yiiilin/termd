use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use termd::auth::current_unix_timestamp_millis;
use termd::config::DaemonConfig;
use termd::net::server::{SharedDaemonProtocol, serve_listener};
use termd::pty::PtyRestoreInfo;
use termd::pty::supervisor::SupervisorPtyBackend;
use termd::runtime::SessionRuntime;
use termd::state::StateStore;
use termd_proto::{PairingQrPayload, ServerId, SessionState};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TERMD_READY_SENTINEL: &str = "termd-e2e-ready";

struct TestDaemon {
    url: String,
    protocol: Option<SharedDaemonProtocol>,
    state_path: PathBuf,
    supervisor_guards: Vec<DirectE2eSupervisorGuard>,
    state_dir: TempDir,
    task: Option<JoinHandle<()>>,
    task_finished: mpsc::Receiver<()>,
}

impl TestDaemon {
    async fn spawn() -> Self {
        let state_dir = tempfile::tempdir().expect("daemon state tempdir should be created");
        let state_path = state_dir.path().join("daemon-state.json");
        let mut config = DaemonConfig::default_for_state_path(&state_path);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("daemon test listener should bind");
        let addr = listener
            .local_addr()
            .expect("daemon test listener should expose local addr");

        config.listen_host = "127.0.0.1".to_owned();
        config.listen_port = addr.port();

        let protocol = termd::net::server::default_protocol(config);
        let server_protocol = Arc::clone(&protocol);
        let (task_finished_tx, task_finished) = mpsc::channel();
        let task = tokio::spawn(async move {
            struct Finished(mpsc::Sender<()>);
            impl Drop for Finished {
                fn drop(&mut self) {
                    let _ = self.0.send(());
                }
            }
            let _finished = Finished(task_finished_tx);
            serve_listener(listener, server_protocol, false)
                .await
                .expect("in-process daemon should keep serving");
        });

        Self {
            url: format!("ws://{addr}/ws"),
            protocol: Some(protocol),
            state_path,
            supervisor_guards: Vec::new(),
            state_dir,
            task: Some(task),
            task_finished,
        }
    }

    fn protocol(&self) -> &SharedDaemonProtocol {
        self.protocol
            .as_ref()
            .expect("test daemon protocol should be available")
    }

    fn track_session_supervisor(&mut self, session_id: &str) {
        let state = StateStore::load(&self.state_path).expect("daemon state should load");
        let session = state
            .sessions
            .iter()
            .find(|session| session.session_id.0.to_string() == session_id)
            .expect("created session should be durable before termctl new returns");
        let Some(PtyRestoreInfo::UnixSocket {
            socket_path,
            supervisor_pid,
            ..
        }) = session.restore_info.as_ref()
        else {
            panic!("direct daemon E2E requires Unix supervisor restore info");
        };
        let mut guard = DirectE2eSupervisorGuard::new(
            *supervisor_pid,
            socket_path.clone(),
            self.state_dir.path().to_path_buf(),
        );
        assert!(
            guard.confirm_ownership(),
            "created supervisor PID/socket must belong to this fixture"
        );
        self.supervisor_guards.push(guard);
    }

    async fn issue_pairing_invite(&self) -> String {
        let protocol = self.protocol().lock().await;
        let server_id = protocol.server_id();
        let daemon_public_key = protocol.daemon_public_identity().public_key.clone();
        let (ticket, expires_at_ms) = protocol
            .issue_pair_ticket_credential(current_unix_timestamp_millis())
            .expect("pair ticket should be issued");

        PairingQrPayload::new(termd_proto::PairingToken(ticket), server_id, expires_at_ms)
            .with_daemon_public_key(daemon_public_key)
            .to_invite_code()
    }

    async fn issue_pairing_invite_for_server(&self, server_id: ServerId) -> String {
        let protocol = self.protocol().lock().await;
        let daemon_public_key = protocol.daemon_public_identity().public_key.clone();
        let (ticket, expires_at_ms) = protocol
            .issue_pair_ticket_credential(current_unix_timestamp_millis())
            .expect("pair ticket should be issued");

        PairingQrPayload::new(termd_proto::PairingToken(ticket), server_id, expires_at_ms)
            .with_daemon_public_key(daemon_public_key)
            .to_invite_code()
    }

    async fn issue_pairing_token(&self) -> String {
        let protocol = self.protocol().lock().await;
        protocol
            .issue_pair_ticket_credential(current_unix_timestamp_millis())
            .expect("pair ticket should be issued")
            .0
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let state = StateStore::load(&self.state_path).ok();
        if let Some(state) = &state {
            for session in &state.sessions {
                let Some(PtyRestoreInfo::UnixSocket {
                    socket_path,
                    supervisor_pid,
                    ..
                }) = session.restore_info.as_ref()
                else {
                    continue;
                };
                if self
                    .supervisor_guards
                    .iter()
                    .any(|guard| guard.pid == *supervisor_pid && guard.socket_path == *socket_path)
                {
                    continue;
                }
                let mut guard = DirectE2eSupervisorGuard::new(
                    *supervisor_pid,
                    socket_path.clone(),
                    self.state_dir.path().to_path_buf(),
                );
                if guard.confirm_ownership() {
                    self.supervisor_guards.push(guard);
                }
            }
        }
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = self.task_finished.recv();
        }
        self.protocol.take();

        let Some(state) = state else {
            self.supervisor_guards.clear();
            return;
        };
        let backend = SupervisorPtyBackend::for_state_path(&self.state_path);
        let mut runtime = SessionRuntime::new(backend);
        for session in state
            .sessions
            .iter()
            .filter(|session| session.state == SessionState::Running)
        {
            let Some(PtyRestoreInfo::UnixSocket {
                socket_path,
                supervisor_pid,
                ..
            }) = session.restore_info.as_ref()
            else {
                continue;
            };
            if !self
                .supervisor_guards
                .iter()
                .any(|guard| guard.pid == *supervisor_pid && guard.socket_path == *socket_path)
            {
                continue;
            }
            let session_id = session.session_id.0.to_string();
            let _ = runtime
                .reconnect_session(session)
                .and_then(|()| runtime.close(&session_id));
        }
        self.supervisor_guards.clear();
    }
}

struct DirectE2eSupervisorGuard {
    pid: u32,
    socket_path: PathBuf,
    state_dir: PathBuf,
    ownership_confirmed: bool,
}

impl DirectE2eSupervisorGuard {
    fn new(pid: u32, socket_path: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            pid,
            socket_path,
            state_dir,
            ownership_confirmed: false,
        }
    }

    fn confirm_ownership(&mut self) -> bool {
        self.ownership_confirmed = self.owns_live_process();
        self.ownership_confirmed
    }

    fn owns_live_process(&self) -> bool {
        if !self.socket_path.starts_with(&self.state_dir) {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;

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
                    pair[0] == b"--socket-path"
                        && pair[1] == self.socket_path.as_os_str().as_bytes()
                })
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.socket_path.exists()
        }
    }
}

impl Drop for DirectE2eSupervisorGuard {
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
            #[cfg(target_os = "linux")]
            {
                let mut status = 0;
                let waited =
                    unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if waited == self.pid as libc::pid_t
                    || !Path::new(&format!("/proc/{}", self.pid)).exists()
                {
                    break;
                }
            }
            #[cfg(not(target_os = "linux"))]
            if !self.socket_path.exists() {
                break;
            }
            thread::yield_now();
        }
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(self.socket_path.with_extension("attach.sock"));
    }
}

struct AttachGuard {
    child: Child,
}

impl AttachGuard {
    fn spawn(state_path: &Path, url: &str, session_id: &str) -> Self {
        let child = base_termctl_command(state_path)
            .args(["attach", session_id, "--url", url])
            // attach 在测试中只负责保持已连接状态；业务输出不参与断言。
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("termctl attach should spawn");

        Self { child }
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_termctl_binary_covers_session_flow_and_invariants() {
    let mut daemon = TestDaemon::spawn().await;
    let temp = tempfile::tempdir().expect("termctl state tempdir should be created");
    let paired_state = temp.path().join("paired-state.json");
    let unpaired_state = temp.path().join("unpaired-state.json");

    let bad_token_only_pair = run_termctl_failure(
        &paired_state,
        &["pair", "--token", "wrong-token", "--url", &daemon.url],
    );
    let bad_token_only_stderr = stderr_string(&bad_token_only_pair);
    assert!(bad_token_only_stderr.contains("token_requires_known_daemon"));
    assert!(!bad_token_only_stderr.contains("wrong-token"));

    let wrong_route_invite = daemon
        .issue_pairing_invite_for_server(ServerId::new())
        .await;
    let wrong_route_pair = run_termctl_failure(
        &paired_state,
        &[
            "pair",
            "--payload",
            &wrong_route_invite,
            "--url",
            &daemon.url,
        ],
    );
    let wrong_route_stderr = stderr_string(&wrong_route_pair);
    assert!(
        wrong_route_stderr.contains("pairing_payload_server_mismatch"),
        "stderr was: {wrong_route_stderr}"
    );
    assert!(!wrong_route_stderr.contains("termd-pair"));

    let invite = daemon.issue_pairing_invite().await;
    let pair = run_termctl_success(&paired_state, &["pair", &invite, "--url", &daemon.url]);
    assert!(stdout_string(&pair).contains("paired server="));

    let state_after_pair =
        fs::read_to_string(&paired_state).expect("paired state should be readable");
    assert!(!state_after_pair.contains("termd-pair"));
    assert!(!state_after_pair.contains("pairing_token"));
    assert!(!state_after_pair.contains("server_private_key"));

    let bad_known_pair = run_termctl_failure(
        &paired_state,
        &["pair", "--token", "wrong-token", "--url", &daemon.url],
    );
    let bad_known_pair_stderr = stderr_string(&bad_known_pair);
    assert!(
        bad_known_pair_stderr.contains("pair_ticket_invalid"),
        "stderr was: {bad_known_pair_stderr}"
    );
    assert!(!bad_known_pair_stderr.contains("wrong-token"));

    let second_token = daemon.issue_pairing_token().await;
    let second_pair = run_termctl_success(
        &paired_state,
        &["pair", "--token", &second_token, "--url", &daemon.url],
    );
    assert!(stdout_string(&second_pair).contains("paired server="));

    let unpaired_new = run_termctl_failure(
        &unpaired_state,
        &["new", "--url", &daemon.url, "--", "/bin/sh", "-lc", "true"],
    );
    let unpaired_stderr = stderr_string(&unpaired_new);
    assert!(unpaired_stderr.contains("missing_pairing"));
    assert!(!unpaired_stderr.contains("termd-pair"));
    assert!(!unpaired_stderr.contains(TERMD_READY_SENTINEL));

    let command = format!("printf {TERMD_READY_SENTINEL}; sleep 5");
    let new_session = run_termctl_success(
        &paired_state,
        &[
            "new",
            "--url",
            &daemon.url,
            "--",
            "/bin/sh",
            "-lc",
            &command,
        ],
    );
    let session_id = parse_session_id(&stdout_string(&new_session));
    daemon.track_session_supervisor(&session_id);

    let list_after_new = run_termctl_success(&paired_state, &["list", "--url", &daemon.url]);
    let list_stdout = stdout_string(&list_after_new);
    assert!(list_stdout.contains(&session_id));
    assert!(list_stdout.contains("state=running"));

    let json_list_after_new =
        run_termctl_success(&paired_state, &["--json", "list", "--url", &daemon.url]);
    let json_list: serde_json::Value =
        serde_json::from_slice(&json_list_after_new.stdout).expect("list JSON should parse");
    assert!(
        json_list["sessions"]
            .as_array()
            .expect("sessions should be an array")
            .iter()
            .any(|session| session["session_id"] == session_id)
    );

    let attach = AttachGuard::spawn(&paired_state, &daemon.url, &session_id);
    let control_stdout = run_control_until_success(&paired_state, &daemon.url, &session_id);
    assert!(control_stdout.contains("control_granted"));
    assert!(control_stdout.contains(&session_id));

    // resize owner 只属于当前持有尺寸权的 attach 连接；短连接 CLI resize 需要等旧 attach 释放。
    drop(attach);

    let resize = run_resize_until_success(&paired_state, &daemon.url, &session_id, "40", "120");
    assert!(stdout_string(&resize).contains("size=40x120"));

    let list_after_detach = run_termctl_success(&paired_state, &["list", "--url", &daemon.url]);
    let list_after_detach_stdout = stdout_string(&list_after_detach);
    assert!(list_after_detach_stdout.contains(&session_id));
    assert!(list_after_detach_stdout.contains("state=running"));
    assert!(list_after_detach_stdout.contains("size=40x120"));

    let state_after_session =
        fs::read_to_string(&paired_state).expect("paired state should remain readable");
    assert!(!state_after_session.contains("termd-pair"));
    assert!(!state_after_session.contains("server_private_key"));
    assert!(!state_after_session.contains(TERMD_READY_SENTINEL));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_termctl_close_uses_http_without_attaching_session() {
    let daemon = TestDaemon::spawn().await;
    let temp = tempfile::tempdir().expect("termctl state tempdir should be created");
    let paired_state = temp.path().join("paired-state.json");

    let invite = daemon.issue_pairing_invite().await;
    let pair = run_termctl_success(
        &paired_state,
        &["pair", "--payload", &invite, "--url", &daemon.url],
    );
    assert!(stdout_string(&pair).contains("paired server="));

    let new_session = run_termctl_success(
        &paired_state,
        &[
            "new",
            "--url",
            &daemon.url,
            "--",
            "/bin/sh",
            "-lc",
            "sleep 5",
        ],
    );
    let session_id = parse_session_id(&stdout_string(&new_session));

    // v0.7 close is one bearer-authenticated JSON request and does not attach first.
    let close = run_termctl_success(&paired_state, &["close", &session_id, "--url", &daemon.url]);
    let close_stdout = stdout_string(&close);
    assert!(close_stdout.contains("closed session="));
    assert!(close_stdout.contains(&session_id));

    let list_after_close = run_termctl_success(&paired_state, &["list", "--url", &daemon.url]);
    assert!(!stdout_string(&list_after_close).contains(&session_id));
}

fn run_control_until_success(state_path: &Path, url: &str, session_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);

    loop {
        let output = run_termctl_raw(
            state_path,
            &["control", session_id, "--url", url],
            Stdio::null(),
            Stdio::piped(),
            Stdio::piped(),
        );
        if output.status.success() {
            return stdout_string(&output);
        }

        let last_stderr = stderr_string(&output);
        assert!(
            Instant::now() < deadline,
            "control did not succeed before timeout; last stderr:\n{last_stderr}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_resize_until_success(
    state_path: &Path,
    url: &str,
    session_id: &str,
    rows: &str,
    cols: &str,
) -> Output {
    let deadline = Instant::now() + Duration::from_secs(3);

    loop {
        let output = run_termctl_raw(
            state_path,
            &[
                "resize", session_id, "--rows", rows, "--cols", cols, "--url", url,
            ],
            Stdio::null(),
            Stdio::piped(),
            Stdio::piped(),
        );
        if output.status.success() {
            return output;
        }

        let last_stderr = stderr_string(&output);
        assert!(
            Instant::now() < deadline,
            "resize did not succeed before timeout; last stderr:\n{last_stderr}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_termctl_success(state_path: &Path, args: &[&str]) -> Output {
    let redacted_args = redacted_termctl_args(args);
    let output = run_termctl_raw(
        state_path,
        args,
        Stdio::null(),
        Stdio::piped(),
        Stdio::piped(),
    );
    assert!(
        output.status.success(),
        "termctl {:?} failed\nstdout:\n{}\nstderr:\n{}",
        redacted_args,
        stdout_string(&output),
        stderr_string(&output)
    );
    output
}

fn run_termctl_failure(state_path: &Path, args: &[&str]) -> Output {
    let redacted_args = redacted_termctl_args(args);
    let output = run_termctl_raw(
        state_path,
        args,
        Stdio::null(),
        Stdio::piped(),
        Stdio::piped(),
    );
    assert!(
        !output.status.success(),
        "termctl {:?} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        redacted_args,
        stdout_string(&output),
        stderr_string(&output)
    );
    output
}

fn run_termctl_raw(
    state_path: &Path,
    args: &[&str],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Output {
    base_termctl_command(state_path)
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .output()
        .expect("termctl binary should run")
}

fn base_termctl_command(state_path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_termctl"));
    command.arg("--state").arg(state_path);
    command
}

fn redacted_termctl_args(args: &[&str]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(args.len());
    let mut index = 0;

    while index < args.len() {
        let arg = args[index];
        match arg {
            "pair" => {
                redacted.push(arg.to_owned());
                if let Some(invite) = args.get(index + 1)
                    && !invite.starts_with('-')
                {
                    redacted.push("<pairing-invite>".to_owned());
                    index += 2;
                    continue;
                }
            }
            "--token" | "--payload" => {
                redacted.push(arg.to_owned());
                if args.get(index + 1).is_some() {
                    redacted.push("<redacted>".to_owned());
                    index += 2;
                    continue;
                }
            }
            _ if looks_like_pairing_invite(arg) => {
                redacted.push("<pairing-invite>".to_owned());
                index += 1;
                continue;
            }
            _ => {}
        }

        redacted.push(arg.to_owned());
        index += 1;
    }

    redacted
}

fn looks_like_pairing_invite(value: &str) -> bool {
    value.starts_with("termd-pair:")
}

fn parse_session_id(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("session="))
        .expect("termctl new output should include session=<uuid>")
        .to_owned()
}

fn stdout_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn helper_redacts_pairing_invite_and_secret_flag_values() {
    let redacted = redacted_termctl_args(&[
        "pair",
        "termd-pair:invite-secret",
        "--token",
        "token-secret",
        "--payload",
        "payload-secret",
        "--url",
        "ws://127.0.0.1:8765/ws",
    ]);

    let joined = redacted.join(" ");

    assert!(!joined.contains("invite-secret"));
    assert!(!joined.contains("token-secret"));
    assert!(!joined.contains("payload-secret"));
    assert!(joined.contains("<pairing-invite>"));
    assert_eq!(joined.matches("<redacted>").count(), 2);
}
