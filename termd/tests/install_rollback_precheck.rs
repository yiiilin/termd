use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;

fn installer_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("scripts/install-termd.sh")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "termd-installer-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_precheck(state_dir: &Path) -> std::process::Output {
    Command::new("bash")
        .args([
            "-c",
            r#"set -euo pipefail
source <(sed '/^main "$@"/,$d' "$1")
STATE_DIR="$2"
assert_session_ownership_quiescent
"#,
            "rollback-precheck",
        ])
        .arg(installer_path())
        .arg(state_dir)
        .output()
        .unwrap()
}

fn write_ledger(state_dir: &Path, phases: &[&str]) -> PathBuf {
    fs::create_dir_all(state_dir).unwrap();
    let path = state_dir.join("daemon-state.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session_ownership (
                session_id TEXT PRIMARY KEY NOT NULL,
                phase TEXT NOT NULL
            ) STRICT;",
        )
        .unwrap();
    for (index, phase) in phases.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO session_ownership (session_id, phase) VALUES (?1, ?2)",
                (format!("session-{index}"), phase),
            )
            .unwrap();
    }
    drop(connection);
    path
}

fn run_staged_commit(
    state_dir: &Path,
    install_prefix: &Path,
    candidate: &Path,
    service_state: &Path,
    service_calls: &Path,
) -> std::process::Output {
    Command::new("bash")
        .args([
            "-c",
            r#"set -euo pipefail
source <(sed '/^main "$@"/,$d' "$1")
STATE_DIR="$2"
INSTALL_PREFIX="$3"
SERVICE_NAME="termd"
ENV_FILE="${INSTALL_PREFIX}/etc/termd.env"
WRAPPER_FILE="${INSTALL_PREFIX}/etc/termd-run"
UNIT_FILE="${INSTALL_PREFIX}/etc/termd.service"
SERVICE_STATE="$5"
SERVICE_CALLS="$6"
systemctl() {
  printf '%s\n' "$*" >>"$SERVICE_CALLS"
  case "${1:-}" in
    is-active) [[ "$(cat "$SERVICE_STATE")" == "active" ]] ;;
    stop) printf 'inactive\n' >"$SERVICE_STATE" ;;
    start) printf 'active\n' >"$SERVICE_STATE" ;;
    *) : ;;
  esac
}
set +e
commit_staged_binary "$4"
status=$?
set -e
printf '%s\n' "$status"
"#,
            "staged-commit",
        ])
        .arg(installer_path())
        .arg(state_dir)
        .arg(install_prefix)
        .arg(candidate)
        .arg(service_state)
        .arg(service_calls)
        .output()
        .unwrap()
}

fn run_failed_install_transaction(
    state_dir: &Path,
    install_prefix: &Path,
    candidate: &Path,
    service_state: &Path,
    service_calls: &Path,
) -> std::process::Output {
    Command::new("bash")
        .args([
            "-c",
            r#"set -euo pipefail
source <(sed '/^main "$@"/,$d' "$1")
STATE_DIR="$2"
INSTALL_PREFIX="$3"
SERVICE_NAME="termd"
SERVICE_STATE="$5"
SERVICE_CALLS="$6"
INSTALL_STAGING_DIR="$(mktemp -d)"
systemctl() {
  printf '%s\n' "$*" >>"$SERVICE_CALLS"
  case "${1:-}" in
    is-active) [[ "$(cat "$SERVICE_STATE")" == "active" ]] ;;
    is-enabled) return 1 ;;
    stop) printf 'inactive\n' >"$SERVICE_STATE" ;;
    start|restart) printf 'active\n' >"$SERVICE_STATE" ;;
    *) : ;;
  esac
}
prepare_install_before_binary_commit() { :; }
complete_install_after_binary_commit() {
  printf 'new-env\n' >"$ENV_FILE"
  printf 'new-wrapper\n' >"$WRAPPER_FILE"
  printf 'new-unit\n' >"$UNIT_FILE"
  return 42
}
set +e
install_staged_candidate "$4"
status=$?
set -e
printf '%s\n' "$status"
rm -rf "$INSTALL_STAGING_DIR"
"#,
            "failed-install-transaction",
        ])
        .arg(installer_path())
        .arg(state_dir)
        .arg(install_prefix)
        .arg(candidate)
        .arg(service_state)
        .arg(service_calls)
        .output()
        .unwrap()
}

#[test]
fn rollback_precheck_is_read_only_and_blocks_non_quiescent_ownership() {
    for phases in [
        Vec::<&str>::new(),
        vec!["active", "quarantined"],
        vec!["preparing"],
        vec!["cleaning"],
    ] {
        let state_dir = std::env::temp_dir().join(format!(
            "termd-installer-rollback-precheck-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sqlite_path = write_ledger(&state_dir, &phases);
        let before = fs::read(&sqlite_path).unwrap();

        let output = run_precheck(&state_dir);

        let blocked = phases
            .iter()
            .any(|phase| matches!(*phase, "preparing" | "cleaning"));
        assert_eq!(output.status.success(), !blocked, "phases={phases:?}");
        if blocked {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("cannot replace termd while 1 session ownership operation(s)"),
                "unexpected stderr for {phases:?}: {stderr}"
            );
        }
        assert_eq!(fs::read(&sqlite_path).unwrap(), before);
        assert!(!sqlite_path.with_extension("sqlite-wal").exists());
        assert!(!sqlite_path.with_extension("sqlite-shm").exists());
        fs::remove_dir_all(state_dir).unwrap();
    }
}

#[test]
fn staged_commit_rechecks_after_stopping_service_and_restores_prior_active_state_on_block() {
    for (phase, initially_active) in [("preparing", true), ("cleaning", false)] {
        let root = TestDir::new("staged-block");
        let state_dir = root.path().join("state");
        let sqlite_path = write_ledger(&state_dir, &[]);
        assert!(run_precheck(&state_dir).status.success());
        Connection::open(&sqlite_path)
            .unwrap()
            .execute(
                "INSERT INTO session_ownership (session_id, phase) VALUES ('late', ?1)",
                [phase],
            )
            .unwrap();

        let install_prefix = root.path().join("prefix");
        let installed = install_prefix.join("bin/termd");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, b"old-binary").unwrap();
        let candidate = root.path().join("candidate");
        fs::write(&candidate, b"new-binary").unwrap();
        let service_state = root.path().join("service-state");
        fs::write(
            &service_state,
            if initially_active {
                "active"
            } else {
                "inactive"
            },
        )
        .unwrap();
        let service_calls = root.path().join("service-calls");

        let output = run_staged_commit(
            &state_dir,
            &install_prefix,
            &candidate,
            &service_state,
            &service_calls,
        );
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
        assert_eq!(fs::read(&installed).unwrap(), b"old-binary");
        assert_eq!(
            fs::read_to_string(&service_state).unwrap().trim(),
            if initially_active {
                "active"
            } else {
                "inactive"
            }
        );
        let calls = fs::read_to_string(&service_calls).unwrap();
        assert_eq!(calls.contains("stop termd"), initially_active);
        assert_eq!(calls.contains("start termd"), initially_active);
    }
}

#[test]
fn staged_commit_replaces_binary_only_after_quiescent_second_check() {
    let root = TestDir::new("staged-success");
    let state_dir = root.path().join("state");
    write_ledger(&state_dir, &["active"]);
    let install_prefix = root.path().join("prefix");
    let installed = install_prefix.join("bin/termd");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(&installed, b"old-binary").unwrap();
    let candidate = root.path().join("candidate");
    fs::write(&candidate, b"new-binary").unwrap();
    let service_state = root.path().join("service-state");
    fs::write(&service_state, b"active").unwrap();
    let service_calls = root.path().join("service-calls");

    let output = run_staged_commit(
        &state_dir,
        &install_prefix,
        &candidate,
        &service_state,
        &service_calls,
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
    assert_eq!(fs::read(&installed).unwrap(), b"new-binary");
    assert_eq!(
        fs::read_to_string(&service_state).unwrap().trim(),
        "inactive"
    );
    let calls = fs::read_to_string(&service_calls).unwrap();
    assert!(calls.contains("stop termd"));
    assert!(!calls.contains("start termd"));
}

#[test]
fn post_commit_failure_restores_old_artifacts_service_state_and_primary_status() {
    for initially_active in [true, false] {
        let root = TestDir::new("post-commit-failure");
        let state_dir = root.path().join("state");
        write_ledger(&state_dir, &["active"]);
        let install_prefix = root.path().join("prefix");
        let installed = install_prefix.join("bin/termd");
        let env_file = install_prefix.join("etc/termd.env");
        let wrapper_file = install_prefix.join("etc/termd-run");
        let unit_file = install_prefix.join("etc/termd.service");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        fs::write(&installed, b"old-binary").unwrap();
        fs::write(&env_file, b"old-env").unwrap();
        fs::write(&wrapper_file, b"old-wrapper").unwrap();
        fs::write(&unit_file, b"old-unit").unwrap();
        let candidate = root.path().join("candidate");
        fs::write(&candidate, b"new-binary").unwrap();
        let service_state = root.path().join("service-state");
        fs::write(
            &service_state,
            if initially_active {
                "active"
            } else {
                "inactive"
            },
        )
        .unwrap();
        let service_calls = root.path().join("service-calls");

        let output = run_failed_install_transaction(
            &state_dir,
            &install_prefix,
            &candidate,
            &service_state,
            &service_calls,
        );
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
        assert_eq!(fs::read(&installed).unwrap(), b"old-binary");
        assert_eq!(fs::read(&env_file).unwrap(), b"old-env");
        assert_eq!(fs::read(&wrapper_file).unwrap(), b"old-wrapper");
        assert_eq!(fs::read(&unit_file).unwrap(), b"old-unit");
        assert_eq!(
            fs::read_to_string(&service_state).unwrap().trim(),
            if initially_active {
                "active"
            } else {
                "inactive"
            }
        );
        let calls = fs::read_to_string(&service_calls).unwrap();
        assert!(calls.contains("stop termd"));
        assert_eq!(calls.contains("start termd"), initially_active);
    }
}
