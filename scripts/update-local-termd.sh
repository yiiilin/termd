#!/usr/bin/env bash

set -euo pipefail

# 从当前源码安全更新本机 systemd 管理的 termd 主进程。
# 这个脚本只替换 /usr/local/bin/termd 并重启 termd.service；不会主动终止 session supervisor。

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVICE_NAME="${TERMD_SERVICE_NAME:-termd.service}"
BIN_PATH="${TERMD_BIN_PATH:-/usr/local/bin/termd}"
STATE_DIR="${TERMD_STATE_DIR:-/var/lib/termd}"
STATE_DB="${TERMD_STATE_DB:-${STATE_DIR}/daemon-state.sqlite}"
HEALTH_URL="${TERMD_HEALTH_URL:-http://127.0.0.1:8765/healthz}"
WORKSPACE_TESTS=0
SKIP_TESTS=0
SKIP_BUILD=0
DRY_RUN=0

usage() {
  cat <<'EOF'
usage: scripts/update-local-termd.sh [OPTIONS]

Build the current checkout and safely update the local systemd termd service.

Options:
  --workspace-tests       Run cargo test --workspace --locked instead of only termd tests.
  --skip-tests            Skip cargo fmt/test verification; build still runs unless --skip-build is set.
  --skip-build            Reuse target/release/termd instead of building it.
  --service <NAME>        systemd service name; default: termd.service.
  --bin <PATH>            Installed termd binary path; default: /usr/local/bin/termd.
  --state-db <PATH>       SQLite state DB; default: /var/lib/termd/daemon-state.sqlite.
  --health-url <URL>      Health check URL; default: http://127.0.0.1:8765/healthz.
  --dry-run               Run checks and print the planned install/restart without changing service state.
  -h, --help              Print this help.

Environment overrides:
  TERMD_SERVICE_NAME, TERMD_BIN_PATH, TERMD_STATE_DIR, TERMD_STATE_DB, TERMD_HEALTH_URL
EOF
}

die() {
  printf '[update-local-termd] %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[update-local-termd] %s\n' "$*"
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

parse_args() {
  while (($#)); do
    case "$1" in
      --workspace-tests)
        WORKSPACE_TESTS=1
        shift
        ;;
      --skip-tests)
        SKIP_TESTS=1
        shift
        ;;
      --skip-build)
        SKIP_BUILD=1
        shift
        ;;
      --service)
        [[ $# -ge 2 && -n "$2" ]] || die "--service requires a value"
        SERVICE_NAME="$2"
        shift 2
        ;;
      --bin)
        [[ $# -ge 2 && -n "$2" ]] || die "--bin requires a value"
        BIN_PATH="$2"
        shift 2
        ;;
      --state-db)
        [[ $# -ge 2 && -n "$2" ]] || die "--state-db requires a value"
        STATE_DB="$2"
        shift 2
        ;;
      --health-url)
        [[ $# -ge 2 && -n "$2" ]] || die "--health-url requires a value"
        HEALTH_URL="$2"
        shift 2
        ;;
      --dry-run)
        DRY_RUN=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done
}

service_property() {
  local property="$1"
  systemctl show "$SERVICE_NAME" -p "$property" --value
}

assert_service_can_restart_without_killing_supervisors() {
  local kill_mode active_state main_pid

  active_state="$(service_property ActiveState)"
  [[ "$active_state" == "active" ]] || die "${SERVICE_NAME} is not active; refusing to update a non-running daemon"

  main_pid="$(service_property MainPID)"
  [[ "$main_pid" =~ ^[0-9]+$ && "$main_pid" -gt 0 ]] || die "cannot determine ${SERVICE_NAME} MainPID"

  kill_mode="$(service_property KillMode)"
  [[ "$kill_mode" == "process" ]] || die "${SERVICE_NAME} KillMode=${kill_mode}; expected process so restart will not kill session supervisors"
}

snapshot_supervisor_pids() {
  local output="$1"

  # supervisor 是独立进程，必须在重启主 daemon 前后保持不变。
  python3 - "$STATE_DB" >"$output" <<'PY'
import os
import sys
from pathlib import Path

state_db = Path(sys.argv[1])
supervisor_dir = (state_db.parent / "termd-supervisors").resolve()
proc = Path("/proc")
if not proc.exists():
    raise SystemExit("cannot inspect /proc")

pids: list[int] = []
for entry in proc.iterdir():
    if not entry.name.isdigit():
        continue
    try:
        raw = (entry / "cmdline").read_bytes()
    except (FileNotFoundError, PermissionError, OSError):
        continue
    args = [part.decode("utf-8", errors="surrogateescape") for part in raw.split(b"\0") if part]
    if "__session-supervisor" not in args:
        continue
    try:
        socket_path = Path(args[args.index("--socket-path") + 1])
    except (ValueError, IndexError):
        continue
    try:
        socket_parent = socket_path.parent.resolve()
    except OSError:
        socket_parent = socket_path.parent.absolute()
    if socket_parent == supervisor_dir:
        pids.append(int(entry.name))

for pid in sorted(pids):
    print(pid)
PY
}

snapshot_supervisor_session_ids() {
  local output="$1"

  # 仅记录本 state 目录下的 supervisor session id，用于和 SQLite running 行交叉校验。
  python3 - "$STATE_DB" >"$output" <<'PY'
import sys
from pathlib import Path

state_db = Path(sys.argv[1])
supervisor_dir = (state_db.parent / "termd-supervisors").resolve()
proc = Path("/proc")
if not proc.exists():
    raise SystemExit("cannot inspect /proc")

session_ids: set[str] = set()
for entry in proc.iterdir():
    if not entry.name.isdigit():
        continue
    try:
        raw = (entry / "cmdline").read_bytes()
    except (FileNotFoundError, PermissionError, OSError):
        continue
    args = [part.decode("utf-8", errors="surrogateescape") for part in raw.split(b"\0") if part]
    if "__session-supervisor" not in args:
        continue
    try:
        socket_path = Path(args[args.index("--socket-path") + 1])
        session_id = args[args.index("--session-id") + 1]
    except (ValueError, IndexError):
        continue
    try:
        socket_parent = socket_path.parent.resolve()
    except OSError:
        socket_parent = socket_path.parent.absolute()
    if socket_parent == supervisor_dir:
        session_ids.add(session_id)

for session_id in sorted(session_ids):
    print(session_id)
PY
}

write_state_counts() {
  local output="$1"

  python3 - "$STATE_DB" >"$output" <<'PY'
import sqlite3
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.exists():
    print("daemon_sessions|missing|0")
    print("runtime_sessions|missing|0")
    print("supervisor_version|missing|")
    raise SystemExit(0)

conn = sqlite3.connect(path)
try:
    tables = {
        row[0]
        for row in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")
    }
    for table in ("daemon_sessions", "runtime_sessions"):
        if table not in tables:
            print(f"{table}|missing|0")
            continue
        rows = conn.execute(
            f"SELECT state, COUNT(*) FROM {table} GROUP BY state ORDER BY state"
        ).fetchall()
        if not rows:
            print(f"{table}|empty|0")
        for state, count in rows:
            print(f"{table}|{state}|{count}")
    if "daemon_meta" in tables:
        row = conn.execute(
            "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
        ).fetchone()
        print("supervisor_version|value|" + (row[0] if row else ""))
    else:
        print("supervisor_version|missing|")
finally:
    conn.close()
PY
}

assert_live_supervisors_are_running_in_state() {
  local supervisor_ids_file="$1"
  local phase="$2"

  # 有 live supervisor 时，SQLite 里对应 daemon/runtime 行必须仍是 running。
  # 这能拦住“进程还在，但状态已经被误标 closed”的危险更新。
  python3 - "$STATE_DB" "$supervisor_ids_file" "$phase" <<'PY'
import sqlite3
import sys
from pathlib import Path

db_path = Path(sys.argv[1])
supervisor_ids_path = Path(sys.argv[2])
phase = sys.argv[3]
supervisor_ids = [
    line.strip()
    for line in supervisor_ids_path.read_text().splitlines()
    if line.strip()
]
if not supervisor_ids:
    raise SystemExit(0)
if not db_path.exists():
    raise SystemExit(f"{phase}: live supervisors exist but state DB is missing: {db_path}")

conn = sqlite3.connect(db_path)
try:
    tables = {
        row[0]
        for row in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")
    }
    missing_tables = [
        table
        for table in ("daemon_sessions", "runtime_sessions")
        if table not in tables
    ]
    if missing_tables:
        raise SystemExit(
            f"{phase}: live supervisors exist but state DB is missing tables: "
            + ", ".join(missing_tables)
        )

    bad: list[str] = []
    for session_id in supervisor_ids:
        daemon = conn.execute(
            "SELECT state FROM daemon_sessions WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        runtime = conn.execute(
            "SELECT state FROM runtime_sessions WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        daemon_state = daemon[0] if daemon else "<missing>"
        runtime_state = runtime[0] if runtime else "<missing>"
        if daemon_state != "running" or runtime_state != "running":
            bad.append(f"{session_id}: daemon={daemon_state}, runtime={runtime_state}")
    if bad:
        raise SystemExit(
            f"{phase}: live supervisor sessions are not running in SQLite:\n"
            + "\n".join(f"- {line}" for line in bad)
        )
finally:
    conn.close()
PY
}

state_count() {
  local file="$1"
  local table="$2"
  local state="$3"

  awk -F'|' -v table="$table" -v state="$state" '$1 == table && $2 == state { print $3 }' "$file" | tail -n1
}

assert_running_sessions_did_not_drop() {
  local before="$1"
  local after="$2"
  local table before_count after_count

  for table in daemon_sessions runtime_sessions; do
    before_count="$(state_count "$before" "$table" running)"
    after_count="$(state_count "$after" "$table" running)"
    before_count="${before_count:-0}"
    after_count="${after_count:-0}"
    if ((after_count < before_count)); then
      die "${table} running count dropped from ${before_count} to ${after_count}"
    fi
  done
}

verify_health() {
  local response

  response="$(curl -fsS "$HEALTH_URL")" || die "health check failed: ${HEALTH_URL}"
  python3 - "$response" <<'PY' || die "health response is not valid ok JSON"
import json
import sys

payload = json.loads(sys.argv[1])
if payload.get("status") != "ok":
    raise SystemExit(1)
PY
  log "health ok: ${HEALTH_URL}"
}

run_verification() {
  if [[ "$SKIP_TESTS" -eq 1 ]]; then
    log "skipping tests by request"
    return 0
  fi

  cargo fmt --check
  if [[ "$WORKSPACE_TESTS" -eq 1 ]]; then
    cargo test --workspace --locked
  else
    cargo test -p termd --locked
  fi
}

build_release_binary() {
  if [[ "$SKIP_BUILD" -eq 1 ]]; then
    [[ -x "${ROOT_DIR}/target/release/termd" ]] || die "target/release/termd does not exist or is not executable"
    log "reusing existing target/release/termd"
    return 0
  fi

  cargo build --release -p termd --bin termd --locked
}

install_and_restart() {
  local new_path

  new_path="${BIN_PATH}.new"
  install -m 0755 "${ROOT_DIR}/target/release/termd" "$new_path"
  mv "$new_path" "$BIN_PATH"
  systemctl restart "$SERVICE_NAME"
}

main() {
  parse_args "$@"
  cd "$ROOT_DIR"

  require_cmd cargo
  require_cmd curl
  require_cmd install
  require_cmd python3
  require_cmd systemctl

  assert_service_can_restart_without_killing_supervisors
  run_verification
  build_release_binary

  local tmp_dir before_pids after_pids before_session_ids after_session_ids before_counts after_counts before_count
  tmp_dir="$(mktemp -d)"
  trap "rm -rf '${tmp_dir}'" EXIT
  before_pids="${tmp_dir}/supervisors-before.txt"
  after_pids="${tmp_dir}/supervisors-after.txt"
  before_session_ids="${tmp_dir}/supervisor-session-ids-before.txt"
  after_session_ids="${tmp_dir}/supervisor-session-ids-after.txt"
  before_counts="${tmp_dir}/state-before.txt"
  after_counts="${tmp_dir}/state-after.txt"

  snapshot_supervisor_pids "$before_pids"
  snapshot_supervisor_session_ids "$before_session_ids"
  before_count="$(wc -l <"$before_pids" | tr -d ' ')"
  [[ "$before_count" =~ ^[0-9]+$ ]] || die "cannot count supervisor pids"
  write_state_counts "$before_counts"
  assert_live_supervisors_are_running_in_state "$before_session_ids" "pre-update"

  log "pre-update supervisor count: ${before_count}"
  log "pre-update state:"
  sed 's/^/[update-local-termd]   /' "$before_counts"

  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "dry run: would install target/release/termd to ${BIN_PATH} and restart ${SERVICE_NAME}"
    return 0
  fi

  install_and_restart

  sleep 1
  verify_health
  snapshot_supervisor_pids "$after_pids"
  snapshot_supervisor_session_ids "$after_session_ids"
  write_state_counts "$after_counts"

  if ! diff -u "$before_pids" "$after_pids" >/dev/null; then
    diff -u "$before_pids" "$after_pids" >&2 || true
    die "session supervisor PID set changed during local termd update"
  fi
  if ! diff -u "$before_session_ids" "$after_session_ids" >/dev/null; then
    diff -u "$before_session_ids" "$after_session_ids" >&2 || true
    die "session supervisor id set changed during local termd update"
  fi
  assert_running_sessions_did_not_drop "$before_counts" "$after_counts"
  assert_live_supervisors_are_running_in_state "$after_session_ids" "post-update"

  log "post-update state:"
  sed 's/^/[update-local-termd]   /' "$after_counts"
  log "updated ${BIN_PATH} and restarted ${SERVICE_NAME}; supervisor PIDs were unchanged"
}

main "$@"
