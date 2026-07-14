#!/usr/bin/env bash

set -euo pipefail

# 这个脚本安装 termrelay 二进制并注册 systemd 服务。
# 默认只监听 loopback；如果需要公网 TLS，建议使用反向代理或 docker-compose 模式。

COMPONENT="termrelay"
BIN_NAME="termrelay"
SERVICE_NAME="termrelay"
INSTALL_PREFIX="${TERMD_INSTALL_PREFIX:-/usr/local}"
REPO="${TERMD_GITHUB_REPO:-${GITHUB_REPOSITORY:-}}"
VERSION="${TERMD_VERSION:-}"
ENV_DIR="/etc/termd"
ENV_FILE="${ENV_DIR}/termrelay.env"
WRAPPER_DIR="/usr/local/lib/termrelay"
WRAPPER_FILE="${WRAPPER_DIR}/termrelay-run"
UNIT_FILE="/etc/systemd/system/termrelay.service"
STATE_DIR="/var/lib/termrelay"
INSTALL_SET_LISTEN=0
INSTALL_SET_WEB=0
INSTALL_SET_SETUP_TOKEN_FILE=0
INSTALL_SET_DAEMON_REGISTRY=0
INSTALL_SET_ALLOW_OPEN_RELAY=0
INSTALL_SET_TLS_CERT=0
INSTALL_SET_TLS_KEY=0
ACTION="install"
PURGE_STATE=0
LOG_EMITTED=0
INSTALL_STAGING_DIR=""
INSTALL_ROLLBACK_DIR=""
INSTALL_STAGING_ONLY=0
INSTALL_ANY_FILE_COMMITTED=0
INSTALL_SERVICE_WAS_ACTIVE=0
INSTALL_SERVICE_WAS_ENABLED=0

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

is_enabled() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

web_ui_requested() {
  local requested

  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    is_enabled "${TERMRELAY_WEB_ENABLED:-0}"
    return
  fi

  if [[ -r "$ENV_FILE" ]]; then
    if ! requested="$(
      # shellcheck source=/dev/null
      source "$ENV_FILE" || exit 1
      printf '%s' "${TERMRELAY_WEB_ENABLED:-0}"
    )"; then
      die "failed to read existing env file at ${ENV_FILE}; installed binary was not changed"
    fi
    is_enabled "$requested"
    return
  fi

  is_enabled "${TERMRELAY_WEB_ENABLED:-0}"
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
usage: install-termrelay.sh [OPTIONS]

Install termrelay and register termrelay.service.

Options:
  --web                       Enable embedded Web UI in systemd.
  --no-web                    Disable embedded Web UI in systemd.
  --listen <HOST:PORT>        Set TERMRELAY_LISTEN, for example 0.0.0.0:8080.
  --public                    Alias for --listen 0.0.0.0:8080.
  --setup-token-file <PATH>   Read relay daemon registration setup token from a file.
  --daemon-registry <PATH>    Set trusted daemon registry JSON path.
  --allow-open-relay          Explicitly allow legacy/open relay mode without daemon registry.
  --tls-cert <PATH>           Set TLS certificate path.
  --tls-key <PATH>            Set TLS private key path.
  --uninstall                 Stop service and remove termrelay program files.
  --purge                     Implies --uninstall; also remove /var/lib/termrelay and system user.
  -h, --help                  Print this help.

Installer network access honors http_proxy, https_proxy, all_proxy and no_proxy,
plus their uppercase variants. Lowercase values take precedence when both are set.

Examples:
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --web
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      -h|--help)
        print_usage
        exit 0
        ;;
      --web)
        TERMRELAY_WEB_ENABLED=1
        INSTALL_SET_WEB=1
        shift
        ;;
      --no-web)
        TERMRELAY_WEB_ENABLED=0
        INSTALL_SET_WEB=1
        shift
        ;;
      --listen)
        [[ $# -ge 2 && -n "$2" ]] || die "--listen requires a value"
        TERMRELAY_LISTEN="$2"
        INSTALL_SET_LISTEN=1
        shift 2
        ;;
      --public)
        TERMRELAY_LISTEN="0.0.0.0:8080"
        INSTALL_SET_LISTEN=1
        shift
        ;;
      --setup-token-file)
        [[ $# -ge 2 && -n "$2" ]] || die "--setup-token-file requires a non-empty value"
        TERMRELAY_SETUP_TOKEN_FILE="$2"
        INSTALL_SET_SETUP_TOKEN_FILE=1
        shift 2
        ;;
      --daemon-registry)
        [[ $# -ge 2 && -n "$2" ]] || die "--daemon-registry requires a non-empty value"
        TERMRELAY_DAEMON_REGISTRY="$2"
        INSTALL_SET_DAEMON_REGISTRY=1
        shift 2
        ;;
      --allow-open-relay)
        TERMRELAY_ALLOW_OPEN_RELAY=1
        INSTALL_SET_ALLOW_OPEN_RELAY=1
        shift
        ;;
      --tls-cert)
        [[ $# -ge 2 && -n "$2" ]] || die "--tls-cert requires a non-empty value"
        TERMRELAY_TLS_CERT="$2"
        INSTALL_SET_TLS_CERT=1
        shift 2
        ;;
      --tls-key)
        [[ $# -ge 2 && -n "$2" ]] || die "--tls-key requires a non-empty value"
        TERMRELAY_TLS_KEY="$2"
        INSTALL_SET_TLS_KEY=1
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
  local destination="${1:-${INSTALL_PREFIX}/bin/${BIN_NAME}}"
  require_cmd cargo
  require_cmd git

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
  chown root:"$SERVICE_NAME" "$path"
  chmod 0640 "$path"
}

generate_secret_token() {
  python3 - <<'PY'
import secrets

print(secrets.token_urlsafe(32))
PY
}

ensure_default_registry_file() {
  local registry_path="${TERMRELAY_DAEMON_REGISTRY:-${STATE_DIR}/daemon-registry.json}"

  if [[ "$INSTALL_STAGING_ONLY" -eq 1 ]]; then
    TERMRELAY_DAEMON_REGISTRY="$registry_path"
    return 0
  fi

  install -d -m 0750 -o "$SERVICE_NAME" -g "$SERVICE_NAME" "$(dirname "$registry_path")"
  if [[ ! -e "$registry_path" ]]; then
    printf '{"daemons":[]}\n' >"$registry_path"
  fi
  chown "$SERVICE_NAME:$SERVICE_NAME" "$registry_path"
  chmod 0640 "$registry_path"
  TERMRELAY_DAEMON_REGISTRY="$registry_path"
}

ensure_setup_token_file() {
  local token_file="${TERMRELAY_SETUP_TOKEN_FILE:-/etc/termd/termrelay_setup_token}"

  if [[ "$INSTALL_STAGING_ONLY" -eq 1 ]]; then
    TERMRELAY_SETUP_TOKEN_FILE="$token_file"
    return 0
  fi

  install -d -m 0755 "$ENV_DIR"
  if [[ ! -s "$token_file" ]]; then
    write_service_secret_file "$token_file" "$(generate_secret_token)"
    log "created relay setup token at ${token_file}"
    log "use this token file value when registering termd with this relay"
  fi
  TERMRELAY_SETUP_TOKEN_FILE="$token_file"
}

trusted_registry_defaults_enabled() {
  [[ "${TERMRELAY_ALLOW_OPEN_RELAY:-0}" != "1" || "$INSTALL_SET_DAEMON_REGISTRY" -eq 1 || "$INSTALL_SET_SETUP_TOKEN_FILE" -eq 1 ]]
}

apply_env_overrides() {
  # 命令行参数只覆盖用户显式传入的项，避免重装时意外抹掉已有 systemd 配置。
  if [[ "$INSTALL_SET_LISTEN" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_LISTEN" "$TERMRELAY_LISTEN"
  fi
  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_WEB_ENABLED" "$TERMRELAY_WEB_ENABLED"
  fi
  if [[ "$INSTALL_SET_SETUP_TOKEN_FILE" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_SETUP_TOKEN_FILE" "$TERMRELAY_SETUP_TOKEN_FILE"
  fi
  if [[ "$INSTALL_SET_DAEMON_REGISTRY" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_DAEMON_REGISTRY" "$TERMRELAY_DAEMON_REGISTRY"
    unset_env_var "TERMRELAY_ALLOW_OPEN_RELAY"
  fi
  if [[ "$INSTALL_SET_ALLOW_OPEN_RELAY" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_ALLOW_OPEN_RELAY" "$TERMRELAY_ALLOW_OPEN_RELAY"
    unset_env_var "TERMRELAY_DAEMON_REGISTRY"
    unset_env_var "TERMRELAY_SETUP_TOKEN_FILE"
  fi
  if [[ "$INSTALL_SET_TLS_CERT" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_TLS_CERT" "$TERMRELAY_TLS_CERT"
  fi
  if [[ "$INSTALL_SET_TLS_KEY" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_TLS_KEY" "$TERMRELAY_TLS_KEY"
  fi
}

write_env_file() {
  # systemd 服务会以 termrelay 用户运行 wrapper；env 文件需要允许 termrelay 组读取。
  install -d -m 0755 "$(dirname "$ENV_FILE")"
  if trusted_registry_defaults_enabled; then
    ensure_default_registry_file
    ensure_setup_token_file
  fi

  if [[ -e "$ENV_FILE" ]]; then
    log "keeping existing env file at ${ENV_FILE}"
    apply_env_overrides
    unset_env_var "TERMRELAY_AUTH_TOKEN"
    unset_env_var "TERMRELAY_AUTH_TOKEN_FILE"
    unset_env_var "TERMRELAY_HTTP_TUNNEL"
    if trusted_registry_defaults_enabled; then
      upsert_env_var "TERMRELAY_DAEMON_REGISTRY" "$TERMRELAY_DAEMON_REGISTRY"
      upsert_env_var "TERMRELAY_SETUP_TOKEN_FILE" "$TERMRELAY_SETUP_TOKEN_FILE"
    fi
    chown root:"$SERVICE_NAME" "$ENV_FILE"
    chmod 0640 "$ENV_FILE"
    return 0
  fi

  {
    printf '# 这个文件由安装脚本创建，systemd wrapper 会读取它。\n'
    printf '# 需要自定义监听、TLS 或 Web 时，取消注释并修改对应变量。\n'
    printf 'TERMRELAY_LISTEN=%q\n' "${TERMRELAY_LISTEN:-127.0.0.1:8080}"
    printf 'TERMRELAY_WEB_ENABLED=%q\n' "${TERMRELAY_WEB_ENABLED:-0}"
    if trusted_registry_defaults_enabled; then
      printf 'TERMRELAY_SETUP_TOKEN_FILE=%q\n' "$TERMRELAY_SETUP_TOKEN_FILE"
      printf 'TERMRELAY_DAEMON_REGISTRY=%q\n' "$TERMRELAY_DAEMON_REGISTRY"
    else
      printf '# TERMRELAY_SETUP_TOKEN_FILE=/etc/termd/termrelay_setup_token\n'
      printf '# TERMRELAY_DAEMON_REGISTRY=/var/lib/termrelay/daemon-registry.json\n'
    fi
    if [[ -n "${TERMRELAY_ALLOW_OPEN_RELAY:-}" ]]; then
      printf 'TERMRELAY_ALLOW_OPEN_RELAY=%q\n' "$TERMRELAY_ALLOW_OPEN_RELAY"
    else
      printf '# TERMRELAY_ALLOW_OPEN_RELAY=1\n'
    fi
    if [[ -n "${TERMRELAY_TLS_CERT:-}" ]]; then
      printf 'TERMRELAY_TLS_CERT=%q\n' "$TERMRELAY_TLS_CERT"
    else
      printf '# TERMRELAY_TLS_CERT=/etc/termd/fullchain.pem\n'
    fi
    if [[ -n "${TERMRELAY_TLS_KEY:-}" ]]; then
      printf 'TERMRELAY_TLS_KEY=%q\n' "$TERMRELAY_TLS_KEY"
    else
      printf '# TERMRELAY_TLS_KEY=/etc/termd/privkey.pem\n'
    fi
  } >"$ENV_FILE"
  chown root:"$SERVICE_NAME" "$ENV_FILE"
  chmod 0640 "$ENV_FILE"
}

write_wrapper() {
  local destination="${1:-$WRAPPER_FILE}"
  local runtime_env_file="${2:-$ENV_FILE}"

  install -d -m 0755 "$(dirname "$destination")"
  {
    cat <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

# 这个 wrapper 在 systemd 下组装 termrelay 的启动参数，便于通过 env 文件配置监听、TLS 和 registry。

EOF
    printf 'ENV_FILE=%q\n\n' "$runtime_env_file"
    cat <<'EOF'
if [[ -r "$ENV_FILE" ]]; then
  # shellcheck source=/dev/null
  source "$ENV_FILE"
fi

EOF
    printf 'INSTALL_PREFIX=%q\n\n' "$INSTALL_PREFIX"
    cat <<'EOF'
args=(--listen "${TERMRELAY_LISTEN:-127.0.0.1:8080}")

if [[ -n "${TERMRELAY_DAEMON_REGISTRY:-}" ]]; then
  args+=(--daemon-registry "$TERMRELAY_DAEMON_REGISTRY")
elif [[ "${TERMRELAY_ALLOW_OPEN_RELAY:-0}" == "1" ]]; then
  args+=(--allow-open-relay)
fi

if [[ -n "${TERMRELAY_SETUP_TOKEN_FILE:-}" ]]; then
  args+=(--setup-token-file "$TERMRELAY_SETUP_TOKEN_FILE")
fi

if [[ -n "${TERMRELAY_TLS_CERT:-}" || -n "${TERMRELAY_TLS_KEY:-}" ]]; then
  if [[ -z "${TERMRELAY_TLS_CERT:-}" || -z "${TERMRELAY_TLS_KEY:-}" ]]; then
    printf '[termrelay-install] TERMRELAY_TLS_CERT 和 TERMRELAY_TLS_KEY 必须成对配置。\n' >&2
    exit 1
  fi
  args+=(--tls-cert "$TERMRELAY_TLS_CERT" --tls-key "$TERMRELAY_TLS_KEY")
fi

is_enabled() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

if is_enabled "${TERMRELAY_WEB_ENABLED:-0}"; then
  args+=(--web)
fi

exec "${INSTALL_PREFIX}/bin/termrelay" "${args[@]}"
EOF
  } >"$destination"
  chmod 0755 "$destination"
}

write_unit() {
  local destination="${1:-$UNIT_FILE}"
  local runtime_wrapper="${2:-$WRAPPER_FILE}"

  install -d -m 0755 "$(dirname "$destination")"
  cat >"$destination" <<EOF
[Unit]
Description=termrelay dumb pipe
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=termrelay
Group=termrelay
StateDirectory=termrelay
WorkingDirectory=${STATE_DIR}
ExecStart=${runtime_wrapper}
Restart=always
RestartSec=2
KillMode=process
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
EOF
  chmod 0644 "$destination"
}

ensure_system_user() {
  if ! id -u termrelay >/dev/null 2>&1; then
    useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin --user-group termrelay
  fi
  install -d -o termrelay -g termrelay -m 0750 "$STATE_DIR"
}

build_install_candidates() {
  local candidate_binary="$1"
  local candidate_env="${INSTALL_STAGING_DIR}/termrelay.env"
  local candidate_wrapper="${INSTALL_STAGING_DIR}/termrelay-run"
  local candidate_unit="${INSTALL_STAGING_DIR}/termrelay.service"
  local final_env="$ENV_FILE"
  local status=0

  [[ -s "$candidate_binary" && -x "$candidate_binary" ]] || return 1
  if [[ -e "$final_env" || -L "$final_env" ]]; then
    cp --dereference --preserve=mode,ownership,timestamps -- "$final_env" "$candidate_env" || return $?
  fi

  INSTALL_STAGING_ONLY=1
  ENV_FILE="$candidate_env"
  if write_env_file; then
    :
  else
    status=$?
  fi
  if [[ "$status" -eq 0 ]]; then
    if source "$candidate_env"; then
      :
    else
      status=$?
    fi
  fi
  if [[ "$status" -eq 0 ]]; then
    if write_wrapper "$candidate_wrapper" "$final_env"; then
      :
    else
      status=$?
    fi
  fi
  if [[ "$status" -eq 0 ]]; then
    if write_unit "$candidate_unit" "$WRAPPER_FILE"; then
      :
    else
      status=$?
    fi
  fi
  ENV_FILE="$final_env"
  INSTALL_STAGING_ONLY=0
  [[ "$status" -eq 0 ]] || return "$status"

  bash -n "$candidate_env" || return $?
  bash -n "$candidate_wrapper" || return $?
  grep -Fq "ExecStart=${WRAPPER_FILE}" "$candidate_unit" || return 1
}

snapshot_install_file() {
  local key="$1"
  local path="$2"

  if [[ -e "$path" || -L "$path" ]]; then
    printf 'present\n' >"${INSTALL_ROLLBACK_DIR}/${key}.state" || return $?
    cp -a -- "$path" "${INSTALL_ROLLBACK_DIR}/${key}" || return $?
  else
    printf 'absent\n' >"${INSTALL_ROLLBACK_DIR}/${key}.state" || return $?
  fi
}

restore_install_file() {
  local key="$1"
  local path="$2"
  local state

  state="$(cat "${INSTALL_ROLLBACK_DIR}/${key}.state")" || return 1
  rm -f -- "$path" || return 1
  if [[ "$state" == "present" ]]; then
    install -d -m 0755 "$(dirname "$path")" || return 1
    cp -a -- "${INSTALL_ROLLBACK_DIR}/${key}" "$path" || return 1
  fi
}

prepare_install_rollback() {
  INSTALL_ROLLBACK_DIR="${INSTALL_STAGING_DIR}/rollback"
  rm -rf -- "$INSTALL_ROLLBACK_DIR" || return $?
  install -d -m 0700 "$INSTALL_ROLLBACK_DIR" || return $?
  snapshot_install_file binary "${INSTALL_PREFIX}/bin/${BIN_NAME}" || return $?
  snapshot_install_file env "$ENV_FILE" || return $?
  snapshot_install_file wrapper "$WRAPPER_FILE" || return $?
  snapshot_install_file unit "$UNIT_FILE" || return $?

  INSTALL_SERVICE_WAS_ACTIVE=0
  if systemctl is-active --quiet "$SERVICE_NAME"; then
    INSTALL_SERVICE_WAS_ACTIVE=1
  fi
  INSTALL_SERVICE_WAS_ENABLED=0
  if systemctl is-enabled --quiet "$SERVICE_NAME"; then
    INSTALL_SERVICE_WAS_ENABLED=1
  fi
}

commit_install_file() {
  local key="$1"
  local candidate="$2"
  local target="$3"
  local mode="$4"
  local owner="$5"
  local group="$6"
  local target_dir staged_target

  target_dir="$(dirname "$target")"
  install -d -m 0755 "$target_dir" || return $?
  staged_target="$(mktemp "${target_dir}/.${BIN_NAME}.${key}.install.XXXXXX")" || return $?
  if install -m "$mode" -o "$owner" -g "$group" "$candidate" "$staged_target"; then
    :
  else
    local status=$?
    rm -f -- "$staged_target"
    return "$status"
  fi
  if mv -f -- "$staged_target" "$target"; then
    :
  else
    local status=$?
    rm -f -- "$staged_target"
    return "$status"
  fi
  INSTALL_ANY_FILE_COMMITTED=1
}

prepare_runtime_support_files() {
  if trusted_registry_defaults_enabled; then
    ensure_default_registry_file || return $?
    ensure_setup_token_file || return $?
  fi
}

verify_service_healthy() {
  systemctl is-active --quiet "$SERVICE_NAME" || return $?
  sleep 1
  systemctl is-active --quiet "$SERVICE_NAME"
}

complete_install_commit() {
  local candidate_binary="$1"

  prepare_runtime_support_files || return $?
  commit_install_file binary "$candidate_binary" "${INSTALL_PREFIX}/bin/${BIN_NAME}" 0755 root root || return $?
  commit_install_file env "${INSTALL_STAGING_DIR}/termrelay.env" "$ENV_FILE" 0640 root "$SERVICE_NAME" || return $?
  commit_install_file wrapper "${INSTALL_STAGING_DIR}/termrelay-run" "$WRAPPER_FILE" 0755 root root || return $?
  commit_install_file unit "${INSTALL_STAGING_DIR}/termrelay.service" "$UNIT_FILE" 0644 root root || return $?
  systemctl daemon-reload || return $?
  systemctl enable "$SERVICE_NAME" || return $?
  systemctl restart "$SERVICE_NAME" || return $?
  verify_service_healthy || return $?
}

rollback_failed_install() {
  local rollback_failed=0

  if [[ "$INSTALL_ANY_FILE_COMMITTED" -eq 0 ]]; then
    return 0
  fi

  restore_install_file binary "${INSTALL_PREFIX}/bin/${BIN_NAME}" || rollback_failed=1
  restore_install_file env "$ENV_FILE" || rollback_failed=1
  restore_install_file wrapper "$WRAPPER_FILE" || rollback_failed=1
  restore_install_file unit "$UNIT_FILE" || rollback_failed=1

  systemctl daemon-reload || rollback_failed=1
  if [[ "$INSTALL_SERVICE_WAS_ENABLED" -eq 1 ]]; then
    systemctl enable "$SERVICE_NAME" || rollback_failed=1
  else
    systemctl disable "$SERVICE_NAME" || rollback_failed=1
  fi
  if [[ "$INSTALL_SERVICE_WAS_ACTIVE" -eq 1 ]]; then
    systemctl restart "$SERVICE_NAME" || rollback_failed=1
  else
    systemctl stop "$SERVICE_NAME" || rollback_failed=1
  fi

  if [[ "$rollback_failed" -ne 0 ]]; then
    printf '[%s-install] rollback after installation failure was incomplete; primary installation failure is preserved; inspect binary, env, wrapper, unit, and service state\n' "$COMPONENT" >&2
    return 1
  fi
}

install_staged_candidate() {
  local candidate_binary="$1"
  local status=0

  INSTALL_ANY_FILE_COMMITTED=0
  prepare_install_rollback || return $?
  if complete_install_commit "$candidate_binary"; then
    status=0
  else
    status=$?
  fi
  if [[ "$status" -ne 0 ]]; then
    printf '[%s-install] installation failed with status %s; attempting rollback\n' "$COMPONENT" "$status" >&2
    rollback_failed_install || true
    return "$status"
  fi

  rm -rf -- "$INSTALL_ROLLBACK_DIR"
  INSTALL_ROLLBACK_DIR=""
}

cleanup_install_staging() {
  if [[ -n "$INSTALL_STAGING_DIR" ]]; then
    rm -rf -- "$INSTALL_STAGING_DIR"
    INSTALL_STAGING_DIR=""
  fi
}

uninstall_component() {
  require_cmd systemctl

  log "stopping and disabling ${SERVICE_NAME}.service if present"
  systemctl stop "$SERVICE_NAME" 2>/dev/null || true
  systemctl disable "$SERVICE_NAME" 2>/dev/null || true

  # 默认保留 relay 本地状态目录；只有 --purge 才删除数据和 system user。
  rm -f "$UNIT_FILE"
  rm -f "$WRAPPER_FILE"
  rmdir "$WRAPPER_DIR" 2>/dev/null || true
  rm -f "${INSTALL_PREFIX}/bin/${BIN_NAME}"
  rm -f "$ENV_FILE"
  rmdir "$ENV_DIR" 2>/dev/null || true

  systemctl daemon-reload
  systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true

  if [[ "$PURGE_STATE" -eq 1 ]]; then
    log "purging ${STATE_DIR} and system user ${SERVICE_NAME}"
    rm -rf "$STATE_DIR"
    if id -u "$SERVICE_NAME" >/dev/null 2>&1; then
      userdel "$SERVICE_NAME" 2>/dev/null || true
    fi
    if getent group "$SERVICE_NAME" >/dev/null 2>&1; then
      groupdel "$SERVICE_NAME" 2>/dev/null || true
    fi
  else
    log "preserved ${STATE_DIR}; rerun with --uninstall --purge to remove local relay state"
  fi

  log "uninstalled ${BIN_NAME}"
}

main() {
  parse_args "$@"
  normalize_proxy_environment
  require_root
  if [[ "$ACTION" == "uninstall" ]]; then
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
  log "installing ${BIN_NAME} ${VERSION}"

  INSTALL_STAGING_DIR="$(mktemp -d)"
  trap cleanup_install_staging EXIT
  local candidate_binary="${INSTALL_STAGING_DIR}/${BIN_NAME}"
  if ! install_from_release "$candidate_binary"; then
    install_from_source "$candidate_binary"
  fi

  ensure_system_user
  build_install_candidates "$candidate_binary" || die "failed to stage and validate installation artifacts; installed files were not changed"
  install_staged_candidate "$candidate_binary" || die "installation failed; the installer attempted to restore the previous binary, configuration, and service state"
  cleanup_install_staging
  trap - EXIT

  log "installed ${BIN_NAME} ${VERSION} and started ${SERVICE_NAME}.service"
  if [[ -r "${TERMRELAY_SETUP_TOKEN_FILE:-}" ]]; then
    log "relay setup token file: ${TERMRELAY_SETUP_TOKEN_FILE}"
    log "read it locally with: sudo cat ${TERMRELAY_SETUP_TOKEN_FILE}"
  fi
}

main "$@"
