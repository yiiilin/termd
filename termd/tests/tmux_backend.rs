use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use termd::pty::tmux::TmuxPtyBackend;
use termd::pty::{CommandSpec, PtyBackend, PtyRestoreInfo, PtySession, PtySize, PtyTerminalFrame};
use termd::runtime::SessionRuntime;
use termd::session::TerminalSize as RuntimeTerminalSize;
use termd::state::{DaemonState, SessionStateRecord, StateStore};
use termd_proto::{SessionId, SessionState, TerminalSize, UnixTimestampMillis};

fn tmux_available() -> bool {
    ProcessCommand::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("td-tmux-{}-{nanos}-{name}", std::process::id()))
}

fn read_until_contains(session: &mut dyn PtySession, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 4096];

    while Instant::now() < deadline {
        let read = session.read(&mut buffer).unwrap();
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

fn read_terminal_frame_until_contains(
    session: &mut dyn PtySession,
    needle: &[u8],
) -> PtyTerminalFrame {
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline {
        if let Some(frame) = session.read_terminal_frame().unwrap() {
            let contains = match &frame {
                PtyTerminalFrame::Output { data, .. } | PtyTerminalFrame::Snapshot { data, .. } => {
                    data.windows(needle.len()).any(|window| window == needle)
                }
                PtyTerminalFrame::Resize { .. } | PtyTerminalFrame::Exit { .. } => false,
            };
            if contains {
                return frame;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!(
        "timed out waiting for terminal frame containing {:?}",
        String::from_utf8_lossy(needle)
    );
}

fn tmux_output(socket_path: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("tmux")
        .arg("-S")
        .arg(socket_path)
        .args(args)
        .output()
        .expect("tmux command should run");
    assert!(
        output.status.success(),
        "tmux command failed: status={:?} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tmux_client_count(socket_path: &Path, session_name: &str) -> usize {
    let clients = tmux_output(
        socket_path,
        &["list-clients", "-t", session_name, "-F", "#{client_pid}"],
    );
    clients
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn wait_for_tmux_client_count(socket_path: &Path, session_name: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if tmux_client_count(socket_path, session_name) == expected {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "timed out waiting for {expected} tmux clients, got {}",
        tmux_client_count(socket_path, session_name)
    );
}

fn cleanup_tmux(socket_path: &Path) {
    let _ = ProcessCommand::new("tmux")
        .arg("-S")
        .arg(socket_path)
        .arg("kill-server")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[test]
#[cfg(unix)]
fn tmux_backend_starts_session_and_bridges_io() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("io");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a001";

    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf ready; cat"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"ready");
    session.write_all(b"hello-from-web\n").unwrap();
    read_until_contains(&mut *session, b"hello-from-web");

    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux {
        socket_path: restored_socket,
        session_name,
    } = restore_info
    else {
        panic!("expected tmux restore info");
    };
    assert_eq!(restored_socket, socket_path);
    assert_eq!(
        tmux_output(
            &socket_path,
            &["display-message", "-pt", &session_name, "#{session_name}"]
        ),
        session_name
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_resize_updates_tmux_window_size() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("resize");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a002";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf sized; cat"]),
            PtySize::new(24, 80),
        )
        .unwrap();
    read_until_contains(&mut *session, b"sized");

    session.resize(PtySize::new(31, 100)).unwrap();
    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info else {
        panic!("expected tmux restore info");
    };

    let size = tmux_output(
        &socket_path,
        &[
            "display-message",
            "-pt",
            &session_name,
            "#{window_width}x#{window_height}",
        ],
    );
    assert_eq!(size, "100x31");

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_snapshot_uses_tmux_capture_pane_after_output_is_drained() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("snapshot");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a004";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf capture-pane-line; sleep 60"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"capture-pane-line");
    let snapshot = session.snapshot().unwrap();

    assert!(
        snapshot
            .retained_output
            .windows(b"capture-pane-line".len())
            .any(|window| window == b"capture-pane-line"),
        "snapshot should come from tmux capture-pane after daemon output cache is drained: {}",
        String::from_utf8_lossy(&snapshot.retained_output)
    );
    let terminal_snapshot = session.terminal_snapshot(None).unwrap();
    let Some(PtyTerminalFrame::Snapshot { base_seq, data, .. }) = terminal_snapshot.first() else {
        panic!("expected tmux terminal snapshot frame");
    };
    assert!(
        data.windows(b"capture-pane-line".len())
            .any(|window| window == b"capture-pane-line"),
        "terminal snapshot should also come from tmux capture-pane: {}",
        String::from_utf8_lossy(data)
    );
    session.write_all(b"live-frame-after-snapshot\n").unwrap();
    let live_frame =
        read_terminal_frame_until_contains(&mut *session, b"live-frame-after-snapshot");
    let PtyTerminalFrame::Output { terminal_seq, .. } = live_frame else {
        panic!("expected live output frame after tmux terminal snapshot");
    };
    assert!(
        terminal_seq > *base_seq,
        "live terminal seq {terminal_seq} must advance past snapshot base_seq {base_seq}"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_sets_history_limit_to_500_before_fast_output() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("history-limit");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a008";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args([
                "-lc",
                "for i in $(seq 1 1600); do echo history-$i; done; printf history-ready; sleep 60",
            ]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"history-ready");
    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info else {
        panic!("expected tmux restore info");
    };

    assert_eq!(
        tmux_output(
            &socket_path,
            &["show-options", "-t", &session_name, "history-limit"]
        ),
        "history-limit 500"
    );
    let captured = tmux_output(
        &socket_path,
        &["capture-pane", "-p", "-S", "-", "-t", &session_name],
    );
    let lines = captured.lines().collect::<Vec<_>>();
    assert!(
        lines.len() <= 520,
        "tmux capture should be bounded near the 500-line history limit, got {} lines",
        lines.len()
    );
    assert!(
        !captured.contains("history-1\n"),
        "early output should not survive once tmux history exceeds 500 lines"
    );
    assert!(
        captured.contains("history-1600"),
        "latest output should remain in tmux history: {captured}"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_clear_cuts_pre_clear_history_from_capture_snapshot() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("clear-history-cut");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a00c";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args([
                "-lc",
                "for i in $(seq 1 80); do echo pre-clear-$i; done; clear; for i in $(seq 1 40); do echo post-clear-$i; done; printf clear-history-ready; sleep 60",
            ]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"clear-history-ready");
    let snapshot = session.snapshot().unwrap();
    let captured = String::from_utf8_lossy(&snapshot.retained_output);

    assert!(
        captured.contains("post-clear-40"),
        "clear 后的最新输出必须仍然存在于 tmux snapshot: {captured}"
    );
    assert!(
        !captured.contains("pre-clear-1") && !captured.contains("pre-clear-80"),
        "tmux clear-history 之后 snapshot 不应再包含 pre-clear pane history: {captured}"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_disables_tmux_status_line_for_managed_sessions() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("status-off");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a00a";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf status-off-ready; sleep 60"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"status-off-ready");
    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info else {
        panic!("expected tmux restore info");
    };

    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "status"]),
        "status off",
        "termd 管理的 tmux server 不能显示 tmux 自带状态栏"
    );
    assert_eq!(
        tmux_output(
            &socket_path,
            &["show-options", "-t", &session_name, "status"]
        ),
        "status off",
        "每个 termd session 也要显式关闭 tmux status line，避免旧 server 配置漏网"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_disables_alternate_screen_switch_for_bridge_term() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("terminal-overrides");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a00d";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf terminal-overrides-ready; sleep 60"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"terminal-overrides-ready");
    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "terminal-overrides"]),
        "terminal-overrides[0] xterm-256color:smcup@:rmcup@",
        "tmux bridge 需要保留 xterm 能力，同时仅关闭 smcup/rmcup，避免 codex/vim 一类 TUI 格式错乱"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_reconnect_repairs_status_line_on_existing_server() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("status-reconnect");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a00b";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf status-reconnect-ready; sleep 60"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"status-reconnect-ready");
    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info.clone() else {
        panic!("expected tmux restore info");
    };

    tmux_output(&socket_path, &["set-option", "-g", "status", "on"]);
    tmux_output(
        &socket_path,
        &["set-option", "-t", &session_name, "status", "on"],
    );
    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "status"]),
        "status on"
    );
    assert_eq!(
        tmux_output(
            &socket_path,
            &["show-options", "-t", &session_name, "status"]
        ),
        "status on"
    );

    let reconnected = backend
        .reconnect(session_id, &restore_info, PtySize::new(24, 80))
        .unwrap();

    // 中文注释：daemon 重启恢复旧 tmux session 时也必须修正旧 server 配置；
    // 不能只覆盖新建 session，否则历史 session 首次打开前仍可能带 tmux 自身状态栏。
    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "status"]),
        "status off"
    );
    assert_eq!(
        tmux_output(
            &socket_path,
            &["show-options", "-t", &session_name, "status"]
        ),
        "status off"
    );

    drop(reconnected);
    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_reconnect_repairs_terminal_overrides_on_existing_server() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("terminal-overrides-reconnect");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a00e";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh")
                .args(["-lc", "printf terminal-overrides-reconnect-ready; sleep 60"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"terminal-overrides-reconnect-ready");
    let restore_info = session.restore_info().expect("tmux sessions persist");

    tmux_output(
        &socket_path,
        &["set-option", "-g", "terminal-overrides", ""],
    );
    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "terminal-overrides"]),
        "terminal-overrides",
        "测试前置条件失败：global terminal-overrides 应先被清空"
    );

    let reconnected = backend
        .reconnect(session_id, &restore_info, PtySize::new(24, 80))
        .unwrap();

    // 中文注释：daemon 重连已有 tmux server 时也必须修复 bridge terminal-overrides；
    // 否则正式环境升级后，旧 server 上继续 attach 的 codex 仍会沿用错误能力集。
    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "terminal-overrides"]),
        "terminal-overrides[0] xterm-256color:smcup@:rmcup@"
    );

    drop(reconnected);
    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_writes_remain_on_exit_for_short_lived_commands() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("remain-on-exit");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a009";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf remain-on-exit-ready"]),
            PtySize::new(24, 80),
        )
        .unwrap();

    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info else {
        panic!("expected tmux restore info");
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if session.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let snapshot = session
        .snapshot()
        .expect("tmux snapshot should work after pane exit");
    assert!(
        String::from_utf8_lossy(&snapshot.retained_output).contains("remain-on-exit-ready"),
        "tmux snapshot should retain output after pane exit"
    );

    assert_eq!(
        tmux_output(&socket_path, &["show-options", "-g", "remain-on-exit"]),
        "remain-on-exit on"
    );
    assert!(
        tmux_output(
            &socket_path,
            &["display-message", "-pt", &session_name, "#{pane_dead}"]
        )
        .starts_with("1"),
        "pane should remain available in dead state until explicit cleanup"
    );

    session.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_reports_pane_current_working_directory_after_shell_cd() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("pane-cwd");
    let root = dir.join("root");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a006";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh")
                .args(["-c", "cd \"$TERMD_TEST_WORK\" && printf cwd-ready && cat"])
                .env("TERMD_TEST_WORK", work.to_string_lossy().to_string())
                .cwd(&root),
            PtySize::new(24, 80),
        )
        .unwrap();

    read_until_contains(&mut *session, b"cwd-ready");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if session.current_working_directory().as_deref() == Some(work.as_path()) {
            session.terminate().unwrap();
            cleanup_tmux(&socket_path);
            let _ = fs::remove_dir_all(dir);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!(
        "tmux pane cwd should follow shell cd; last cwd was {:?}, expected {:?}",
        session.current_working_directory(),
        work
    );
}

#[test]
#[cfg(unix)]
fn tmux_runtime_reconnects_persisted_session_with_saved_size() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("runtime-reconnect");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a003";
    let size = RuntimeTerminalSize {
        rows: 28,
        cols: 90,
        pixel_width: 1440,
        pixel_height: 900,
    };
    let mut runtime = SessionRuntime::new(backend.clone());
    runtime
        .create_session_with_id(
            session_id,
            CommandSpec::new("sh").args(["-lc", "printf runtime-reconnect-ready; cat"]),
            size,
        )
        .unwrap();
    runtime.attach(session_id, "dev-a").unwrap();

    let mut buffer = [0_u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let read = runtime.read_output(session_id, &mut buffer).unwrap();
        if read > 0 {
            collected.extend_from_slice(&buffer[..read]);
            if collected
                .windows(b"runtime-reconnect-ready".len())
                .any(|window| window == b"runtime-reconnect-ready")
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        collected
            .windows(b"runtime-reconnect-ready".len())
            .any(|window| window == b"runtime-reconnect-ready"),
        "initial runtime output missing: {}",
        String::from_utf8_lossy(&collected)
    );

    let mut persisted = runtime.persisted_sessions();
    assert_eq!(persisted.len(), 1);
    persisted[0].size = TerminalSize::new(31, 100);
    let PtyRestoreInfo::Tmux { session_name, .. } = persisted[0]
        .restore_info
        .clone()
        .expect("tmux runtime should persist restore info")
    else {
        panic!("expected tmux restore info");
    };
    drop(runtime);

    let mut restarted = SessionRuntime::new(backend);
    restarted.reconnect_session(&persisted[0]).unwrap();
    assert_eq!(
        restarted.size(session_id).unwrap(),
        RuntimeTerminalSize::cells(31, 100)
    );
    assert_eq!(
        tmux_output(
            &socket_path,
            &[
                "display-message",
                "-pt",
                &session_name,
                "#{window_width}x#{window_height}",
            ],
        ),
        "100x31"
    );
    let snapshot = restarted.snapshot(session_id).unwrap();
    assert!(
        snapshot
            .retained_output
            .windows(b"runtime-reconnect-ready".len())
            .any(|window| window == b"runtime-reconnect-ready"),
        "reconnected snapshot should be captured from tmux pane: {}",
        String::from_utf8_lossy(&snapshot.retained_output)
    );
    restarted.attach(session_id, "dev-b").unwrap();
    restarted
        .write_input(session_id, "dev-b", b"after-tmux-reconnect\n")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let read = restarted.read_output(session_id, &mut buffer).unwrap();
        if read > 0 {
            collected.extend_from_slice(&buffer[..read]);
            if collected
                .windows(b"after-tmux-reconnect".len())
                .any(|window| window == b"after-tmux-reconnect")
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        collected
            .windows(b"after-tmux-reconnect".len())
            .any(|window| window == b"after-tmux-reconnect"),
        "reconnected runtime output missing: {}",
        String::from_utf8_lossy(&collected)
    );

    restarted.close(session_id).unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_backend_reconnect_applies_saved_size_before_bridge_output_is_consumed() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("reconnect-size-before-bridge");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a010";
    let mut session = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf reconnect-size-ready; cat"]),
            PtySize::new(24, 80),
        )
        .unwrap();
    read_until_contains(&mut *session, b"reconnect-size-ready");
    let restore_info = session.restore_info().expect("tmux sessions persist");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info.clone() else {
        panic!("expected tmux restore info");
    };

    drop(session);

    let mut reconnected = backend
        .reconnect(session_id, &restore_info, PtySize::new(31, 100))
        .unwrap();

    assert_eq!(
        tmux_output(
            &socket_path,
            &[
                "display-message",
                "-pt",
                &session_name,
                "#{window_width}x#{window_height}",
            ],
        ),
        "100x31",
        "reconnect 建立 daemon bridge 前必须先把 tmux window 调整到保存尺寸"
    );

    let mut buffer = [0_u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let read = reconnected.read(&mut buffer).unwrap();
        if read > 0 {
            collected.extend_from_slice(&buffer[..read]);
            if collected
                .windows(b"reconnect-size-ready".len())
                .any(|window| window == b"reconnect-size-ready")
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        collected
            .windows(b"reconnect-size-ready".len())
            .any(|window| window == b"reconnect-size-ready"),
        "reconnected bridge output missing: {}",
        String::from_utf8_lossy(&collected)
    );

    reconnected.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_drop_detaches_only_own_bridge_client() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("drop-one-bridge");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a005";
    let mut first = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf bridge-ready; cat"]),
            PtySize::new(24, 80),
        )
        .unwrap();
    read_until_contains(&mut *first, b"bridge-ready");
    let restore_info = first.restore_info().expect("tmux restore info");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info.clone() else {
        panic!("expected tmux restore info");
    };
    let mut second = backend
        .reconnect(session_id, &restore_info, PtySize::new(24, 80))
        .unwrap();
    wait_for_tmux_client_count(&socket_path, &session_name, 2);

    drop(first);
    wait_for_tmux_client_count(&socket_path, &session_name, 1);
    second.write_all(b"second-bridge-still-attached\n").unwrap();
    read_until_contains(&mut *second, b"second-bridge-still-attached");

    second.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_watched_attachment_handles_are_independent_tmux_clients() {
    if !tmux_available() {
        eprintln!("tmux unavailable; skipping integration test");
        return;
    }

    let dir = temp_dir("watched-attachments");
    fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("tmux.sock");
    let backend = TmuxPtyBackend::with_socket_path(&socket_path);
    let session_id = "00000000-0000-0000-0000-00000000a007";
    let mut host = backend
        .spawn_named(
            session_id,
            &CommandSpec::new("sh").args(["-lc", "printf watched-ready; cat"]),
            PtySize::new(24, 80),
        )
        .unwrap();
    read_until_contains(&mut *host, b"watched-ready");
    let restore_info = host.restore_info().expect("tmux restore info");
    let PtyRestoreInfo::Tmux { session_name, .. } = restore_info.clone() else {
        panic!("expected tmux restore info");
    };

    let first = backend
        .attach_client(
            session_id,
            Some(&restore_info),
            PtySize::new(24, 80),
            "test-watch-1",
        )
        .unwrap();
    let second = backend
        .attach_client(
            session_id,
            Some(&restore_info),
            PtySize::new(24, 80),
            "test-watch-2",
        )
        .unwrap();
    wait_for_tmux_client_count(&socket_path, &session_name, 3);

    drop(first);
    wait_for_tmux_client_count(&socket_path, &session_name, 2);
    host.write_all(b"host-still-running-after-first-watched-drop\n")
        .unwrap();
    read_until_contains(&mut *host, b"host-still-running-after-first-watched-drop");

    drop(second);
    wait_for_tmux_client_count(&socket_path, &session_name, 1);
    host.write_all(b"host-still-running-after-all-watched-drop\n")
        .unwrap();
    read_until_contains(&mut *host, b"host-still-running-after-all-watched-drop");

    host.terminate().unwrap();
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(unix)]
fn tmux_restore_info_roundtrips_through_state_store() {
    let dir = temp_dir("state");
    fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("daemon-state.json");
    let socket_path = dir.join("tmux.sock");
    let session_id = SessionId::new();
    let restore_info = PtyRestoreInfo::Tmux {
        socket_path: socket_path.clone(),
        session_name: "termd-test-session".to_owned(),
    };

    let state = DaemonState {
        version: termd::state::STATE_SCHEMA_VERSION,
        daemon_identity: None,
        trusted_devices: Vec::new(),
        sessions: vec![SessionStateRecord {
            session_id,
            state: SessionState::Running,
            size: TerminalSize::new(24, 80),
            created_at_ms: UnixTimestampMillis(1),
            updated_at_ms: UnixTimestampMillis(2),
            restore_info: Some(restore_info.clone()),
        }],
    };

    StateStore::save(&state_path, &state).unwrap();
    let loaded = StateStore::load(&state_path).unwrap();

    assert_eq!(loaded.sessions[0].restore_info, Some(restore_info));
    cleanup_tmux(&socket_path);
    let _ = fs::remove_dir_all(dir);
}
