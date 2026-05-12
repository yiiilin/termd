#!/usr/bin/env bash

set -euo pipefail

# 这个脚本安装 termd 二进制并注册 systemd 服务。
# 服务默认只监听 loopback，relay 和 TLS 通过 /etc/termd/termd.env 进行可选配置。

COMPONENT="termd"
BIN_NAME="termd"
SERVICE_NAME="termd"
INSTALL_PREFIX="${TERMD_INSTALL_PREFIX:-/usr/local}"
REPO="${TERMD_GITHUB_REPO:-${GITHUB_REPOSITORY:-}}"
VERSION="${TERMD_VERSION:-}"
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
INSTALL_SET_RELAY_AUTH_TOKEN=0
INSTALL_SET_TLS_CERT=0
INSTALL_SET_TLS_KEY=0
INSTALL_SET_USER=0
ACTION="install"
PURGE_STATE=0
LOG_EMITTED=0

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
  --relay-auth-token <TOKEN>    Set relay transport auth token.
  --tls-cert <PATH>             Set TLS certificate path.
  --tls-key <PATH>              Set TLS private key path.
  --user <USER>                 Run termd.service as this Linux user; default: existing service user, then termd.
  --uninstall                   Stop service and remove termd program files.
  --purge                       Implies --uninstall; also remove /var/lib/termd and system user.
  -h, --help                    Print this help.

Examples:
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --user alice
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765
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
      --relay-auth-token)
        [[ $# -ge 2 && -n "$2" ]] || die "--relay-auth-token requires a non-empty value"
        TERMD_RELAY_AUTH_TOKEN="$2"
        INSTALL_SET_RELAY_AUTH_TOKEN=1
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
  local arch archive_name archive_url checksums_url tmp_dir archive_path checksums_path

  arch="$(detect_arch)"
  [[ -n "$arch" ]] || return 1

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
  install -Dm0755 "$tmp_dir/$BIN_NAME" "${INSTALL_PREFIX}/bin/${BIN_NAME}"
  rm -rf "$tmp_dir"
  return 0
}

install_from_source() {
  require_cmd cargo
  require_cmd git

  local src_dir
  src_dir="$(mktemp -d)"

  log "falling back to source build from ${REPO}@${VERSION}"
  git clone --depth 1 --branch "$VERSION" "https://github.com/${REPO}.git" "$src_dir/repo"
  (
    cd "$src_dir/repo"
    cargo build --release --locked -p "$COMPONENT" --bin "$BIN_NAME"
  )
  install -Dm0755 "$src_dir/repo/target/release/$BIN_NAME" "${INSTALL_PREFIX}/bin/${BIN_NAME}"
  rm -rf "$src_dir"
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

apply_env_overrides() {
  # 命令行参数只覆盖用户显式传入的项，避免重装时意外抹掉已有 systemd 配置。
  if [[ "$INSTALL_SET_LISTEN" -eq 1 ]]; then
    upsert_env_var "TERMD_LISTEN" "$TERMD_LISTEN"
  fi
  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    upsert_env_var "TERMD_WEB_ENABLED" "$TERMD_WEB_ENABLED"
  fi
  if [[ "$INSTALL_SET_RELAY_URLS" -eq 1 ]]; then
    upsert_env_var "TERMD_RELAY_URLS" "$TERMD_RELAY_URLS"
  fi
  if [[ "$INSTALL_SET_RELAY_AUTH_TOKEN" -eq 1 ]]; then
    upsert_env_var "TERMD_RELAY_AUTH_TOKEN" "$TERMD_RELAY_AUTH_TOKEN"
  fi
  if [[ "$INSTALL_SET_TLS_CERT" -eq 1 ]]; then
    upsert_env_var "TERMD_TLS_CERT" "$TERMD_TLS_CERT"
  fi
  if [[ "$INSTALL_SET_TLS_KEY" -eq 1 ]]; then
    upsert_env_var "TERMD_TLS_KEY" "$TERMD_TLS_KEY"
  fi
}

apply_service_env_defaults() {
  # HOME/SHELL 由 daemon 的 systemd 运行身份决定，始终写进 env 文件，避免 unit 和 env 文件各自维护。
  upsert_env_var "HOME" "$SERVICE_HOME"
  upsert_env_var "SHELL" "$SERVICE_SHELL"
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
  STATE_DIR="/var/lib/termd"
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

  {
    printf '# 这个文件由安装脚本创建，systemd wrapper 会读取它。\n'
    printf '# 需要 relay、TLS、Web 或自定义监听时，取消注释并修改对应变量。\n'
    printf 'HOME=%q\n' "$SERVICE_HOME"
    printf 'SHELL=%q\n' "$SERVICE_SHELL"
    printf 'TERMD_LISTEN=%q\n' "${TERMD_LISTEN:-127.0.0.1:8765}"
    printf 'TERMD_WEB_ENABLED=%q\n' "${TERMD_WEB_ENABLED:-0}"
    if [[ -n "${TERMD_RELAY_URLS:-}" ]]; then
      printf 'TERMD_RELAY_URLS=%q\n' "$TERMD_RELAY_URLS"
    else
      printf '# TERMD_RELAY_URLS="wss://relay.example:443"\n'
    fi
    if [[ -n "${TERMD_RELAY_AUTH_TOKEN:-}" ]]; then
      printf 'TERMD_RELAY_AUTH_TOKEN=%q\n' "$TERMD_RELAY_AUTH_TOKEN"
    else
      printf '# TERMD_RELAY_AUTH_TOKEN=replace-me\n'
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
  cat >"$WRAPPER_FILE" <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

# 这个 wrapper 在 systemd 下组装 termd 的启动参数，便于通过 env 文件配置 relay 和 TLS。

ENV_FILE="/etc/termd/termd.env"

if [[ -r "$ENV_FILE" ]]; then
  # shellcheck source=/dev/null
  source "$ENV_FILE"
fi

args=()
args+=(--listen "${TERMD_LISTEN:-127.0.0.1:8765}")

if [[ -n "${TERMD_RELAY_AUTH_TOKEN:-}" ]]; then
  args+=(--relay-auth-token "$TERMD_RELAY_AUTH_TOKEN")
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

exec /usr/local/bin/termd "${args[@]}"
EOF
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

post_local_pairing_token() {
  local endpoint="$1"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSk -X POST "$endpoint"
  elif command -v wget >/dev/null 2>&1; then
    wget --no-check-certificate -qO- --method=POST "$endpoint"
  else
    return 1
  fi
}

print_initial_pairing_token() {
  local listen scheme base_url endpoint response summary

  listen="${TERMD_LISTEN:-127.0.0.1:8765}"
  scheme="http"
  if [[ -n "${TERMD_TLS_CERT:-}" || -n "${TERMD_TLS_KEY:-}" ]]; then
    scheme="https"
  fi

  if ! base_url="$(local_pairing_base_url "$listen" "$scheme")"; then
    log "cannot derive local pairing URL from TERMD_LISTEN=${listen}; run '${INSTALL_PREFIX}/bin/${BIN_NAME} pair' manually"
    return 0
  fi

  endpoint="${base_url}/local/pairing-token"
  for _ in {1..40}; do
    if response="$(post_local_pairing_token "$endpoint" 2>/dev/null)"; then
      if summary="$(printf '%s' "$response" | PAIRING_BASE_URL="$base_url" python3 -c '
import base64
import json
import os
import shlex
import sys

payload = json.load(sys.stdin)
base_url = os.environ["PAIRING_BASE_URL"]
token = payload["token"]
ttl_ms = int(payload.get("ttl_ms", 0))
server_id = payload.get("server_id", "")
expires_at_ms = int(payload.get("expires_at_ms", 0))
direct_ws_url = base_url.replace("https://", "wss://", 1).replace("http://", "ws://", 1) + "/ws"

def invite_code():
    # 邀请码只是单行 URL-safe 包装，不是长期密钥；真正认证仍由 daemon 的 pairing/auth 完成。
    invite_payload = {
        "type": "termd_pairing_qr",
        "version": 1,
        "token": token,
        "server_id": server_id,
        "expires_at_ms": expires_at_ms,
    }
    raw = json.dumps(invite_payload, separators=(",", ":")).encode("utf-8")
    return "termd-pair:v1:" + base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")

web_invite = invite_code()
print(f"[termd-install] initial pairing invite, expires in {ttl_ms // 1000}s:")
print("[termd-install] raw token:")
print(token)
print("[termd-install] pair with:")
print(f"termctl pair --payload {shlex.quote(web_invite)} --url {shlex.quote(direct_ws_url)}")
print("[termd-install] web invite code:")
print(web_invite)
print("[termd-install] open the Web page you plan to use and paste or scan this invite code.")
')"; then
        printf '\n%s\n' "$summary"
        return 0
      fi
    fi
    sleep 0.25
  done

  log "service started, but initial pairing invite could not be issued from ${endpoint}"
  log "run '${INSTALL_PREFIX}/bin/${BIN_NAME} pair --url ${base_url}' on this host to issue a new token"
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

  local sqlite_path supervisor_dir
  sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  supervisor_dir="${STATE_DIR}/termd-supervisors"

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
    find "$supervisor_dir" -maxdepth 1 -type s -delete
  fi
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
  require_root
  inherit_existing_service_identity
  if [[ "$ACTION" == "uninstall" ]]; then
    resolve_service_identity
    uninstall_component
    return 0
  fi

  require_cmd install
  require_cmd tar
  require_cmd sha256sum
  require_cmd python3
  require_cmd systemctl
  require_cmd useradd
  [[ -n "$REPO" ]] || die "set TERMD_GITHUB_REPO=owner/repo before running the installer"

  resolve_version
  resolve_service_identity
  log "installing ${BIN_NAME} ${VERSION}"

  if ! install_from_release; then
    install_from_source
  fi

  ensure_system_user
  clear_session_state_after_state_dir_change
  chown_state_dir
  write_env_file
  # 重新读取最终 env，保证后续 wrapper 和初始 pairing token 都使用同一组监听/TLS 配置。
  # shellcheck source=/dev/null
  source "$ENV_FILE"
  write_wrapper
  write_unit

  systemctl daemon-reload
  systemctl enable "$SERVICE_NAME"
  systemctl restart "$SERVICE_NAME"

  log "installed ${BIN_NAME} ${VERSION} and started ${SERVICE_NAME}.service"
  print_initial_pairing_token
}

main "$@"
