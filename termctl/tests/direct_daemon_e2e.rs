use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use termd::auth::current_unix_timestamp_millis;
use termd::config::DaemonConfig;
use termd::net::server::{DefaultDaemonProtocol, serve_listener};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TERMD_READY_SENTINEL: &str = "termd-e2e-ready";

struct TestDaemon {
    url: String,
    protocol: Arc<Mutex<DefaultDaemonProtocol>>,
    _state_dir: TempDir,
    task: JoinHandle<()>,
}

impl TestDaemon {
    async fn spawn() -> Self {
        let state_dir = tempfile::tempdir().expect("daemon state tempdir should be created");
        let mut config =
            DaemonConfig::default_for_state_path(state_dir.path().join("daemon-state.json"));
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
        let task = tokio::spawn(async move {
            serve_listener(listener, server_protocol, false)
                .await
                .expect("in-process daemon should keep serving");
        });

        Self {
            url: format!("ws://{addr}/ws"),
            protocol,
            _state_dir: state_dir,
            task,
        }
    }

    fn issue_pairing_token(&self) -> String {
        let mut protocol = self
            .protocol
            .lock()
            .expect("daemon protocol mutex should not be poisoned");
        protocol
            .issue_pairing_token(current_unix_timestamp_millis())
            .expect("pairing token should be issued")
            .token()
            .0
            .clone()
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.task.abort();
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
    let daemon = TestDaemon::spawn().await;
    let temp = tempfile::tempdir().expect("termctl state tempdir should be created");
    let paired_state = temp.path().join("paired-state.json");
    let unpaired_state = temp.path().join("unpaired-state.json");

    let bad_pair = run_termctl_failure(
        &paired_state,
        &["pair", "--token", "wrong-token", "--url", &daemon.url],
    );
    let bad_pair_stderr = stderr_string(&bad_pair);
    assert!(bad_pair_stderr.contains("pairing_failed"));
    assert!(!bad_pair_stderr.contains("wrong-token"));

    let token = daemon.issue_pairing_token();
    let pair = run_termctl_success(
        &paired_state,
        &["pair", "--token", &token, "--url", &daemon.url],
    );
    assert!(stdout_string(&pair).contains("paired server="));

    let state_after_pair =
        fs::read_to_string(&paired_state).expect("paired state should be readable");
    assert!(!state_after_pair.contains(&token));
    assert!(!state_after_pair.contains("pairing_token"));
    assert!(!state_after_pair.contains("server_private_key"));

    let unpaired_new = run_termctl_failure(
        &unpaired_state,
        &["new", "--url", &daemon.url, "--", "/bin/sh", "-lc", "true"],
    );
    let unpaired_stderr = stderr_string(&unpaired_new);
    assert!(unpaired_stderr.contains("missing_pairing"));
    assert!(!unpaired_stderr.contains(&token));
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

    let list_after_new = run_termctl_success(&paired_state, &["list", "--url", &daemon.url]);
    let list_stdout = stdout_string(&list_after_new);
    assert!(list_stdout.contains(&session_id));
    assert!(list_stdout.contains("state=running"));

    let attach = AttachGuard::spawn(&paired_state, &daemon.url, &session_id);
    let control_stdout = run_control_until_success(&paired_state, &daemon.url, &session_id);
    assert!(control_stdout.contains("control_granted"));
    assert!(control_stdout.contains(&session_id));

    let resize = run_termctl_success(
        &paired_state,
        &[
            "resize",
            &session_id,
            "--rows",
            "40",
            "--cols",
            "120",
            "--url",
            &daemon.url,
        ],
    );
    assert!(stdout_string(&resize).contains("size=40x120"));

    drop(attach);

    let list_after_detach = run_termctl_success(&paired_state, &["list", "--url", &daemon.url]);
    let list_after_detach_stdout = stdout_string(&list_after_detach);
    assert!(list_after_detach_stdout.contains(&session_id));
    assert!(list_after_detach_stdout.contains("state=running"));
    assert!(list_after_detach_stdout.contains("size=40x120"));

    let state_after_session =
        fs::read_to_string(&paired_state).expect("paired state should remain readable");
    assert!(!state_after_session.contains(&token));
    assert!(!state_after_session.contains("server_private_key"));
    assert!(!state_after_session.contains(TERMD_READY_SENTINEL));
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

fn run_termctl_success(state_path: &Path, args: &[&str]) -> Output {
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
        args,
        stdout_string(&output),
        stderr_string(&output)
    );
    output
}

fn run_termctl_failure(state_path: &Path, args: &[&str]) -> Output {
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
        args,
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
