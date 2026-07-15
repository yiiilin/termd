#!/usr/bin/env bash

set -euo pipefail

# 这个脚本通过 curl/wget 拉取 GitHub Release 产物；没有对应产物时，会回退到源码编译。
# 约定：TERMD_GITHUB_REPO 必须指向实际仓库，例如 owner/repo。

COMPONENT="termctl"
BIN_NAME="termctl"
INSTALL_PREFIX="${TERMD_INSTALL_PREFIX:-/usr/local}"
REPO="${TERMD_GITHUB_REPO:-${GITHUB_REPOSITORY:-}}"
VERSION="${TERMD_VERSION:-}"
ACTION="install"
LOG_EMITTED=0
SELF_INSTALL_MODE_SET=0
SELF_INSTALL_BINARY_SET=0
SELF_INSTALL_ENABLED=0
SELF_INSTALL_MODE=""
SELF_INSTALL_BINARY=""

# 内嵌安装器通过 proc fd 传入已经打开的运行中可执行文件。先捕获再立即清除，
# 避免内部控制变量继续传给版本探测或后续命令。
if [[ -v TERMD_INSTALL_SELF_MODE ]]; then
  SELF_INSTALL_MODE_SET=1
  SELF_INSTALL_MODE="$TERMD_INSTALL_SELF_MODE"
fi
if [[ -v TERMD_INSTALL_SELF_BINARY ]]; then
  SELF_INSTALL_BINARY_SET=1
  SELF_INSTALL_BINARY="$TERMD_INSTALL_SELF_BINARY"
fi
unset TERMD_INSTALL_SELF_MODE TERMD_INSTALL_SELF_BINARY TERMD_INSTALL_ASSUME_YES

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

validate_self_install_request() {
  local reported_version

  if [[ "$SELF_INSTALL_MODE_SET" -ne "$SELF_INSTALL_BINARY_SET" ]]; then
    die "embedded self-install identity is invalid"
  fi
  if [[ "$SELF_INSTALL_MODE_SET" -eq 0 ]]; then
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

require_root() {
  if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    die "please run this installer with sudo/root so it can write to ${INSTALL_PREFIX}/bin"
  fi
}

print_usage() {
  cat <<'EOF'
usage: install-termctl.sh [OPTIONS]

Install termctl CLI.

Options:
  --uninstall                 Remove the termctl binary.
  -h, --help                  Print this help.

Installer network access honors http_proxy, https_proxy, all_proxy and no_proxy,
plus their uppercase variants. Lowercase values take precedence when both are set.

Examples:
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
  curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash -s -- --uninstall
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      -h|--help)
        print_usage
        exit 0
        ;;
      --uninstall)
        ACTION="uninstall"
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

install_from_self_binary() {
  local staging_dir candidate

  staging_dir="$(mktemp -d)"
  candidate="${staging_dir}/${BIN_NAME}"
  if ! install -m0755 -- "$SELF_INSTALL_BINARY" "$candidate"; then
    rm -rf -- "$staging_dir"
    return 1
  fi
  if ! install -Dm0755 -- "$candidate" "${INSTALL_PREFIX}/bin/${BIN_NAME}"; then
    rm -rf -- "$staging_dir"
    return 1
  fi
  rm -rf -- "$staging_dir"
}

uninstall_component() {
  rm -f "${INSTALL_PREFIX}/bin/${BIN_NAME}"
  log "uninstalled ${BIN_NAME}; user pairing state files were not removed"
}

main() {
  parse_args "$@"
  normalize_proxy_environment
  validate_self_install_request
  require_root
  if [[ "$ACTION" == "uninstall" ]]; then
    uninstall_component
    return 0
  fi

  require_cmd install
  if [[ "$SELF_INSTALL_ENABLED" -eq 0 ]]; then
    require_cmd tar
    require_cmd sha256sum
    require_cmd python3
    [[ -n "$REPO" ]] || die "set TERMD_GITHUB_REPO=owner/repo before running the installer"
    resolve_version
  fi
  log "installing ${BIN_NAME} ${VERSION} into ${INSTALL_PREFIX}/bin"

  if [[ "$SELF_INSTALL_ENABLED" -eq 1 ]]; then
    install_from_self_binary || die "failed to stage embedded self-install binary"
  elif ! install_from_release; then
    install_from_source
  fi

  log "installed ${BIN_NAME} ${VERSION}"
  [[ "$("${INSTALL_PREFIX}/bin/${BIN_NAME}" --version 2>/dev/null)" == "$BIN_NAME $VERSION" ]] || \
    die "installed binary identity is invalid"
}

main "$@"
