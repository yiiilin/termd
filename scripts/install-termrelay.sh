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
INSTALL_SET_AUTH_TOKEN=0
INSTALL_SET_TLS_CERT=0
INSTALL_SET_TLS_KEY=0

log() {
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
usage: install-termrelay.sh [OPTIONS]

Install termrelay and register termrelay.service.

Options:
  --web                       Enable embedded Web UI in systemd.
  --no-web                    Disable embedded Web UI in systemd.
  --listen <HOST:PORT>        Set TERMRELAY_LISTEN, for example 0.0.0.0:8080.
  --public                    Alias for --listen 0.0.0.0:8080.
  --auth-token <TOKEN>        Set relay transport auth token.
  --tls-cert <PATH>           Set TLS certificate path.
  --tls-key <PATH>            Set TLS private key path.
  -h, --help                  Print this help.

Examples:
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --web
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080 --auth-token replace-me
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
      --auth-token)
        [[ $# -ge 2 && -n "$2" ]] || die "--auth-token requires a non-empty value"
        TERMRELAY_AUTH_TOKEN="$2"
        INSTALL_SET_AUTH_TOKEN=1
        shift 2
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
    upsert_env_var "TERMRELAY_LISTEN" "$TERMRELAY_LISTEN"
  fi
  if [[ "$INSTALL_SET_WEB" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_WEB_ENABLED" "$TERMRELAY_WEB_ENABLED"
  fi
  if [[ "$INSTALL_SET_AUTH_TOKEN" -eq 1 ]]; then
    upsert_env_var "TERMRELAY_AUTH_TOKEN" "$TERMRELAY_AUTH_TOKEN"
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
  install -d -m 0755 "$ENV_DIR"

  if [[ -e "$ENV_FILE" ]]; then
    log "keeping existing env file at ${ENV_FILE}"
    apply_env_overrides
    chown root:"$SERVICE_NAME" "$ENV_FILE"
    chmod 0640 "$ENV_FILE"
    return 0
  fi

  {
    printf '# 这个文件由安装脚本创建，systemd wrapper 会读取它。\n'
    printf '# 需要自定义监听、TLS、Web 或 relay auth 时，取消注释并修改对应变量。\n'
    printf 'TERMRELAY_LISTEN=%q\n' "${TERMRELAY_LISTEN:-127.0.0.1:8080}"
    printf 'TERMRELAY_WEB_ENABLED=%q\n' "${TERMRELAY_WEB_ENABLED:-0}"
    if [[ -n "${TERMRELAY_AUTH_TOKEN:-}" ]]; then
      printf 'TERMRELAY_AUTH_TOKEN=%q\n' "$TERMRELAY_AUTH_TOKEN"
    else
      printf '# TERMRELAY_AUTH_TOKEN=replace-me\n'
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
  install -d -m 0755 "$WRAPPER_DIR"
  cat >"$WRAPPER_FILE" <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

# 这个 wrapper 在 systemd 下组装 termrelay 的启动参数，便于通过 env 文件配置监听、TLS 和 auth token。

ENV_FILE="/etc/termd/termrelay.env"

if [[ -r "$ENV_FILE" ]]; then
  # shellcheck source=/dev/null
  source "$ENV_FILE"
fi

args=(--listen "${TERMRELAY_LISTEN:-127.0.0.1:8080}")

if [[ -n "${TERMRELAY_AUTH_TOKEN:-}" ]]; then
  args+=(--auth-token "$TERMRELAY_AUTH_TOKEN")
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

exec /usr/local/bin/termrelay "${args[@]}"
EOF
  chmod 0755 "$WRAPPER_FILE"
}

write_unit() {
  cat >"$UNIT_FILE" <<EOF
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
ExecStart=${WRAPPER_FILE}
Restart=always
RestartSec=2
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
EOF
}

ensure_system_user() {
  if ! id -u termrelay >/dev/null 2>&1; then
    useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin --user-group termrelay
  fi
  install -d -o termrelay -g termrelay -m 0750 "$STATE_DIR"
}

main() {
  parse_args "$@"
  require_root
  require_cmd install
  require_cmd tar
  require_cmd sha256sum
  require_cmd python3
  require_cmd systemctl
  require_cmd useradd
  [[ -n "$REPO" ]] || die "set TERMD_GITHUB_REPO=owner/repo before running the installer"

  resolve_version
  log "installing ${BIN_NAME} ${VERSION}"

  if ! install_from_release; then
    install_from_source
  fi

  ensure_system_user
  write_env_file
  # 重新读取最终 env，保证 wrapper 和 systemd 重启使用同一组配置。
  # shellcheck source=/dev/null
  source "$ENV_FILE"
  write_wrapper
  write_unit

  systemctl daemon-reload
  systemctl enable "$SERVICE_NAME"
  systemctl restart "$SERVICE_NAME"

  log "installed ${BIN_NAME} ${VERSION} and started ${SERVICE_NAME}.service"
}

main "$@"
