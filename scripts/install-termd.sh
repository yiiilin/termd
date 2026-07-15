#!/usr/bin/env bash

set -euo pipefail

# 这个脚本安装 termd 二进制并注册 systemd 服务。
# 服务默认只监听 loopback，relay 和 TLS 通过 /etc/termd/termd.env 进行可选配置。
# supervisor 兼容版本默认跟随源码树里的 `SUPERVISOR_VERSION` 文件；release 资产会
# 通过 REQUIRED_SUPERVISOR_VERSION 注入。版本是 opaque compatibility id，不按日期解析。

COMPONENT="termd"
BIN_NAME="termd"
SERVICE_NAME="termd"
INSTALL_PREFIX="${TERMD_INSTALL_PREFIX:-/usr/local}"
if [[ -z "${ROOT_DIR:-}" ]]; then
  if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
    ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  else
    # Release installers are commonly piped to `bash -s`, where BASH_SOURCE has no entry.
    ROOT_DIR="$PWD"
  fi
fi
REPO="${TERMD_GITHUB_REPO:-${GITHUB_REPOSITORY:-}}"
VERSION="${TERMD_VERSION:-}"
SUPERVISOR_VERSION="${TERMD_SUPERVISOR_VERSION:-}"
REQUIRED_SUPERVISOR_VERSION="${TERMD_REQUIRED_SUPERVISOR_VERSION:-}"
ENV_DIR="/etc/termd"
ENV_FILE="${ENV_DIR}/termd.env"
WRAPPER_DIR="/usr/local/lib/termd"
WRAPPER_FILE="${WRAPPER_DIR}/termd-run"
UNIT_FILE="/etc/systemd/system/termd.service"
SERVICE_USER="termd"
SERVICE_GROUP="termd"
SERVICE_GROUP_FROM_UNIT=0
SERVICE_HOME=""
SERVICE_SHELL=""
STATE_DIR="/var/lib/termd"
PREVIOUS_STATE_DIR=""
INSTALL_SET_LISTEN=0
INSTALL_SET_WEB=0
INSTALL_SET_RELAY_URLS=0
INSTALL_SET_RELAY_DAEMON_TOKEN_FILE=0
INSTALL_SET_RELAY_SETUP_TOKEN_FILE=0
INSTALL_SET_RELAY_SETUP_TOKEN=0
INSTALL_SET_TLS_CERT=0
INSTALL_SET_TLS_KEY=0
INSTALL_SET_SUPERVISOR_VERSION=0
INSTALL_SET_USER=0
INSTALL_SET_PROXY=0
ACTION="install"
PURGE_STATE=0
LOG_EMITTED=0
SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR=0
SUPERVISOR_VERSION_EXPLICIT=0
SUPERVISOR_VERSION_REQUIRED=0
INSTALL_STAGING_DIR=""
INSTALL_SERVICE_WAS_ACTIVE=0
INSTALL_SERVICE_WAS_ENABLED=0
INSTALL_ROLLBACK_DIR=""
INSTALL_BINARY_COMMITTED=0
SUPERVISOR_VERSION_PERSIST_DEFERRED=0
SELF_INSTALL_MODE_SET=0
SELF_INSTALL_BINARY_SET=0
SELF_INSTALL_ENABLED=0
SELF_INSTALL_MODE=""
SELF_INSTALL_BINARY=""
INSTALL_ALLOW_SESSION_LOSS=0
INSTALL_ALLOW_OPEN_RELAY=0
INTERNAL_INSTALL_ARGUMENT_INVALID=0
INTERNAL_INSTALL_ARGUMENTS_PRESENT=0
INTERNAL_ARG_RELAY_URL=""
INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE=""
INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE=""
INTERNAL_ARG_RELAY_SETUP_TOKEN=""
INTERNAL_ARG_PROXY=""
INTERNAL_ARG_TLS_KEY=""

if [[ -v TERMD_INSTALL_SELF_MODE ]]; then
  SELF_INSTALL_MODE_SET=1
  SELF_INSTALL_MODE="$TERMD_INSTALL_SELF_MODE"
fi
if [[ -v TERMD_INSTALL_SELF_BINARY ]]; then
  SELF_INSTALL_BINARY_SET=1
  SELF_INSTALL_BINARY="$TERMD_INSTALL_SELF_BINARY"
fi
if [[ -v TERMD_INSTALL_ARG_RELAY_URL ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_RELAY_URL="$TERMD_INSTALL_ARG_RELAY_URL"
  [[ -n "$INTERNAL_ARG_RELAY_URL" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
if [[ -v TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE="$TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE"
  [[ -n "$INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
if [[ -v TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE="$TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE"
  [[ -n "$INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
if [[ -v TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_RELAY_SETUP_TOKEN="$TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN"
  [[ -n "$INTERNAL_ARG_RELAY_SETUP_TOKEN" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
if [[ -v TERMD_INSTALL_ARG_PROXY ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_PROXY="$TERMD_INSTALL_ARG_PROXY"
  [[ -n "$INTERNAL_ARG_PROXY" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
if [[ -v TERMD_INSTALL_ARG_TLS_KEY ]]; then
  INTERNAL_INSTALL_ARGUMENTS_PRESENT=1
  INTERNAL_ARG_TLS_KEY="$TERMD_INSTALL_ARG_TLS_KEY"
  [[ -n "$INTERNAL_ARG_TLS_KEY" ]] || INTERNAL_INSTALL_ARGUMENT_INVALID=1
fi
unset TERMD_INSTALL_SELF_MODE TERMD_INSTALL_SELF_BINARY TERMD_INSTALL_ASSUME_YES
unset TERMD_INSTALL_ARG_RELAY_URL TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE
unset TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE TERMD_INSTALL_ARG_PROXY TERMD_INSTALL_ARG_TLS_KEY
unset TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN

# 只有用户显式传入 TERMD_SUPERVISOR_VERSION 或 --supervisor-version 时，
# supervisor 版本才表示兼容性切换请求；release 脚本中的默认值不能触发清 session。
if [[ -n "${TERMD_SUPERVISOR_VERSION:-}" ]]; then
  SUPERVISOR_VERSION_EXPLICIT=1
  INSTALL_SET_SUPERVISOR_VERSION=1
fi
if [[ -n "$REQUIRED_SUPERVISOR_VERSION" ]]; then
  # release 产物会把真正不兼容的 supervisor 版本写到这里；这类升级必须阻止静默复用旧 supervisor。
  SUPERVISOR_VERSION_REQUIRED=1
fi

log() {
  if [[ "$LOG_EMITTED" -eq 1 ]]; then
    printf '\n'
  fi
  LOG_EMITTED=1
  printf '[%s-install] %s\n' "$COMPONENT" "$*"
}

die() {
  printf '[%s-install] %s\n' "$COMPONENT" "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

normalize_proxy_pair() {
  local lower_name="$1"
  local upper_name="$2"
  local value

  if [[ -v "$lower_name" ]]; then
    value="${!lower_name}"
  elif [[ -v "$upper_name" ]]; then
    value="${!upper_name}"
  else
    return 0
  fi

  printf -v "$lower_name" '%s' "$value"
  printf -v "$upper_name" '%s' "$value"
  export "$lower_name" "$upper_name"
}

normalize_proxy_environment() {
  normalize_proxy_pair http_proxy HTTP_PROXY
  normalize_proxy_pair https_proxy HTTPS_PROXY
  normalize_proxy_pair all_proxy ALL_PROXY
  normalize_proxy_pair no_proxy NO_PROXY
}

validate_internal_install_request() {
  local reported_version

  [[ "$INTERNAL_INSTALL_ARGUMENT_INVALID" -eq 0 ]] || die "embedded installer arguments are invalid"
  if [[ "$SELF_INSTALL_MODE_SET" -ne "$SELF_INSTALL_BINARY_SET" ]]; then
    die "embedded self-install identity is invalid"
  fi
  if [[ "$SELF_INSTALL_MODE_SET" -eq 0 ]]; then
    [[ "$INTERNAL_INSTALL_ARGUMENTS_PRESENT" -eq 0 ]] || die "embedded installer arguments are invalid"
    return 0
  fi

  SELF_INSTALL_ENABLED=1
  [[ "$SELF_INSTALL_MODE" == "embedded-v1" ]] || die "embedded self-install identity is invalid"
  [[ -n "$VERSION" ]] || die "embedded self-install identity is invalid"
  if [[ ! "$SELF_INSTALL_BINARY" =~ ^/proc/([0-9]+)/fd/([0-9]+)$ ]] || \
    [[ "${BASH_REMATCH[1]}" != "$PPID" ]]; then
    die "embedded self-install path is invalid"
  fi
  [[ -f "$SELF_INSTALL_BINARY" && -r "$SELF_INSTALL_BINARY" && -x "$SELF_INSTALL_BINARY" ]] || \
    die "embedded self-install path is invalid"
  if ! reported_version="$("$SELF_INSTALL_BINARY" --version 2>/dev/null)"; then
    die "embedded self-install identity is invalid"
  fi
  [[ "$reported_version" == "$BIN_NAME $VERSION" ]] || die "embedded self-install identity is invalid"
}

apply_internal_install_arguments() {
  if [[ -n "$INTERNAL_ARG_RELAY_URL" ]]; then
    TERMD_RELAY_URLS="$INTERNAL_ARG_RELAY_URL"
    INSTALL_SET_RELAY_URLS=1
  fi
  if [[ -n "$INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE" ]]; then
    TERMD_RELAY_DAEMON_TOKEN_FILE="$INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE"
    INSTALL_SET_RELAY_DAEMON_TOKEN_FILE=1
  fi
  if [[ -n "$INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE" ]]; then
    TERMD_RELAY_SETUP_TOKEN_FILE="$INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE"
    INSTALL_SET_RELAY_SETUP_TOKEN_FILE=1
  fi
  if [[ -n "$INTERNAL_ARG_RELAY_SETUP_TOKEN" ]]; then
    TERMD_RELAY_SETUP_TOKEN="$INTERNAL_ARG_RELAY_SETUP_TOKEN"
    export -n TERMD_RELAY_SETUP_TOKEN
    INSTALL_SET_RELAY_SETUP_TOKEN=1
  fi
  if [[ -n "$INTERNAL_ARG_PROXY" ]]; then
    http_proxy="$INTERNAL_ARG_PROXY"
    https_proxy="$INTERNAL_ARG_PROXY"
    HTTP_PROXY="$INTERNAL_ARG_PROXY"
    HTTPS_PROXY="$INTERNAL_ARG_PROXY"
    INSTALL_SET_PROXY=1
  fi
  if [[ -n "$INTERNAL_ARG_TLS_KEY" ]]; then
    TERMD_TLS_KEY="$INTERNAL_ARG_TLS_KEY"
    INSTALL_SET_TLS_KEY=1
  fi

  INTERNAL_ARG_RELAY_URL=""
  INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE=""
  INTERNAL_ARG_RELAY_SETUP_TOKEN_FILE=""
  INTERNAL_ARG_RELAY_SETUP_TOKEN=""
  INTERNAL_ARG_PROXY=""
  INTERNAL_ARG_TLS_KEY=""

  if [[ "$INSTALL_SET_RELAY_SETUP_TOKEN" -eq 1 && "$INSTALL_SET_RELAY_SETUP_TOKEN_FILE" -eq 1 ]]; then
    die "--relay-token conflicts with --relay-setup-token-file; provide only one relay setup token source"
  fi
}

is_enabled() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

web_ui_requested() {
  local requested

  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    is_enabled "${TERMD_WEB_ENABLED:-0}"
    return
  fi

  if [[ -r "$ENV_FILE" ]]; then
    if ! requested="$(
      # shellcheck source=/dev/null
      source "$ENV_FILE" || exit 1
      printf '%s' "${TERMD_WEB_ENABLED:-0}"
    )"; then
      die "failed to read existing env file at ${ENV_FILE}; installed binary was not changed"
    fi
    is_enabled "$requested"
    return
  fi

  is_enabled "${TERMD_WEB_ENABLED:-0}"
}

frontend_build_required() {
  local frontend_dir="$1"
  local index_file="${frontend_dir}/dist/index.html"
  local newer_source

  [[ -f "$index_file" ]] || return 0
  newer_source="$(find "$frontend_dir" \
    \( -path "${frontend_dir}/dist" -o -path "${frontend_dir}/node_modules" \) -prune -o \
    -type f -newer "$index_file" -print -quit)"
  [[ -n "$newer_source" ]]
}

build_frontend_for_source() {
  local repo_dir="$1"
  local frontend_dir="${repo_dir}/termui/frontend"

  if ! frontend_build_required "$frontend_dir"; then
    return 0
  fi

  log "building Web UI from ${REPO}@${VERSION}"
  (
    cd "$frontend_dir" &&
      npm ci &&
      npm run build
  ) || return 1

  [[ -f "${frontend_dir}/dist/index.html" ]] || return 1
}

require_root() {
  if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    die "please run this installer with sudo/root so it can write system files"
  fi
}

print_usage() {
  cat <<'EOF'
usage: install-termd.sh [OPTIONS]

Install termd and register termd.service.

Options:
  --web                         Enable embedded Web UI in systemd.
  --no-web                      Disable embedded Web UI in systemd.
  --listen <HOST:PORT>          Set TERMD_LISTEN, for example 0.0.0.0:8765.
  --public                      Alias for --listen 0.0.0.0:8765.
  --relay <WS_URL>              Set the relay URL.
  --relay-daemon-token-file <PATH> Read trusted relay daemon admission token from a file.
  --relay-setup-token-file <PATH> Read relay setup token once from a file.
  --allow-open-relay             Explicitly connect to a relay running with --allow-open-relay.
  --proxy <URL>                 Set relay outbound proxy; http://host:port or socks5://host:port.
                                Also supports HTTP_PROXY, HTTPS_PROXY, ALL_PROXY and NO_PROXY in /etc/termd/termd.env.
  --tls-cert <PATH>             Set TLS certificate path.
  --tls-key <PATH>              Set TLS private key path.
  --supervisor-version <VER>    Set the target supervisor compatibility version.
  --allow-session-loss          Non-interactively confirm session loss only when supervisor compatibility changes.
  --user <USER>                 Run termd.service as this Linux user; default: existing service user, then termd.
  --uninstall                   Stop service and remove termd program files.
  --purge                       Implies --uninstall; also remove /var/lib/termd and system user.
  -h, --help                    Print this help.

Installer network access honors http_proxy, https_proxy, all_proxy and no_proxy,
plus their uppercase variants. Lowercase values take precedence when both are set.

Examples:
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --user alice
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --supervisor-version 0.1.0
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall
EOF
}

append_space_separated() {
  local current="${1:-}"
  local next="$2"

  if [[ -z "$current" ]]; then
    printf '%s' "$next"
  else
    printf '%s %s' "$current" "$next"
  fi
}

parse_args() {
  while (($#)); do
    case "$1" in
      -h|--help)
        print_usage
        exit 0
        ;;
      --web)
        TERMD_WEB_ENABLED=1
        INSTALL_SET_WEB=1
        shift
        ;;
      --no-web)
        TERMD_WEB_ENABLED=0
        INSTALL_SET_WEB=1
        shift
        ;;
      --listen)
        [[ $# -ge 2 && -n "$2" ]] || die "--listen requires a value"
        TERMD_LISTEN="$2"
        INSTALL_SET_LISTEN=1
        shift 2
        ;;
      --public)
        TERMD_LISTEN="0.0.0.0:8765"
        INSTALL_SET_LISTEN=1
        shift
        ;;
      --relay|--relay-url)
        [[ $# -ge 2 && -n "$2" ]] || die "$1 requires a value"
        [[ "${INSTALL_SET_RELAY_URLS}" -eq 0 ]] || die "termd can connect to only one relay; pass a single --relay"
        TERMD_RELAY_URLS="$(append_space_separated "${TERMD_RELAY_URLS:-}" "$2")"
        INSTALL_SET_RELAY_URLS=1
        shift 2
        ;;
      --relay-daemon-token-file)
        [[ $# -ge 2 && -n "$2" ]] || die "--relay-daemon-token-file requires a non-empty value"
        TERMD_RELAY_DAEMON_TOKEN_FILE="$2"
        INSTALL_SET_RELAY_DAEMON_TOKEN_FILE=1
        shift 2
        ;;
      --relay-setup-token-file)
        [[ $# -ge 2 && -n "$2" ]] || die "--relay-setup-token-file requires a non-empty value"
        TERMD_RELAY_SETUP_TOKEN_FILE="$2"
        INSTALL_SET_RELAY_SETUP_TOKEN_FILE=1
        shift 2
        ;;
      --allow-open-relay)
        INSTALL_ALLOW_OPEN_RELAY=1
        shift
        ;;
      --proxy|--relay-proxy)
        [[ $# -ge 2 && -n "$2" ]] || die "$1 requires a non-empty value"
        http_proxy="$2"
        https_proxy="$2"
        HTTP_PROXY="$2"
        HTTPS_PROXY="$2"
        INSTALL_SET_PROXY=1
        shift 2
        ;;
      --tls-cert)
        [[ $# -ge 2 && -n "$2" ]] || die "--tls-cert requires a non-empty value"
        TERMD_TLS_CERT="$2"
        INSTALL_SET_TLS_CERT=1
        shift 2
        ;;
      --tls-key)
        [[ $# -ge 2 && -n "$2" ]] || die "--tls-key requires a non-empty value"
        TERMD_TLS_KEY="$2"
        INSTALL_SET_TLS_KEY=1
        shift 2
        ;;
      --supervisor-version)
        [[ $# -ge 2 && -n "$2" ]] || die "--supervisor-version requires a non-empty value"
        SUPERVISOR_VERSION="$2"
        SUPERVISOR_VERSION_EXPLICIT=1
        INSTALL_SET_SUPERVISOR_VERSION=1
        shift 2
        ;;
      --allow-session-loss)
        INSTALL_ALLOW_SESSION_LOSS=1
        shift
        ;;
      --user)
        [[ $# -ge 2 && -n "$2" ]] || die "--user requires a non-empty value"
        SERVICE_USER="$2"
        SERVICE_GROUP="$2"
        SERVICE_GROUP_FROM_UNIT=0
        INSTALL_SET_USER=1
        shift 2
        ;;
      --uninstall)
        ACTION="uninstall"
        shift
        ;;
      --purge)
        ACTION="uninstall"
        PURGE_STATE=1
        shift
        ;;
      *)
        die "unknown installer argument: $1"
        ;;
    esac
  done
}

read_systemd_unit_assignment() {
  local key="$1"
  local file="$2"

  [[ -r "$file" ]] || return 0
  awk -F= -v key="$key" '
    $1 == key {
      value = substr($0, length(key) + 2)
      gsub(/^[ \t]+|[ \t]+$/, "", value)
      print value
      exit
    }
  ' "$file"
}

inherit_existing_service_identity() {
  if [[ "$INSTALL_SET_USER" -eq 1 || ! -e "$UNIT_FILE" ]]; then
    return 0
  fi

  local existing_user existing_group existing_working_directory
  existing_user="$(read_systemd_unit_assignment "User" "$UNIT_FILE")"
  existing_group="$(read_systemd_unit_assignment "Group" "$UNIT_FILE")"
  existing_working_directory="$(read_systemd_unit_assignment "WorkingDirectory" "$UNIT_FILE")"

  if [[ -n "$existing_user" ]]; then
    # 重装/升级时不带 --user 应保留既有 systemd User，避免把个人账号服务重写成 termd。
    SERVICE_USER="$existing_user"
    if [[ -n "$existing_group" ]]; then
      # 既有 unit 显式写了 Group 时要一起继承，后续账号解析不能再改成用户主组。
      SERVICE_GROUP="$existing_group"
      SERVICE_GROUP_FROM_UNIT=1
    else
      SERVICE_GROUP="$existing_user"
      SERVICE_GROUP_FROM_UNIT=0
    fi
  fi

  if [[ -n "$existing_working_directory" ]]; then
    PREVIOUS_STATE_DIR="$existing_working_directory"
  fi
}

read_sqlite_meta_value() {
  local sqlite_path="$1"
  local key="$2"

  [[ -f "$sqlite_path" ]] || return 0

  python3 - "$sqlite_path" "$key" <<'PY'
import sqlite3
import sys

path = sys.argv[1]
key = sys.argv[2]

try:
    conn = sqlite3.connect(path)
    try:
        row = conn.execute(
            "SELECT value FROM daemon_meta WHERE key = ?",
            (key,),
        ).fetchone()
        if row and row[0] is not None:
            print(row[0])
    finally:
        conn.close()
except sqlite3.Error:
    pass
PY
}

upsert_sqlite_meta_value() {
  local sqlite_path="$1"
  local key="$2"
  local value="$3"

  python3 - "$sqlite_path" "$key" "$value" <<'PY'
import sqlite3
import sys
import time

path = sys.argv[1]
key = sys.argv[2]
value = sys.argv[3]

conn = sqlite3.connect(path)
try:
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS daemon_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
        )
        """
    )
    now_ms = int(time.time() * 1000)
    conn.execute(
        """
        INSERT INTO daemon_meta (key, value, updated_at_ms)
        VALUES (?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at_ms = excluded.updated_at_ms
        """,
        (key, value, now_ms),
    )
    conn.commit()
finally:
    conn.close()
PY
}

repair_installer_poisoned_state_db() {
  local sqlite_path="$1"
  local supervisor_dir="$2"

  [[ -f "$sqlite_path" ]] || return 1
  python3 - "$sqlite_path" "$supervisor_dir" <<'PY'
import sqlite3
import stat
import sys
from pathlib import Path

path = Path(sys.argv[1]).resolve()
supervisor_dir = Path(sys.argv[2]).resolve()
allowed_tables = {
    "daemon_meta",
    "trusted_devices",
    "runtime_sessions",
    "http_uploads",
    "daemon_clients",
    "daemon_client_attached_sessions",
    "daemon_sessions",
    "session_ownership",
}


def has_supervisor_socket() -> bool:
    try:
        return any(
            stat.S_ISSOCK(entry.stat(follow_symlinks=False).st_mode)
            for entry in supervisor_dir.iterdir()
        )
    except FileNotFoundError:
        return False
    except OSError:
        return True


try:
    conn = sqlite3.connect(path)
    try:
        conn.execute("BEGIN IMMEDIATE")
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
            )
        }
        if "daemon_meta" not in tables or not tables.issubset(allowed_tables):
            raise RuntimeError("unexpected state tables")
        meta_keys = {row[0] for row in conn.execute("SELECT key FROM daemon_meta")}
        if meta_keys != {"supervisor_version"}:
            raise RuntimeError("unexpected daemon metadata")
        for table in sorted(tables - {"daemon_meta"}):
            if conn.execute(f'SELECT 1 FROM "{table}" LIMIT 1').fetchone() is not None:
                raise RuntimeError("state database contains user data")
        if has_supervisor_socket():
            raise RuntimeError("live supervisor socket exists")
        conn.execute("DELETE FROM daemon_meta WHERE key = 'supervisor_version'")
        conn.commit()
    except BaseException:
        conn.rollback()
        raise
    finally:
        conn.close()
except (OSError, RuntimeError, sqlite3.Error):
    raise SystemExit(1)
PY
}

sqlite_has_runtime_sessions() {
  local sqlite_path="$1"

  [[ -f "$sqlite_path" ]] || return 1

  python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys

path = sys.argv[1]

try:
    conn = sqlite3.connect(path)
    try:
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        for table in (
            "daemon_client_attached_sessions",
            "daemon_sessions",
            "runtime_sessions",
        ):
            if table in tables:
                row = conn.execute(f"SELECT 1 FROM {table} LIMIT 1").fetchone()
                if row is not None:
                    raise SystemExit(0)
    finally:
        conn.close()
except sqlite3.Error:
    pass

raise SystemExit(1)
PY
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'amd64' ;;
    aarch64|arm64) printf 'arm64' ;;
    *) printf '' ;;
  esac
}

fetch_file() {
  local url="$1"
  local dest="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    die "need curl or wget to download release assets"
  fi
}

resolve_version() {
  if [[ -n "$VERSION" ]]; then
    return 0
  fi

  local api_url="https://api.github.com/repos/${REPO}/releases/latest"
  local tmp_json
  tmp_json="$(mktemp)"
  fetch_file "$api_url" "$tmp_json" || die "failed to query latest release from ${REPO}"
  VERSION="$(python3 - "$tmp_json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text())
tag = data.get("tag_name", "").strip()
if not tag:
    raise SystemExit("latest release did not include a tag_name")
print(tag)
PY
)"
  rm -f "$tmp_json"
}

verify_release_archive() {
  local archive="$1"
  local checksums="$2"

  local expected actual
  expected="$(awk -v name="$(basename "$archive")" '$2 == name { print $1 }' "$checksums")"
  actual="$(sha256sum "$archive" | awk '{print $1}')"
  [[ -n "$expected" && "$expected" == "$actual" ]]
}

install_from_release() {
  local destination="${1:-${INSTALL_PREFIX}/bin/${BIN_NAME}}"
  local arch archive_name archive_url checksums_url tmp_dir archive_path checksums_path

  arch="$(detect_arch)"
  [[ -n "$arch" ]] || return 1
  if [[ "$arch" != "amd64" ]]; then
    # 当前 release workflow 只发布 linux-amd64；arm64 明确走源码 fallback。
    log "当前 release 只发布 linux-amd64；linux-${arch} 将从源码构建"
    return 1
  fi

  tmp_dir="$(mktemp -d)"
  archive_name="${BIN_NAME}-${VERSION}-linux-${arch}.tar.gz"
  archive_url="https://github.com/${REPO}/releases/download/${VERSION}/${archive_name}"
  checksums_url="https://github.com/${REPO}/releases/download/${VERSION}/checksums.txt"
  archive_path="${tmp_dir}/${archive_name}"
  checksums_path="${tmp_dir}/checksums.txt"

  if ! fetch_file "$archive_url" "$archive_path"; then
    rm -rf "$tmp_dir"
    return 1
  fi

  if ! fetch_file "$checksums_url" "$checksums_path"; then
    rm -rf "$tmp_dir"
    return 1
  fi

  if ! verify_release_archive "$archive_path" "$checksums_path"; then
    rm -rf "$tmp_dir"
    return 1
  fi

  tar -xzf "$archive_path" -C "$tmp_dir"
  install -Dm0755 "$tmp_dir/$BIN_NAME" "$destination"
  rm -rf "$tmp_dir"
  return 0
}

install_from_source() {
  require_cmd cargo
  require_cmd git

  local destination="${1:-${INSTALL_PREFIX}/bin/${BIN_NAME}}"
  local src_dir build_web_ui=0
  if web_ui_requested; then
    require_cmd node
    require_cmd npm
    build_web_ui=1
  fi
  src_dir="$(mktemp -d)"

  log "falling back to source build from ${REPO}@${VERSION}"
  if ! git clone --depth 1 --branch "$VERSION" "https://github.com/${REPO}.git" "$src_dir/repo"; then
    rm -rf "$src_dir"
    die "failed to clone ${REPO}@${VERSION}; installed binary was not changed"
  fi
  if [[ "$build_web_ui" -eq 1 ]] && ! build_frontend_for_source "$src_dir/repo"; then
    rm -rf "$src_dir"
    die "failed to build Web UI from ${REPO}@${VERSION}; installed binary was not changed"
  fi
  if ! (
    cd "$src_dir/repo"
    cargo build --release --locked -p "$COMPONENT" --bin "$BIN_NAME"
  ); then
    rm -rf "$src_dir"
    die "failed to build ${BIN_NAME} from source; installed binary was not changed"
  fi
  install -Dm0755 "$src_dir/repo/target/release/$BIN_NAME" "$destination"
  rm -rf "$src_dir"
}

install_from_self_binary() {
  local destination="$1"

  install -m0755 -- "$SELF_INSTALL_BINARY" "$destination"
}

upsert_env_var() {
  local key="$1"
  local value="$2"
  local quoted tmp

  printf -v quoted '%q' "$value"
  tmp="$(mktemp)"
  awk -v key="$key" -v line="${key}=${quoted}" '
    $0 ~ "^[[:space:]]*#?[[:space:]]*" key "=" {
      if (!done) {
        print line
        done = 1
      }
      next
    }
    { print }
    END {
      if (!done) {
        print line
      }
    }
  ' "$ENV_FILE" >"$tmp"
  cat "$tmp" >"$ENV_FILE"
  rm -f "$tmp"
}

unset_env_var() {
  local key="$1"
  local tmp

  tmp="$(mktemp)"
  awk -v key="$key" '
    $0 ~ "^[[:space:]]*" key "=" { next }
    { print }
  ' "$ENV_FILE" >"$tmp"
  cat "$tmp" >"$ENV_FILE"
  rm -f "$tmp"
}

write_service_secret_file() {
  local path="$1"
  local value="$2"
  local old_umask

  install -d -m 0755 "$ENV_DIR"
  old_umask="$(umask)"
  umask 077
  printf '%s\n' "$value" >"$path"
  umask "$old_umask"
  chown root:"$SERVICE_GROUP" "$path"
  chmod 0640 "$path"
}

generate_secret_token() {
  python3 - <<'PY'
import secrets

print(secrets.token_urlsafe(32))
PY
}

read_first_line() {
  local path="$1"
  IFS= read -r REPLY <"$path"
  printf '%s' "$REPLY"
}

read_default_supervisor_version() {
  local supervisor_version_file="$ROOT_DIR/SUPERVISOR_VERSION"

  if [[ -n "$REQUIRED_SUPERVISOR_VERSION" ]]; then
    printf '%s' "$REQUIRED_SUPERVISOR_VERSION"
    return 0
  fi

  if [[ -s "$supervisor_version_file" ]]; then
    local file_version
    IFS= read -r file_version <"$supervisor_version_file"
    if [[ -n "$file_version" ]]; then
      printf '%s' "$file_version"
      return 0
    fi
  fi

  printf '%s' "${VERSION:-}"
}

apply_env_overrides() {
  # 命令行参数只覆盖用户显式传入的项，避免重装时意外抹掉已有 systemd 配置。
  # 0.7 只使用 daemon admission token；升级时移除已废弃的 relay transport token 配置。
  unset_env_var "TERMD_RELAY_AUTH_TOKEN"
  unset_env_var "TERMD_RELAY_AUTH_TOKEN_FILE"
  if [[ "$INSTALL_SET_LISTEN" -eq 1 ]]; then
    upsert_env_var "TERMD_LISTEN" "$TERMD_LISTEN"
  fi
  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    upsert_env_var "TERMD_WEB_ENABLED" "$TERMD_WEB_ENABLED"
  fi
  if [[ "$INSTALL_SET_RELAY_URLS" -eq 1 ]]; then
    upsert_env_var "TERMD_RELAY_URLS" "$TERMD_RELAY_URLS"
  fi
  if [[ "$INSTALL_SET_RELAY_DAEMON_TOKEN_FILE" -eq 1 ]]; then
    upsert_env_var "TERMD_RELAY_DAEMON_TOKEN_FILE" "$TERMD_RELAY_DAEMON_TOKEN_FILE"
  fi
  if [[ -n "${TERMD_RELAY_URLS:-}" && "$INSTALL_SET_RELAY_DAEMON_TOKEN_FILE" -eq 0 && "$INSTALL_ALLOW_OPEN_RELAY" -eq 0 ]]; then
    TERMD_RELAY_DAEMON_TOKEN_FILE="/etc/termd/termd_daemon_token"
    if [[ ! -s "$TERMD_RELAY_DAEMON_TOKEN_FILE" ]]; then
      write_service_secret_file "$TERMD_RELAY_DAEMON_TOKEN_FILE" "$(generate_secret_token)"
    fi
    upsert_env_var "TERMD_RELAY_DAEMON_TOKEN_FILE" "$TERMD_RELAY_DAEMON_TOKEN_FILE"
  fi
  if [[ "$INSTALL_SET_PROXY" -eq 1 ]]; then
    upsert_env_var "HTTP_PROXY" "$HTTP_PROXY"
    upsert_env_var "HTTPS_PROXY" "$HTTPS_PROXY"
  fi
  if [[ "$INSTALL_SET_TLS_CERT" -eq 1 ]]; then
    upsert_env_var "TERMD_TLS_CERT" "$TERMD_TLS_CERT"
  fi
  if [[ "$INSTALL_SET_TLS_KEY" -eq 1 ]]; then
    upsert_env_var "TERMD_TLS_KEY" "$TERMD_TLS_KEY"
  fi
  if [[ "$INSTALL_SET_SUPERVISOR_VERSION" -eq 1 ]]; then
    upsert_env_var "TERMD_SUPERVISOR_VERSION" "$SUPERVISOR_VERSION"
  fi
}

apply_service_env_defaults() {
  # HOME/SHELL 由 daemon 的 systemd 运行身份决定，始终写进 env 文件，避免 unit 和 env 文件各自维护。
  upsert_env_var "HOME" "$SERVICE_HOME"
  upsert_env_var "SHELL" "$SERVICE_SHELL"
}

prompt_confirmation() {
  local message="$1"
  local answer confirm_fd

  if [[ "$INSTALL_ALLOW_SESSION_LOSS" -eq 1 ]]; then
    return 0
  fi

  if [[ -n "${TERMD_INSTALL_CONFIRM_FD:-}" ]]; then
    confirm_fd="$TERMD_INSTALL_CONFIRM_FD"
    [[ "$confirm_fd" =~ ^[0-9]+$ ]] || die "TERMD_INSTALL_CONFIRM_FD must be a numeric file descriptor"
  else
    confirm_fd=9
    exec 9</dev/tty || return 1
  fi

  printf '%s [y/N] ' "$message" >&2
  if ! read -r -u "$confirm_fd" answer; then
    if [[ -z "${TERMD_INSTALL_CONFIRM_FD:-}" ]]; then
      exec 9<&-
    fi
    return 1
  fi
  if [[ -z "${TERMD_INSTALL_CONFIRM_FD:-}" ]]; then
    exec 9<&-
  fi

  case "${answer,,}" in
    y|yes)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

clear_runtime_session_state() {
  local sqlite_path="$1"
  local _supervisor_dir="$2"

  if [[ -f "$sqlite_path" ]]; then
    python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys

path = sys.argv[1]
conn = sqlite3.connect(path)
try:
    tables = {
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        )
    }
    if "daemon_client_attached_sessions" in tables:
        conn.execute("DELETE FROM daemon_client_attached_sessions")
    # state 目录迁移只清运行态，保留展示行名称，方便人工排查旧 session 来源。
    if "daemon_sessions" in tables:
        columns = {
            row[1]
            for row in conn.execute("PRAGMA table_info(daemon_sessions)")
        }
        if {"state", "updated_at_ms"}.issubset(columns):
            conn.execute(
                "UPDATE daemon_sessions SET state = 'closed', updated_at_ms = ?",
                (int(__import__("time").time() * 1000),),
            )
    if "runtime_sessions" in tables:
        columns = {
            row[1]
            for row in conn.execute("PRAGMA table_info(runtime_sessions)")
        }
        if {"state", "updated_at_ms", "restore_kind", "restore_value"}.issubset(columns):
            conn.execute(
                """
                UPDATE runtime_sessions
                SET state = 'closed',
                    updated_at_ms = ?,
                    restore_kind = NULL,
                    restore_value = NULL
                """,
                (int(__import__("time").time() * 1000),),
            )
        else:
            conn.execute("DELETE FROM runtime_sessions")
    conn.commit()
finally:
    conn.close()
PY
  fi

  # 这个函数只用于非 supervisor 兼容版本切换的清理路径，不终止 supervisor。
  # supervisor 版本升级必须走 clear_runtime_session_state_for_supervisor_upgrade。
}

terminate_session_supervisors() {
  local supervisor_dir="$1"

  [[ -d "$supervisor_dir" ]] || return 0

  python3 - "$supervisor_dir" <<'PY'
import os
import signal
import sys
import time
from pathlib import Path

target_dir = Path(sys.argv[1]).resolve()
proc_dir = Path("/proc")
if not proc_dir.exists():
    raise SystemExit("cannot inspect /proc to terminate old session supervisors")


def process_is_alive(pid: int) -> bool:
    status_path = proc_dir / str(pid) / "status"
    try:
        for line in status_path.read_text(errors="replace").splitlines():
            if line.startswith("State:"):
                # 僵尸进程已经不能继续托管 PTY，可视为已退出。
                return not line.split(None, 2)[1].startswith("Z")
    except FileNotFoundError:
        return False
    except OSError:
        return True
    return True


def session_supervisor_pids() -> list[int]:
    matched: list[int] = []
    for entry in proc_dir.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        if pid == os.getpid():
            continue
        try:
            raw_cmdline = (entry / "cmdline").read_bytes()
        except (FileNotFoundError, PermissionError, OSError):
            continue
        args = [
            part.decode("utf-8", errors="surrogateescape")
            for part in raw_cmdline.split(b"\0")
            if part
        ]
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
        if socket_parent == target_dir:
            matched.append(pid)
    return matched


pids = session_supervisor_pids()
for pid in pids:
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    except PermissionError as error:
        raise SystemExit(f"failed to terminate session supervisor {pid}: {error}")

deadline = time.monotonic() + 5
remaining = {pid for pid in pids if process_is_alive(pid)}
while remaining and time.monotonic() < deadline:
    time.sleep(0.1)
    remaining = {pid for pid in remaining if process_is_alive(pid)}

for pid in sorted(remaining):
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError as error:
        raise SystemExit(f"failed to kill session supervisor {pid}: {error}")

deadline = time.monotonic() + 5
remaining = {pid for pid in remaining if process_is_alive(pid)}
while remaining and time.monotonic() < deadline:
    time.sleep(0.1)
    remaining = {pid for pid in remaining if process_is_alive(pid)}

if remaining:
    raise SystemExit(
        "session supervisors did not exit after supervisor version upgrade: "
        + ", ".join(str(pid) for pid in sorted(remaining))
    )
PY
}

clear_runtime_session_state_for_supervisor_upgrade() {
  local sqlite_path="$1"
  local supervisor_dir="$2"

  # supervisor 兼容版本切换不能让旧 supervisor 被新 daemon 重新领养；
  # 用户确认后必须先终止旧 supervisor，再清空所有 session 运行态和展示态数据。
  terminate_session_supervisors "$supervisor_dir"

  if [[ -f "$sqlite_path" ]]; then
    python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys

path = sys.argv[1]
conn = sqlite3.connect(path)
try:
    tables = {
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        )
    }
    for table in (
        "daemon_client_attached_sessions",
        "daemon_sessions",
        "runtime_sessions",
    ):
        if table in tables:
            conn.execute(f"DELETE FROM {table}")
    conn.commit()
finally:
    conn.close()
PY
  fi

  if [[ -d "$supervisor_dir" ]]; then
    find "$supervisor_dir" -maxdepth 1 -type s -name '*.sock' -delete
  fi
}

resolve_supervisor_version() {
  local sqlite_path current_supervisor_version desired_supervisor_version
  sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  current_supervisor_version="$(read_sqlite_meta_value "$sqlite_path" "supervisor_version")"
  local has_runtime_sessions=0
  if sqlite_has_runtime_sessions "$sqlite_path"; then
    has_runtime_sessions=1
  fi

  if [[ "$SUPERVISOR_VERSION_EXPLICIT" -eq 1 && "$SUPERVISOR_VERSION_REQUIRED" -eq 1 && "$SUPERVISOR_VERSION" != "$REQUIRED_SUPERVISOR_VERSION" ]]; then
    die "requested supervisor version ${SUPERVISOR_VERSION} conflicts with required release supervisor version ${REQUIRED_SUPERVISOR_VERSION}"
  fi

  if [[ "$SUPERVISOR_VERSION_EXPLICIT" -eq 1 ]]; then
    desired_supervisor_version="$SUPERVISOR_VERSION"
  elif [[ "$SUPERVISOR_VERSION_REQUIRED" -eq 1 ]]; then
    desired_supervisor_version="$REQUIRED_SUPERVISOR_VERSION"
  elif [[ -n "$current_supervisor_version" ]]; then
    desired_supervisor_version="$current_supervisor_version"
  else
    desired_supervisor_version="${SUPERVISOR_VERSION:-$(read_default_supervisor_version)}"
  fi
  [[ -n "$desired_supervisor_version" ]] || die "unable to determine supervisor version"

  if [[ "$SUPERVISOR_VERSION_EXPLICIT" -eq 1 || "$SUPERVISOR_VERSION_REQUIRED" -eq 1 ]] && [[ -n "$current_supervisor_version" && "$current_supervisor_version" != "$desired_supervisor_version" ]]; then
    log "supervisor version change detected: ${current_supervisor_version} -> ${desired_supervisor_version}"
    if ! prompt_confirmation "updating the supervisor version will lose existing sessions; confirm you are prepared for session loss and continue"; then
      die "supervisor version upgrade cancelled"
    fi
    SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR=1
  elif [[ "$SUPERVISOR_VERSION_EXPLICIT" -eq 1 || "$SUPERVISOR_VERSION_REQUIRED" -eq 1 ]] && [[ -z "$current_supervisor_version" && "$has_runtime_sessions" -eq 1 ]]; then
    log "supervisor version baseline will be set to ${desired_supervisor_version} for this daemon"
    if ! prompt_confirmation "setting the initial supervisor version will lose existing sessions; confirm you are prepared for session loss and continue"; then
      die "supervisor version initialization cancelled"
    fi
    SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR=1
  elif [[ -z "$current_supervisor_version" && "$has_runtime_sessions" -eq 1 ]]; then
    # 旧安装可能已有 session 但还没有 supervisor_version 元数据；默认更新只补 baseline，
    # 避免普通二进制升级把用户的持久 session 当作不兼容 runtime 删除。
    log "supervisor version baseline will be set to ${desired_supervisor_version} without clearing existing sessions"
  fi

  SUPERVISOR_VERSION="$desired_supervisor_version"
  INSTALL_SET_SUPERVISOR_VERSION=1
}

assert_session_ownership_quiescent() {
  local sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  [[ -f "$sqlite_path" ]] || return 0

  local pending
  if ! pending="$(python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys
from pathlib import Path
from urllib.parse import quote

path = Path(sys.argv[1]).resolve()
conn = sqlite3.connect(f"file:{quote(str(path))}?mode=ro", uri=True)
try:
    table = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_ownership'"
    ).fetchone()
    if table is None:
        print(0)
    else:
        print(conn.execute(
            "SELECT COUNT(*) FROM session_ownership WHERE phase IN ('preparing', 'cleaning')"
        ).fetchone()[0])
finally:
    conn.close()
PY
  )"; then
    die "failed to run the read-only session ownership rollback precheck for ${sqlite_path}; installed binary was not changed"
  fi
  [[ "$pending" =~ ^[0-9]+$ ]] || die "invalid session ownership rollback precheck result; installed binary was not changed"
  if [[ "$pending" -ne 0 ]]; then
    die "cannot replace termd while ${pending} session ownership operation(s) are preparing or cleaning; leave the current daemon running until they converge, then retry"
  fi
}

restore_service_after_failed_binary_commit() {
  if [[ "$INSTALL_SERVICE_WAS_ACTIVE" -eq 1 ]]; then
    systemctl start "$SERVICE_NAME" || die "failed to restore ${SERVICE_NAME}.service after install was blocked"
  fi
}

commit_staged_binary() {
  local candidate="$1"
  local target_dir="${INSTALL_PREFIX}/bin"
  local target="${target_dir}/${BIN_NAME}"
  local staged_target

  [[ -f "$candidate" ]] || return 1
  INSTALL_SERVICE_WAS_ACTIVE=0
  if systemctl is-active --quiet "$SERVICE_NAME"; then
    INSTALL_SERVICE_WAS_ACTIVE=1
    if ! systemctl stop "$SERVICE_NAME"; then
      return 1
    fi
  fi

  # 停止 daemon 后重新读取 ledger，关闭初检与 binary commit 之间的写入窗口。
  if ! (assert_session_ownership_quiescent); then
    restore_service_after_failed_binary_commit
    return 1
  fi

  install -d -m0755 "$target_dir" || {
    restore_service_after_failed_binary_commit
    return 1
  }
  staged_target="$(mktemp "${target_dir}/.${BIN_NAME}.install.XXXXXX")" || {
    restore_service_after_failed_binary_commit
    return 1
  }
  if ! install -m0755 "$candidate" "$staged_target"; then
    rm -f "$staged_target"
    restore_service_after_failed_binary_commit
    return 1
  fi
  if ! mv -f "$staged_target" "$target"; then
    rm -f "$staged_target"
    restore_service_after_failed_binary_commit
    return 1
  fi
  return 0
}

snapshot_install_file() {
  local key="$1"
  local path="$2"

  if [[ -e "$path" || -L "$path" ]]; then
    printf 'present\n' >"${INSTALL_ROLLBACK_DIR}/${key}.state"
    cp -a -- "$path" "${INSTALL_ROLLBACK_DIR}/${key}"
  else
    printf 'absent\n' >"${INSTALL_ROLLBACK_DIR}/${key}.state"
  fi
}

restore_install_file() {
  local key="$1"
  local path="$2"
  local state

  state="$(cat "${INSTALL_ROLLBACK_DIR}/${key}.state")" || return 1
  rm -f -- "$path" || return 1
  if [[ "$state" == "present" ]]; then
    install -d -m0755 "$(dirname "$path")" || return 1
    cp -a -- "${INSTALL_ROLLBACK_DIR}/${key}" "$path" || return 1
  fi
}

prepare_install_rollback() {
  INSTALL_ROLLBACK_DIR="${INSTALL_STAGING_DIR}/rollback"
  rm -rf -- "$INSTALL_ROLLBACK_DIR" || return 1
  install -d -m0700 "$INSTALL_ROLLBACK_DIR" || return 1
  snapshot_install_file binary "${INSTALL_PREFIX}/bin/${BIN_NAME}" || return 1
  snapshot_install_file env "$ENV_FILE" || return 1
  snapshot_install_file wrapper "$WRAPPER_FILE" || return 1
  snapshot_install_file unit "$UNIT_FILE" || return 1
  INSTALL_SERVICE_WAS_ENABLED=0
  if systemctl is-enabled --quiet "$SERVICE_NAME"; then
    INSTALL_SERVICE_WAS_ENABLED=1
  fi
}

rollback_failed_install() {
  local rollback_failed=0

  restore_install_file binary "${INSTALL_PREFIX}/bin/${BIN_NAME}" || rollback_failed=1
  restore_install_file env "$ENV_FILE" || rollback_failed=1
  restore_install_file wrapper "$WRAPPER_FILE" || rollback_failed=1
  restore_install_file unit "$UNIT_FILE" || rollback_failed=1
  if [[ "$INSTALL_BINARY_COMMITTED" -eq 0 ]]; then
    [[ "$rollback_failed" -eq 0 ]]
    return
  fi
  systemctl daemon-reload || rollback_failed=1
  if [[ "$INSTALL_SERVICE_WAS_ENABLED" -eq 1 ]]; then
    systemctl enable "$SERVICE_NAME" || rollback_failed=1
  else
    systemctl disable "$SERVICE_NAME" || rollback_failed=1
  fi
  if [[ "$INSTALL_SERVICE_WAS_ACTIVE" -eq 1 ]]; then
    systemctl start "$SERVICE_NAME" || rollback_failed=1
  else
    systemctl stop "$SERVICE_NAME" || rollback_failed=1
  fi
  if [[ "$rollback_failed" -ne 0 ]]; then
    printf '[%s-install] rollback after installation failure was incomplete; inspect binary, unit, env, and service state\n' "$COMPONENT" >&2
    return 1
  fi
  return 0
}

prepare_install_before_binary_commit() {
  ensure_system_user || return $?
  chown_state_dir || return $?
  write_env_file || return $?
  # 重新读取最终 env，保证后续 wrapper 和初始 pairing token 都使用同一组监听/TLS 配置。
  # shellcheck source=/dev/null
  source "$ENV_FILE" || return $?
  write_wrapper || return $?
  write_unit || return $?
}

complete_install_after_binary_commit() {
  stop_service_before_supervisor_runtime_clear || return $?
  clear_session_state_after_state_dir_change || return $?
  persist_supervisor_version || return $?
  # installer 可能首次创建 SQLite 元数据文件；写入 supervisor_version 后要重新归属给服务用户。
  chown_state_dir || return $?
  systemctl daemon-reload || return $?
  systemctl enable "$SERVICE_NAME" || return $?
  systemctl restart "$SERVICE_NAME" || return $?
  persist_deferred_supervisor_version || return $?
}

install_staged_candidate() {
  local candidate="$1"
  local status

  INSTALL_BINARY_COMMITTED=0
  prepare_install_rollback || return $?
  if prepare_install_before_binary_commit; then
    status=0
  else
    status=$?
    rollback_failed_install || true
    return "$status"
  fi
  if ! commit_staged_binary "$candidate"; then
    rollback_failed_install || true
    return 1
  fi
  INSTALL_BINARY_COMMITTED=1

  if complete_install_after_binary_commit; then
    status=0
  else
    status=$?
  fi
  if [[ "$status" -ne 0 ]]; then
    rollback_failed_install || true
    return "$status"
  fi
  rm -rf -- "$INSTALL_ROLLBACK_DIR"
  INSTALL_ROLLBACK_DIR=""
  return 0
}

cleanup_install_staging() {
  if [[ -n "$INSTALL_STAGING_DIR" ]]; then
    rm -rf "$INSTALL_STAGING_DIR"
    INSTALL_STAGING_DIR=""
  fi
}

persist_supervisor_version() {
  local sqlite_path schema_version
  sqlite_path="${STATE_DIR}/daemon-state.sqlite"

  if [[ "$SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR" -eq 1 ]]; then
    clear_runtime_session_state_for_supervisor_upgrade "$sqlite_path" "${STATE_DIR}/termd-supervisors"
  fi

  if [[ "$INSTALL_SET_SUPERVISOR_VERSION" -ne 1 ]]; then
    return 0
  fi

  # The daemon owns initial schema creation. Writing daemon_meta into a missing database here
  # makes the daemon treat fresh state as an incompatible legacy database.
  if [[ ! -f "$sqlite_path" ]]; then
    SUPERVISOR_VERSION_PERSIST_DEFERRED=1
    return 0
  fi

  schema_version="$(read_sqlite_meta_value "$sqlite_path" "state_schema_version")"
  if [[ -z "$schema_version" ]]; then
    if ! repair_installer_poisoned_state_db "$sqlite_path" "${STATE_DIR}/termd-supervisors"; then
      log "state database has no state_schema_version and is not an empty installer-created database; leaving it unchanged"
      return 1
    fi
    log "recovered incomplete supervisor metadata left by an earlier fresh installation"
    SUPERVISOR_VERSION_PERSIST_DEFERRED=1
    return 0
  fi

  upsert_sqlite_meta_value "$sqlite_path" "supervisor_version" "$SUPERVISOR_VERSION"
}

persist_deferred_supervisor_version() {
  local sqlite_path schema_version

  if [[ "$SUPERVISOR_VERSION_PERSIST_DEFERRED" -ne 1 ]]; then
    return 0
  fi

  sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  for _ in {1..40}; do
    schema_version="$(read_sqlite_meta_value "$sqlite_path" "state_schema_version")"
    if [[ -n "$schema_version" ]]; then
      upsert_sqlite_meta_value "$sqlite_path" "supervisor_version" "$SUPERVISOR_VERSION" || return $?
      # The root-run SQLite writer may create or replace WAL/SHM sidecars.
      chown_state_dir || return $?
      SUPERVISOR_VERSION_PERSIST_DEFERRED=0
      return 0
    fi
    sleep 0.25
  done

  log "daemon state schema was not initialized after ${SERVICE_NAME}.service started"
  return 1
}

stop_service_before_supervisor_runtime_clear() {
  if [[ "$SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR" -ne 1 ]]; then
    return 0
  fi

  # supervisor 兼容版本升级已经由用户确认会丢 session；清 SQLite 之前必须先停旧 daemon，
  # 否则旧 daemon 可能在 systemctl restart 前把内存里的 session 再写回数据库。
  if systemctl is-active --quiet "$SERVICE_NAME"; then
    log "stopping ${SERVICE_NAME}.service before clearing supervisor runtime state"
    systemctl stop "$SERVICE_NAME" || die "failed to stop ${SERVICE_NAME}.service before supervisor upgrade"
  fi
}

resolve_service_identity() {
  [[ "$SERVICE_USER" =~ ^[A-Za-z_][A-Za-z0-9_.-]*[$]?$ ]] || die "invalid --user value: ${SERVICE_USER}"

  if id -u "$SERVICE_USER" >/dev/null 2>&1; then
    local passwd_entry primary_group
    primary_group="$(id -gn "$SERVICE_USER")"
    passwd_entry="$(getent passwd "$SERVICE_USER")"
    if [[ "$SERVICE_GROUP_FROM_UNIT" -eq 1 ]]; then
      [[ "$SERVICE_GROUP" =~ ^[A-Za-z_][A-Za-z0-9_.-]*[$]?$ ]] || die "invalid Group value in existing service: ${SERVICE_GROUP}"
      getent group "$SERVICE_GROUP" >/dev/null 2>&1 || die "system group ${SERVICE_GROUP} from existing service does not exist"
    else
      SERVICE_GROUP="$primary_group"
    fi
    SERVICE_HOME="$(printf '%s' "$passwd_entry" | cut -d: -f6)"
    SERVICE_SHELL="$(printf '%s' "$passwd_entry" | cut -d: -f7)"
  else
    if [[ "$SERVICE_USER" != "termd" ]]; then
      die "system user ${SERVICE_USER} does not exist; create it first or omit --user to use the managed termd user"
    fi
    SERVICE_GROUP="termd"
    SERVICE_HOME="/var/lib/termd"
    SERVICE_SHELL="/bin/sh"
  fi

  [[ -n "$SERVICE_HOME" ]] || SERVICE_HOME="/var/lib/${SERVICE_USER}"
  [[ -n "$SERVICE_SHELL" && "$SERVICE_SHELL" != "/usr/sbin/nologin" && "$SERVICE_SHELL" != "/sbin/nologin" ]] || SERVICE_SHELL="/bin/sh"
  # daemon state、SQLite 和 supervisor socket 路径必须稳定；--user 只影响 shell 的 HOME/SHELL，
  # 不再改变持久化根目录，避免升级或切换用户后丢失/错连会话状态。
  STATE_DIR="${TERMD_STATE_DIR:-/var/lib/termd}"
}

write_env_file() {
  # systemd 服务会以目标用户运行 wrapper；env 文件需要允许该用户的主组读取。
  install -d -m 0755 "$ENV_DIR"

  if [[ -e "$ENV_FILE" ]]; then
    log "keeping existing env file at ${ENV_FILE}"
    apply_service_env_defaults
    apply_env_overrides
    chown root:"$SERVICE_GROUP" "$ENV_FILE"
    chmod 0640 "$ENV_FILE"
    return 0
  fi

  if [[ -n "${TERMD_RELAY_URLS:-}" && "$INSTALL_SET_RELAY_DAEMON_TOKEN_FILE" -eq 0 && "$INSTALL_ALLOW_OPEN_RELAY" -eq 0 ]]; then
    TERMD_RELAY_DAEMON_TOKEN_FILE="/etc/termd/termd_daemon_token"
    if [[ ! -s "$TERMD_RELAY_DAEMON_TOKEN_FILE" ]]; then
      write_service_secret_file "$TERMD_RELAY_DAEMON_TOKEN_FILE" "$(generate_secret_token)"
    fi
  fi

  {
    printf '# 这个文件由安装脚本创建，systemd wrapper 会读取它。\n'
    printf '# 需要 relay、TLS、Web 或自定义监听时，取消注释并修改对应变量。\n'
    printf 'HOME=%q\n' "$SERVICE_HOME"
    printf 'SHELL=%q\n' "$SERVICE_SHELL"
    printf 'TERMD_LISTEN=%q\n' "${TERMD_LISTEN:-127.0.0.1:8765}"
    printf 'TERMD_WEB_ENABLED=%q\n' "${TERMD_WEB_ENABLED:-0}"
    printf 'TERMD_SUPERVISOR_VERSION=%q\n' "${SUPERVISOR_VERSION:-$VERSION}"
    if [[ -n "${TERMD_RELAY_URLS:-}" ]]; then
      printf 'TERMD_RELAY_URLS=%q\n' "$TERMD_RELAY_URLS"
    else
      printf '# TERMD_RELAY_URLS="wss://relay.example:443"\n'
    fi
    if [[ -n "${TERMD_RELAY_DAEMON_TOKEN_FILE:-}" ]]; then
      printf 'TERMD_RELAY_DAEMON_TOKEN_FILE=%q\n' "$TERMD_RELAY_DAEMON_TOKEN_FILE"
    else
      printf '# TERMD_RELAY_DAEMON_TOKEN_FILE=/etc/termd/termd_daemon_token\n'
    fi
    if [[ -n "${HTTP_PROXY:-}" ]]; then
      printf 'HTTP_PROXY=%q\n' "$HTTP_PROXY"
    else
      printf '# HTTP_PROXY=http://127.0.0.1:3128\n'
    fi
    if [[ -n "${HTTPS_PROXY:-}" ]]; then
      printf 'HTTPS_PROXY=%q\n' "$HTTPS_PROXY"
    else
      printf '# HTTPS_PROXY=http://127.0.0.1:3128\n'
    fi
    if [[ -n "${ALL_PROXY:-}" ]]; then
      printf 'ALL_PROXY=%q\n' "$ALL_PROXY"
    else
      printf '# ALL_PROXY=socks5://127.0.0.1:1080\n'
    fi
    if [[ -n "${NO_PROXY:-}" ]]; then
      printf 'NO_PROXY=%q\n' "$NO_PROXY"
    else
      printf '# NO_PROXY=localhost,127.0.0.1\n'
    fi
    if [[ -n "${TERMD_TLS_CERT:-}" ]]; then
      printf 'TERMD_TLS_CERT=%q\n' "$TERMD_TLS_CERT"
    else
      printf '# TERMD_TLS_CERT=/etc/termd/fullchain.pem\n'
    fi
    if [[ -n "${TERMD_TLS_KEY:-}" ]]; then
      printf 'TERMD_TLS_KEY=%q\n' "$TERMD_TLS_KEY"
    else
      printf '# TERMD_TLS_KEY=/etc/termd/privkey.pem\n'
    fi
  } >"$ENV_FILE"
  apply_env_overrides
  chown root:"$SERVICE_GROUP" "$ENV_FILE"
  chmod 0640 "$ENV_FILE"
}

write_wrapper() {
  install -d -m 0755 "$WRAPPER_DIR"
  {
    cat <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

# 这个 wrapper 在 systemd 下组装 termd 的启动参数，便于通过 env 文件配置 relay 和 TLS。

ENV_FILE="/etc/termd/termd.env"

if [[ -r "$ENV_FILE" ]]; then
  # shellcheck source=/dev/null
  set -a
  source "$ENV_FILE"
  set +a
fi

EOF
    printf 'INSTALL_PREFIX=%q\n\n' "$INSTALL_PREFIX"
    cat <<'EOF'
args=()
args+=(--listen "${TERMD_LISTEN:-127.0.0.1:8765}")

if [[ -n "${TERMD_RELAY_DAEMON_TOKEN_FILE:-}" ]]; then
  args+=(--relay-daemon-token-file "$TERMD_RELAY_DAEMON_TOKEN_FILE")
fi

if [[ -n "${TERMD_TLS_CERT:-}" || -n "${TERMD_TLS_KEY:-}" ]]; then
  if [[ -z "${TERMD_TLS_CERT:-}" || -z "${TERMD_TLS_KEY:-}" ]]; then
    printf '[termd-install] TERMD_TLS_CERT 和 TERMD_TLS_KEY 必须成对配置。\n' >&2
    exit 1
  fi
  args+=(--tls-cert "$TERMD_TLS_CERT" --tls-key "$TERMD_TLS_KEY")
fi

if [[ -n "${TERMD_RELAY_URLS:-}" ]]; then
  read -r -a relay_urls <<<"${TERMD_RELAY_URLS}"
  if [[ "${#relay_urls[@]}" -gt 1 ]]; then
    printf '[termd-install] termd 只能连接一个 relay；请在 TERMD_RELAY_URLS 中保留一个地址。\n' >&2
    exit 1
  fi
  for relay_url in "${relay_urls[@]}"; do
    [[ -n "$relay_url" ]] || continue
    args+=(--relay "$relay_url")
  done
fi

is_enabled() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

if is_enabled "${TERMD_WEB_ENABLED:-0}"; then
  args+=(--web)
fi

exec "${INSTALL_PREFIX}/bin/termd" "${args[@]}"
EOF
  } >"$WRAPPER_FILE"
  chmod 0755 "$WRAPPER_FILE"
}

write_unit() {
  cat >"$UNIT_FILE" <<EOF
[Unit]
Description=termd persistent terminal daemon
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_GROUP}
WorkingDirectory=${STATE_DIR}
EnvironmentFile=-${ENV_FILE}
ExecStart=${WRAPPER_FILE}
Restart=always
RestartSec=2
KillMode=process
EOF

  if [[ "$SERVICE_USER" == "termd" ]]; then
    cat >>"$UNIT_FILE" <<EOF
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
StateDirectory=termd
EOF
  else
    cat >>"$UNIT_FILE" <<'EOF'
# 自定义用户用于提供接近 SSH 的个人 shell 体验；这里不额外隐藏 home 或只读化文件系统。
NoNewPrivileges=no
PrivateTmp=no
ProtectHome=no
ProtectSystem=no
EOF
  fi

  cat >>"$UNIT_FILE" <<EOF

[Install]
WantedBy=multi-user.target
EOF
}

local_pairing_base_url() {
  local listen="$1"
  local scheme="$2"

  python3 - "$listen" "$scheme" <<'PY'
import ipaddress
import sys

listen = sys.argv[1]
scheme = sys.argv[2]

try:
    if listen.startswith("["):
        host, port = listen[1:].rsplit("]:", 1)
    else:
        host, port = listen.rsplit(":", 1)
    port_number = int(port)
    if port_number <= 0 or port_number > 65535:
        raise ValueError("invalid port")
except ValueError:
    raise SystemExit(1)

try:
    ip = ipaddress.ip_address(host)
    if ip.is_unspecified:
        host = "::1" if ip.version == 6 else "127.0.0.1"
except ValueError:
    pass

if ":" in host and not (host.startswith("[") and host.endswith("]")):
    host = f"[{host}]"

print(f"{scheme}://{host}:{port_number}")
PY
}

get_local_healthz() {
  local endpoint="$1"

  python3 - "$endpoint" <<'PY'
from urllib.request import urlopen, Request
import sys

endpoint = sys.argv[1]
request = Request(endpoint, method="GET")
try:
    with urlopen(request, timeout=2) as response:
        sys.stdout.buffer.write(response.read())
except Exception:
    raise SystemExit(1)
PY
}

relay_api_url() {
  local relay_url="$1"
  local api_path="$2"

  python3 - "$relay_url" "$api_path" <<'PY'
from urllib.parse import urlsplit, urlunsplit
import sys

raw = sys.argv[1].strip()
api_path = sys.argv[2]
parsed = urlsplit(raw)
if parsed.scheme == "wss":
    scheme = "https"
elif parsed.scheme == "ws":
    scheme = "http"
else:
    raise SystemExit(1)
prefix = parsed.path.rstrip("/")
if prefix.endswith("/ws"):
    prefix = prefix[:-3]
print(urlunsplit((scheme, parsed.netloc, prefix + api_path, "", "")))
PY
}

warn_missing_relay_registration_token() {
  [[ -n "${TERMD_RELAY_URLS:-}" ]] || return 0
  [[ -z "${TERMD_RELAY_SETUP_TOKEN:-}" && -z "${TERMD_RELAY_SETUP_TOKEN_FILE:-}" ]] || return 0

  log "relay is configured, but no relay setup token was provided; trusted relay registration was not attempted"
}

read_relay_setup_token() {
  if [[ -n "${TERMD_RELAY_SETUP_TOKEN:-}" ]]; then
    printf '%s' "$TERMD_RELAY_SETUP_TOKEN"
    return 0
  fi
  [[ -n "${TERMD_RELAY_SETUP_TOKEN_FILE:-}" && -r "$TERMD_RELAY_SETUP_TOKEN_FILE" ]] || return 1
  read_first_line "$TERMD_RELAY_SETUP_TOKEN_FILE"
}

validate_relay_install_mode() {
  if [[ "$INSTALL_ALLOW_OPEN_RELAY" -eq 1 ]]; then
    [[ "$INSTALL_SET_RELAY_URLS" -eq 1 ]] || die "--allow-open-relay requires an explicit --relay <WS_URL>"
    if [[ -n "${TERMD_RELAY_SETUP_TOKEN:-}" || -n "${TERMD_RELAY_SETUP_TOKEN_FILE:-}" ]]; then
      die "--allow-open-relay conflicts with relay setup token options"
    fi
    return 0
  fi
  if [[ "$INSTALL_SET_RELAY_URLS" -eq 1 && -z "${TERMD_RELAY_SETUP_TOKEN:-}" && -z "${TERMD_RELAY_SETUP_TOKEN_FILE:-}" ]]; then
    die "trusted relay setup token is required; use --relay-token or --relay-setup-token-file with the termd install command"
  fi
}

relay_connection_verification_requested() {
  [[ "$INSTALL_SET_RELAY_URLS" -eq 1 || \
    "$INSTALL_SET_RELAY_SETUP_TOKEN" -eq 1 || \
    "$INSTALL_SET_RELAY_SETUP_TOKEN_FILE" -eq 1 || \
    "$INSTALL_ALLOW_OPEN_RELAY" -eq 1 ]]
}

register_daemon_with_relay() (
  local health_response="$1"
  local server_id daemon_public_key relay_url register_url setup_token tmp_dir curl_config payload_file
  tmp_dir=""
  cleanup_relay_registration_files() {
    [[ -z "$tmp_dir" ]] || rm -rf -- "$tmp_dir"
  }
  trap cleanup_relay_registration_files EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  [[ -n "${TERMD_RELAY_URLS:-}" ]] || return 0
  [[ "$INSTALL_ALLOW_OPEN_RELAY" -eq 0 ]] || return 0
  if [[ -z "${TERMD_RELAY_SETUP_TOKEN:-}" && -z "${TERMD_RELAY_SETUP_TOKEN_FILE:-}" ]]; then
    warn_missing_relay_registration_token
    return 0
  fi
  [[ -n "${TERMD_RELAY_DAEMON_TOKEN_FILE:-}" && -r "$TERMD_RELAY_DAEMON_TOKEN_FILE" ]] || {
    log "relay setup token was provided, but daemon token file is missing"
    return 1
  }
  if ! setup_token="$(read_relay_setup_token)" || [[ -z "$setup_token" ]]; then
    log "relay setup token is empty or unreadable"
    return 1
  fi

  # daemon registry 只接收稳定身份材料，不注册或同步 pair/device/access credential。
  server_id="$(printf '%s' "$health_response" | python3 -c 'import json,sys; print(json.load(sys.stdin)["server_id"])')" || return 1
  daemon_public_key="$(printf '%s' "$health_response" | python3 -c 'import json,sys; print(json.load(sys.stdin)["daemon_public_key"])')" || return 1
  read -r -a relay_urls <<<"${TERMD_RELAY_URLS}"
  relay_url="${relay_urls[0]:-}"
  [[ -n "$relay_url" ]] || return 0
  if ! register_url="$(relay_api_url "$relay_url" "/api/relay/daemon/register")"; then
    log "cannot derive relay registration URL from ${relay_url}"
    return 1
  fi

  if ! command -v curl >/dev/null 2>&1; then
    log "curl is required for secret-safe relay registration"
    return 1
  fi

  if ! tmp_dir="$(mktemp -d)"; then
    log "failed to create temporary directory for relay registration"
    return 1
  fi
  if ! chmod 0700 "$tmp_dir"; then
    log "failed to secure temporary directory for relay registration"
    return 1
  fi
  curl_config="${tmp_dir}/curl.conf"
  payload_file="${tmp_dir}/register.json"
  if ! python3 - "$server_id" "$daemon_public_key" "$TERMD_RELAY_DAEMON_TOKEN_FILE" >"$payload_file" <<'PY'
import json
import sys

server_id = sys.argv[1]
daemon_public_key = sys.argv[2]
with open(sys.argv[3], "r", encoding="utf-8") as token_file:
    daemon_token = token_file.readline().strip()
print(json.dumps({"server_id": server_id, "daemon_token": daemon_token, "daemon_public_key": daemon_public_key}, separators=(",", ":")))
PY
  then
    log "failed to prepare relay registration payload"
    return 1
  fi
  if ! chmod 0600 "$payload_file"; then
    log "failed to secure relay registration payload"
    return 1
  fi
  if ! {
    printf 'url = "%s"\n' "$register_url"
    printf 'request = "POST"\n'
    printf 'fail\n'
    printf 'silent\n'
    printf 'show-error\n'
    printf 'header = "content-type: application/json"\n'
    printf 'header = "x-termd-relay-setup-token: %s"\n' "$setup_token"
    printf 'data-binary = "@%s"\n' "$payload_file"
  } >"$curl_config"; then
    log "failed to prepare relay registration request"
    return 1
  fi
  if ! chmod 0600 "$curl_config"; then
    log "failed to secure relay registration request"
    return 1
  fi

  if ! curl --disable --config "$curl_config" >/dev/null; then
    log "relay registration failed at ${register_url}"
    return 1
  fi
  log "registered local daemon ${server_id} with relay ${relay_url}"
)

verify_daemon_relay_connected() (
  local health_response="$1"
  local server_id relay_url status_url setup_token tmp_dir curl_config payload_file response connected
  tmp_dir=""
  cleanup_relay_status_files() {
    [[ -z "$tmp_dir" ]] || rm -rf -- "$tmp_dir"
  }
  trap cleanup_relay_status_files EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  [[ -n "${TERMD_RELAY_URLS:-}" ]] || return 0
  server_id="$(printf '%s' "$health_response" | python3 -c 'import json,sys; print(json.load(sys.stdin)["server_id"])')" || return 1
  read -r -a relay_urls <<<"${TERMD_RELAY_URLS}"
  relay_url="${relay_urls[0]:-}"
  [[ -n "$relay_url" ]] || return 0
  if ! status_url="$(relay_api_url "$relay_url" "/api/relay/daemon/status")"; then
    log "FAILED: cannot derive relay status URL from ${relay_url}"
    return 1
  fi
  setup_token=""
  if [[ "$INSTALL_ALLOW_OPEN_RELAY" -eq 0 ]]; then
    if ! setup_token="$(read_relay_setup_token)" || [[ -z "$setup_token" ]]; then
      log "FAILED: relay setup token is empty or unreadable"
      return 1
    fi
  fi
  if ! command -v curl >/dev/null 2>&1; then
    log "FAILED: curl is required to verify the relay connection"
    return 1
  fi

  if ! tmp_dir="$(mktemp -d)"; then
    log "FAILED: failed to create temporary directory for relay verification"
    return 1
  fi
  if ! chmod 0700 "$tmp_dir"; then
    log "FAILED: failed to secure temporary directory for relay verification"
    return 1
  fi
  curl_config="${tmp_dir}/curl.conf"
  payload_file="${tmp_dir}/status.json"
  if ! printf '{"server_id":"%s"}\n' "$server_id" >"$payload_file"; then
    log "FAILED: failed to prepare relay verification payload"
    return 1
  fi
  if ! chmod 0600 "$payload_file"; then
    log "FAILED: failed to secure relay verification payload"
    return 1
  fi
  if ! {
    printf 'url = "%s"\n' "$status_url"
    printf 'request = "POST"\n'
    printf 'fail\n'
    printf 'silent\n'
    printf 'show-error\n'
    printf 'header = "content-type: application/json"\n'
    if [[ -n "$setup_token" ]]; then
      printf 'header = "x-termd-relay-setup-token: %s"\n' "$setup_token"
    fi
    printf 'data-binary = "@%s"\n' "$payload_file"
  } >"$curl_config"; then
    log "FAILED: failed to prepare relay verification request"
    return 1
  fi
  if ! chmod 0600 "$curl_config"; then
    log "FAILED: failed to secure relay verification request"
    return 1
  fi

  for _ in {1..40}; do
    if response="$(curl --disable --config "$curl_config" 2>/dev/null)" && \
      connected="$(printf '%s' "$response" | python3 -c 'import json,sys; payload=json.load(sys.stdin); print("true" if payload.get("server_id") == sys.argv[1] and payload.get("connected") is True else "false")' "$server_id")" && \
      [[ "$connected" == "true" ]]; then
      log "SUCCESS: daemon ${server_id} is connected to relay ${relay_url}"
      return 0
    fi
    sleep 0.25
  done

  log "FAILED: daemon ${server_id} did not establish its relay control connection to ${relay_url}"
  return 1
)

print_relay_install_retry() {
  local relay_url retry_command quoted_arg arg
  local -a relay_urls=()
  read -r -a relay_urls <<<"${TERMD_RELAY_URLS:-}"
  relay_url="${relay_urls[0]:-}"
  [[ -n "$relay_url" ]] || return 0

  retry_command="sudo ${INSTALL_PREFIX}/bin/${BIN_NAME} install"
  for arg in "$@"; do
    printf -v quoted_arg '%q' "$arg"
    retry_command+=" ${quoted_arg}"
  done
  printf -v quoted_arg '%q' "$relay_url"
  retry_command+=" --relay ${quoted_arg}"
  if [[ "$INSTALL_ALLOW_OPEN_RELAY" -eq 1 ]]; then
    retry_command+=" --allow-open-relay"
  else
    log "the trusted relay setup token will be requested again"
  fi
  log "retry with: ${retry_command}"
}

print_initial_pairing_token() {
  local listen scheme base_url healthz_endpoint response

  listen="${TERMD_LISTEN:-127.0.0.1:8765}"
  scheme="http"
  if [[ -n "${TERMD_TLS_CERT:-}" || -n "${TERMD_TLS_KEY:-}" ]]; then
    scheme="https"
  fi

  if ! base_url="$(local_pairing_base_url "$listen" "$scheme")"; then
    log "cannot derive local pairing URL from TERMD_LISTEN=${listen}"
    if [[ -n "${TERMD_RELAY_URLS:-}" ]] && relay_connection_verification_requested; then
      print_relay_install_retry --listen 127.0.0.1:8765
    else
      log "retry with: sudo ${INSTALL_PREFIX}/bin/${BIN_NAME} install --listen 127.0.0.1:8765"
    fi
    return 1
  fi

  healthz_endpoint="${base_url}/healthz"
  for _ in {1..40}; do
    if response="$(get_local_healthz "$healthz_endpoint" 2>/dev/null)"; then
      if [[ -n "${TERMD_RELAY_URLS:-}" ]] && ! relay_connection_verification_requested; then
        log "SKIPPED: existing relay configuration preserved; setup token was not provided for connection verification"
      else
        if ! register_daemon_with_relay "$response"; then
          log "FAILED: trusted relay registration failed"
          print_relay_install_retry
          return 1
        fi
        if ! verify_daemon_relay_connected "$response"; then
          print_relay_install_retry
          return 1
        fi
      fi
      TERMD_RELAY_SETUP_TOKEN=""
      log "initial pairing QR and invite (sensitive, expires shortly):"
      if "${INSTALL_PREFIX}/bin/${BIN_NAME}" pair --qr --url "$base_url"; then
        return 0
      fi
      log "initial pairing failed; run '${INSTALL_PREFIX}/bin/${BIN_NAME} pair --qr --url ${base_url}' manually"
      return 1
    fi
    sleep 0.25
  done

  log "service started, but local health check failed at ${healthz_endpoint}"
  if [[ -n "${TERMD_RELAY_URLS:-}" ]] && relay_connection_verification_requested; then
    print_relay_install_retry
  else
    log "retry with: sudo systemctl restart ${SERVICE_NAME}"
    log "then run: ${INSTALL_PREFIX}/bin/${BIN_NAME} pair --qr --url ${base_url}"
  fi
  return 1
}

complete_post_install() {
  if print_initial_pairing_token; then
    return 0
  fi
  log "local service installed but post-install verification/pairing failed"
  return 1
}

ensure_system_user() {
  if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin --user-group "$SERVICE_USER"
    SERVICE_GROUP="$SERVICE_USER"
  fi
  install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0750 "$STATE_DIR"
  chown_state_dir
}

chown_state_dir() {
  # 切换 --user 时服务进程也必须能读写同一套 daemon identity、SQLite 和 supervisor socket 目录。
  chown -R "$SERVICE_USER:$SERVICE_GROUP" "$STATE_DIR"
  chmod 0750 "$STATE_DIR"
}

clear_session_state_after_state_dir_change() {
  if [[ -z "$PREVIOUS_STATE_DIR" || "$PREVIOUS_STATE_DIR" == "$STATE_DIR" ]]; then
    return 0
  fi

  log "using fixed state directory ${STATE_DIR}; previous WorkingDirectory was ${PREVIOUS_STATE_DIR}"
  log "clearing stale session metadata in ${STATE_DIR}; pairing identity and trusted devices are preserved"
  clear_runtime_session_state "${STATE_DIR}/daemon-state.sqlite" "${STATE_DIR}/termd-supervisors"
}

uninstall_component() {
  require_cmd systemctl

  log "stopping and disabling ${SERVICE_NAME}.service if present"
  systemctl stop "$SERVICE_NAME" 2>/dev/null || true
  systemctl disable "$SERVICE_NAME" 2>/dev/null || true

  # 默认只删除程序与 systemd 配置，不删除 daemon identity、可信设备和 session 状态。
  rm -f "$UNIT_FILE"
  rm -f "$WRAPPER_FILE"
  rmdir "$WRAPPER_DIR" 2>/dev/null || true
  rm -f "${INSTALL_PREFIX}/bin/${BIN_NAME}"
  rm -f "$ENV_FILE"
  rmdir "$ENV_DIR" 2>/dev/null || true

  systemctl daemon-reload
  systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true

  if [[ "$PURGE_STATE" -eq 1 ]]; then
    log "purging ${STATE_DIR} and managed system user ${SERVICE_NAME}"
    rm -rf "$STATE_DIR"
    if [[ "$SERVICE_USER" == "$SERVICE_NAME" ]] && id -u "$SERVICE_NAME" >/dev/null 2>&1; then
      userdel "$SERVICE_NAME" 2>/dev/null || true
    fi
    if [[ "$SERVICE_USER" == "$SERVICE_NAME" ]] && getent group "$SERVICE_NAME" >/dev/null 2>&1; then
      groupdel "$SERVICE_NAME" 2>/dev/null || true
    fi
  else
    log "preserved ${STATE_DIR}; rerun with --uninstall --purge to remove local daemon state"
  fi

  log "uninstalled ${BIN_NAME}"
}

main() {
  parse_args "$@"
  if [[ "$ACTION" != "install" && "$INSTALL_ALLOW_SESSION_LOSS" -eq 1 ]]; then
    die "--allow-session-loss is valid only for installation"
  fi
  validate_internal_install_request
  apply_internal_install_arguments
  validate_relay_install_mode
  normalize_proxy_environment
  require_root
  inherit_existing_service_identity
  if [[ "$ACTION" == "uninstall" ]]; then
    resolve_service_identity
    uninstall_component
    return 0
  fi

  require_cmd install
  require_cmd python3
  require_cmd systemctl
  require_cmd useradd
  if [[ "$SELF_INSTALL_ENABLED" -eq 0 ]]; then
    require_cmd tar
    require_cmd sha256sum
    [[ -n "$REPO" ]] || die "set TERMD_GITHUB_REPO=owner/repo before running the installer"
    resolve_version
  fi
  resolve_service_identity
  resolve_supervisor_version
  assert_session_ownership_quiescent
  log "installing ${BIN_NAME} ${VERSION}"

  INSTALL_STAGING_DIR="$(mktemp -d)"
  trap cleanup_install_staging EXIT
  local candidate_binary="${INSTALL_STAGING_DIR}/${BIN_NAME}"
  if [[ "$SELF_INSTALL_ENABLED" -eq 1 ]]; then
    install_from_self_binary "$candidate_binary" || die "failed to stage embedded self-install binary"
  elif ! install_from_release "$candidate_binary"; then
    install_from_source "$candidate_binary"
  fi
  if ! install_staged_candidate "$candidate_binary"; then
    die "installation failed; the installer attempted to restore the previous binary, configuration, and service state"
  fi
  cleanup_install_staging
  trap - EXIT

  log "installed ${BIN_NAME} ${VERSION} and started ${SERVICE_NAME}.service"
  if ! complete_post_install; then
    return 1
  fi
}

main "$@"
