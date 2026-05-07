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
STATE_DIR="/var/lib/termd"

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

write_env_file() {
  if [[ -e "$ENV_FILE" ]]; then
    log "keeping existing env file at ${ENV_FILE}"
    return 0
  fi

  install -d -m 0750 "$ENV_DIR"
  {
    printf '# 这个文件由安装脚本创建，systemd wrapper 会读取它。\n'
    printf '# 需要 relay、TLS、Web 或自定义监听时，取消注释并修改对应变量。\n'
    printf 'TERMD_LISTEN=%q\n' "${TERMD_LISTEN:-127.0.0.1:8765}"
    printf 'TERMD_WEB_ENABLED=%q\n' "${TERMD_WEB_ENABLED:-0}"
    if [[ -n "${TERMD_RELAY_URLS:-}" ]]; then
      printf 'TERMD_RELAY_URLS=%q\n' "$TERMD_RELAY_URLS"
    else
      printf '# TERMD_RELAY_URLS="ws://127.0.0.1:8080 wss://relay.example:443"\n'
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
  chmod 0600 "$ENV_FILE"
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
User=termd
Group=termd
StateDirectory=termd
WorkingDirectory=${STATE_DIR}
Environment=HOME=${STATE_DIR}
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
  if ! id -u termd >/dev/null 2>&1; then
    useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin --user-group termd
  fi
  install -d -o termd -g termd -m 0750 "$STATE_DIR"
}

main() {
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
  write_wrapper
  write_unit

  systemctl daemon-reload
  systemctl enable --now "$SERVICE_NAME"

  log "installed ${BIN_NAME} ${VERSION} and started ${SERVICE_NAME}.service"
}

main "$@"
