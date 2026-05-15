#!/usr/bin/env bash

set -euo pipefail

# 安装脚本的轻量回归测试。
# 这里不执行真实安装/卸载，只检查 CLI 帮助和 shell 语法，避免测试误删系统文件。

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

assert_help_contains() {
  local script="$1"
  local expected="$2"
  local output

  output="$(bash "${ROOT_DIR}/${script}" --help)"
  if [[ "$output" != *"$expected"* ]]; then
    printf 'expected %s --help to contain %q\n' "$script" "$expected" >&2
    exit 1
  fi
}

for script in \
  scripts/install-termd.sh \
  scripts/install-termctl.sh \
  scripts/install-termrelay.sh
do
  bash -n "${ROOT_DIR}/${script}"
  assert_help_contains "$script" "--uninstall"
done

assert_help_contains scripts/install-termd.sh "--web"
assert_help_contains scripts/install-termd.sh "--listen <HOST:PORT>"
assert_help_contains scripts/install-termd.sh "--proxy <URL>"
assert_help_contains scripts/install-termd.sh "--supervisor-version <VER>"
assert_help_contains scripts/install-termd.sh "--user <USER>"
assert_help_contains scripts/install-termd.sh "--purge"

assert_help_contains scripts/install-termrelay.sh "--web"
assert_help_contains scripts/install-termrelay.sh "--listen <HOST:PORT>"
assert_help_contains scripts/install-termrelay.sh "--auth-token <TOKEN>"
assert_help_contains scripts/install-termrelay.sh "--purge"

grep -q "KillMode=process" "${ROOT_DIR}/scripts/install-termd.sh"
grep -q "KillMode=process" "${ROOT_DIR}/scripts/install-termrelay.sh"
grep -q "termctl pair --payload" "${ROOT_DIR}/scripts/install-termd.sh"
! grep -q "termctl pair --token" "${ROOT_DIR}/scripts/install-termd.sh"
grep -q 'SUPERVISOR_VERSION="${TERMD_SUPERVISOR_VERSION:-}"' "${ROOT_DIR}/scripts/install-termd.sh"
! grep -q 'TERMD_SUPERVISOR_VERSION:-.*supervisor_version' "${ROOT_DIR}/.github/workflows/release.yml"
test -s "${ROOT_DIR}/SUPERVISOR_VERSION"

load_termd_installer_functions() {
  # 测试只加载函数和默认变量，跳过脚本末尾的 main 调用，避免触发真实安装。
  unset SUPERVISOR_VERSION TERMD_SUPERVISOR_VERSION TERMD_INSTALL_CONFIRM_FD
  # shellcheck source=/dev/null
  source <(sed '/^main "\$@"/,$d' "${ROOT_DIR}/scripts/install-termd.sh")
}

assert_file_contains() {
  local file="$1"
  local expected="$2"

  if ! grep -Fq "$expected" "$file"; then
    printf 'expected %s to contain %q\n' "$file" "$expected" >&2
    printf 'actual file:\n' >&2
    sed -n '1,160p' "$file" >&2
    exit 1
  fi
}

install_fake_termd_system_commands() {
  # 用假的系统账号数据库覆盖 id/getent，测试即可稳定覆盖 alice/bob/termd 三种路径。
  id() {
    case "${1:-}" in
      -u)
        case "${2:-}" in
          alice) printf '1001\n' ;;
          bob) printf '1002\n' ;;
          *) return 1 ;;
        esac
        ;;
      -gn)
        case "${2:-}" in
          alice) printf 'alice-primary\n' ;;
          bob) printf 'bob-primary\n' ;;
          *) return 1 ;;
        esac
        ;;
      *)
        command id "$@"
        ;;
    esac
  }

  getent() {
    case "${1:-}:${2:-}" in
      passwd:alice) printf 'alice:x:1001:1001:Alice:/home/alice:/bin/zsh\n' ;;
      passwd:bob) printf 'bob:x:1002:1002:Bob:/srv/bob:/usr/sbin/nologin\n' ;;
      group:deploy) printf 'deploy:x:2001:\n' ;;
      group:alice-primary) printf 'alice-primary:x:1001:\n' ;;
      group:bob-primary) printf 'bob-primary:x:1002:\n' ;;
      *) return 2 ;;
    esac
  }

  require_root() { :; }
  require_cmd() { :; }
  resolve_version() { VERSION="v-test"; }
  install_from_release() { return 0; }
  install_from_source() { return 1; }
  ensure_system_user() { :; }
  chown() { :; }
  chmod() { :; }
  systemctl() { :; }
  print_initial_pairing_token() { :; }
}

seed_termd_runtime_sqlite() {
  local sqlite_file="$1"
  local supervisor_version="$2"

  python3 - "$sqlite_file" "$supervisor_version" <<'PY'
import sqlite3
import sys

sqlite_path = sys.argv[1]
supervisor_version = sys.argv[2]

conn = sqlite3.connect(sqlite_path)
conn.executescript("""
CREATE TABLE daemon_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
CREATE TABLE trusted_devices (device_id TEXT PRIMARY KEY, public_key TEXT NOT NULL, trusted_at_ms INTEGER NOT NULL);
CREATE TABLE daemon_clients (device_id TEXT PRIMARY KEY);
CREATE TABLE daemon_client_attached_sessions (device_id TEXT NOT NULL, connection_id TEXT NOT NULL, session_id TEXT NOT NULL);
CREATE TABLE daemon_sessions (
  session_id TEXT PRIMARY KEY,
  name TEXT,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE runtime_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  restore_kind TEXT,
  restore_value TEXT
);
""")
conn.execute("INSERT INTO daemon_meta VALUES ('server_id', 'server', 1)")
conn.execute("INSERT INTO daemon_meta VALUES ('supervisor_version', ?, 1)", (supervisor_version,))
conn.execute("INSERT INTO trusted_devices VALUES ('device', 'public', 1)")
conn.execute("INSERT INTO daemon_clients VALUES ('device')")
conn.execute("INSERT INTO daemon_client_attached_sessions VALUES ('device', 'connection', 'session')")
conn.execute("INSERT INTO daemon_sessions VALUES ('session', 'work shell', 'running', 1)")
conn.execute("INSERT INTO runtime_sessions VALUES ('session', 'running', 1, 'unix_socket', '/tmp/session.sock')")
conn.commit()
conn.close()
PY
}

seed_termd_runtime_sqlite_without_supervisor_version() {
  local sqlite_file="$1"

  python3 - "$sqlite_file" <<'PY'
import sqlite3
import sys

sqlite_path = sys.argv[1]

conn = sqlite3.connect(sqlite_path)
conn.executescript("""
CREATE TABLE daemon_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
CREATE TABLE trusted_devices (device_id TEXT PRIMARY KEY, public_key TEXT NOT NULL, trusted_at_ms INTEGER NOT NULL);
CREATE TABLE daemon_clients (device_id TEXT PRIMARY KEY);
CREATE TABLE daemon_client_attached_sessions (device_id TEXT NOT NULL, connection_id TEXT NOT NULL, session_id TEXT NOT NULL);
CREATE TABLE daemon_sessions (
  session_id TEXT PRIMARY KEY,
  name TEXT,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE runtime_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  restore_kind TEXT,
  restore_value TEXT
);
INSERT INTO daemon_meta VALUES ('server_id', 'server', 1);
INSERT INTO trusted_devices VALUES ('device', 'public', 1);
INSERT INTO daemon_clients VALUES ('device');
INSERT INTO daemon_client_attached_sessions VALUES ('device', 'connection', 'session');
INSERT INTO daemon_sessions VALUES ('session', 'work shell', 'running', 1);
INSERT INTO runtime_sessions VALUES ('session', 'running', 1, 'unix_socket', '/tmp/session.sock');
""")
conn.close()
PY
}

create_stale_supervisor_socket() {
  local socket_file="$1"

  python3 - "$socket_file" <<'PY'
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.close()
PY
}

run_fake_termd_install() {
  local unit_file="$1"
  shift

  REPO="example/termd"
  VERSION=""
  UNIT_FILE="$unit_file"
  ENV_FILE="${unit_file%.service}.env"
  ENV_DIR="$(dirname "$ENV_FILE")"
  WRAPPER_FILE="${unit_file%.service}-run"
  main "$@" >/dev/null
}

test_termd_default_install_uses_managed_user() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"

  run_fake_termd_install "$unit_file"

  assert_file_contains "$unit_file" "User=termd"
  assert_file_contains "$unit_file" "Group=termd"
  assert_file_contains "$unit_file" "WorkingDirectory=/var/lib/termd"
  assert_file_contains "$unit_file" "EnvironmentFile=-${tmp_dir}/termd.env"
  assert_file_contains "$unit_file" "StateDirectory=termd"

  assert_file_contains "${tmp_dir}/termd.env" "HOME=/var/lib/termd"
  assert_file_contains "${tmp_dir}/termd.env" "SHELL=/bin/sh"
)

test_termd_upgrade_inherits_existing_user_without_user_arg() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  cat >"$unit_file" <<'EOF'
[Unit]
Description=existing termd

[Service]
User=alice
Group=deploy
WorkingDirectory=/old/state
EOF

  run_fake_termd_install "$unit_file"

  assert_file_contains "$unit_file" "User=alice"
  assert_file_contains "$unit_file" "Group=deploy"
  assert_file_contains "$unit_file" "WorkingDirectory=/var/lib/termd"
  assert_file_contains "$unit_file" "EnvironmentFile=-${tmp_dir}/termd.env"
  assert_file_contains "${tmp_dir}/termd.env" "HOME=/home/alice"
  assert_file_contains "${tmp_dir}/termd.env" "SHELL=/bin/zsh"
)

test_termd_upgrade_uses_fixed_state_dir_when_existing_unit_has_no_working_directory() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  cat >"$unit_file" <<'EOF'
[Unit]
Description=existing termd

[Service]
User=alice
Group=deploy
EOF

  run_fake_termd_install "$unit_file"

  assert_file_contains "$unit_file" "User=alice"
  assert_file_contains "$unit_file" "Group=deploy"
  assert_file_contains "$unit_file" "WorkingDirectory=/var/lib/termd"
  assert_file_contains "$unit_file" "EnvironmentFile=-${tmp_dir}/termd.env"
  assert_file_contains "${tmp_dir}/termd.env" "HOME=/home/alice"
  assert_file_contains "${tmp_dir}/termd.env" "SHELL=/bin/zsh"
)

test_termd_explicit_user_overrides_existing_service_user() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  cat >"$unit_file" <<'EOF'
[Service]
User=alice
Group=deploy
EOF

  run_fake_termd_install "$unit_file" --user bob

  assert_file_contains "$unit_file" "User=bob"
  assert_file_contains "$unit_file" "Group=bob-primary"
  assert_file_contains "$unit_file" "WorkingDirectory=/var/lib/termd"
  assert_file_contains "$unit_file" "EnvironmentFile=-${tmp_dir}/termd.env"
  assert_file_contains "${tmp_dir}/termd.env" "HOME=/srv/bob"
  assert_file_contains "${tmp_dir}/termd.env" "SHELL=/bin/sh"
)

test_termd_proxy_arg_writes_common_proxy_env_vars() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"

  run_fake_termd_install "$unit_file" --proxy http://127.0.0.1:3128

  assert_file_contains "${tmp_dir}/termd.env" "HTTP_PROXY=http://127.0.0.1:3128"
  assert_file_contains "${tmp_dir}/termd.env" "HTTPS_PROXY=http://127.0.0.1:3128"
  assert_file_contains "${unit_file%.service}-run" "set -a"
)

test_termd_state_dir_change_clears_only_session_state() (
  load_termd_installer_functions

  local tmp_dir sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  PREVIOUS_STATE_DIR="/old/state"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"

  python3 - "$sqlite_file" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
conn.executescript("""
CREATE TABLE daemon_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
CREATE TABLE trusted_devices (device_id TEXT PRIMARY KEY, public_key TEXT NOT NULL, trusted_at_ms INTEGER NOT NULL);
CREATE TABLE daemon_clients (device_id TEXT PRIMARY KEY);
CREATE TABLE daemon_client_attached_sessions (device_id TEXT NOT NULL, connection_id TEXT NOT NULL, session_id TEXT NOT NULL);
CREATE TABLE daemon_sessions (
  session_id TEXT PRIMARY KEY,
  name TEXT,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE runtime_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  restore_kind TEXT,
  restore_value TEXT
);
INSERT INTO daemon_meta VALUES ('server_id', 'server', 1);
INSERT INTO trusted_devices VALUES ('device', 'public', 1);
INSERT INTO daemon_clients VALUES ('device');
INSERT INTO daemon_client_attached_sessions VALUES ('device', 'connection', 'session');
INSERT INTO daemon_sessions VALUES ('session', 'work shell', 'running', 1);
INSERT INTO runtime_sessions VALUES ('session', 'running', 1, 'unix_socket', '/tmp/session.sock');
""")
conn.close()
PY
  python3 - "$socket_file" <<'PY'
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.close()
PY

  clear_session_state_after_state_dir_change >/dev/null

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    attached = conn.execute("SELECT COUNT(*) FROM daemon_client_attached_sessions").fetchone()[0]
    assert attached == 0, attached
    daemon_session = conn.execute(
        "SELECT name, state FROM daemon_sessions WHERE session_id = 'session'"
    ).fetchone()
    assert daemon_session == ("work shell", "closed"), daemon_session
    runtime_session = conn.execute(
        "SELECT state, restore_kind, restore_value FROM runtime_sessions WHERE session_id = 'session'"
    ).fetchone()
    assert runtime_session == ("closed", None, None), runtime_session
    assert conn.execute("SELECT COUNT(*) FROM daemon_meta").fetchone()[0] == 1
    assert conn.execute("SELECT COUNT(*) FROM trusted_devices").fetchone()[0] == 1
    assert conn.execute("SELECT COUNT(*) FROM daemon_clients").fetchone()[0] == 1
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_supervisor_version_match_keeps_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-test"
  create_stale_supervisor_socket "$socket_file"

  SUPERVISOR_VERSION="v-test"
  unit_file="${tmp_dir}/termd.service"
  run_fake_termd_install "$unit_file" >/dev/null
  unset SUPERVISOR_VERSION

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    for table in ("daemon_client_attached_sessions", "daemon_sessions", "runtime_sessions"):
        count = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
        assert count == 1, (table, count)
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-test", version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_baked_supervisor_default_keeps_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  # release 产物曾把默认 supervisor 版本烘进脚本；这不是用户显式请求升级，
  # 普通二进制更新必须继续沿用现有 baseline，并保留 runtime session。
  SUPERVISOR_VERSION="v-new"
  export TERMD_INSTALL_CONFIRM_FD=0
  unit_file="${tmp_dir}/termd.service"
  printf 'y\n' | run_fake_termd_install "$unit_file" >/dev/null
  unset SUPERVISOR_VERSION TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    for table in ("daemon_client_attached_sessions", "daemon_sessions", "runtime_sessions"):
        count = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
        assert count == 1, (table, count)
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-old", version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_missing_supervisor_meta_keeps_runtime_state_on_default_update() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite_without_supervisor_version "$sqlite_file"
  create_stale_supervisor_socket "$socket_file"

  # 旧版本可能还没有 supervisor_version 元数据；默认更新只能补 baseline，
  # 不能把已有 session 当成需要清理的旧 runtime。
  export TERMD_INSTALL_CONFIRM_FD=0
  unit_file="${tmp_dir}/termd.service"
  printf 'y\n' | run_fake_termd_install "$unit_file" >/dev/null
  unset TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    for table in ("daemon_client_attached_sessions", "daemon_sessions", "runtime_sessions"):
        count = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
        assert count == 1, (table, count)
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-test", version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_supervisor_version_mismatch_prompts_and_clears_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  export TERMD_INSTALL_CONFIRM_FD=0
  unit_file="${tmp_dir}/termd.service"
  printf 'y\n' | run_fake_termd_install "$unit_file" --supervisor-version v-new >/dev/null
  unset TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    attached = conn.execute("SELECT COUNT(*) FROM daemon_client_attached_sessions").fetchone()[0]
    assert attached == 0, attached
    daemon_session = conn.execute(
        "SELECT name, state FROM daemon_sessions WHERE session_id = 'session'"
    ).fetchone()
    assert daemon_session == ("work shell", "closed"), daemon_session
    runtime_session = conn.execute(
        "SELECT state, restore_kind, restore_value FROM runtime_sessions WHERE session_id = 'session'"
    ).fetchone()
    assert runtime_session == ("closed", None, None), runtime_session
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-new", version
    assert conn.execute("SELECT COUNT(*) FROM daemon_meta").fetchone()[0] == 2
    assert conn.execute("SELECT COUNT(*) FROM trusted_devices").fetchone()[0] == 1
    assert conn.execute("SELECT COUNT(*) FROM daemon_clients").fetchone()[0] == 1
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_supervisor_version_mismatch_decline_preserves_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file status
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  TERMD_STATE_DIR="${tmp_dir}/termd"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  export TERMD_INSTALL_CONFIRM_FD=0
  unit_file="${tmp_dir}/termd.service"
  set +e
  printf 'n\n' | run_fake_termd_install "$unit_file" --supervisor-version v-new >/dev/null 2>/dev/null
  status=$?
  set -e
  unset TERMD_INSTALL_CONFIRM_FD

  [[ "$status" -ne 0 ]]

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    for table in ("daemon_client_attached_sessions", "daemon_sessions", "runtime_sessions"):
        count = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
        assert count == 1, (table, count)
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-old", version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_default_install_uses_managed_user
test_termd_upgrade_inherits_existing_user_without_user_arg
test_termd_upgrade_uses_fixed_state_dir_when_existing_unit_has_no_working_directory
test_termd_explicit_user_overrides_existing_service_user
test_termd_proxy_arg_writes_common_proxy_env_vars
test_termd_state_dir_change_clears_only_session_state
test_termd_supervisor_version_match_keeps_runtime_state
test_termd_baked_supervisor_default_keeps_runtime_state
test_termd_missing_supervisor_meta_keeps_runtime_state_on_default_update
test_termd_supervisor_version_mismatch_prompts_and_clears_runtime_state
test_termd_supervisor_version_mismatch_decline_preserves_runtime_state

printf 'installer tests passed\n'
