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

assert_help_excludes() {
  local script="$1"
  local forbidden="$2"
  local output

  output="$(bash "${ROOT_DIR}/${script}" --help)"
  if [[ "$output" == *"$forbidden"* ]]; then
    printf 'expected %s --help to exclude %q\n' "$script" "$forbidden" >&2
    exit 1
  fi
}

for script in \
  scripts/install-termd.sh \
  scripts/install-termctl.sh \
  scripts/install-termrelay.sh \
  scripts/update-local-termd.sh
do
  bash -n "${ROOT_DIR}/${script}"
done

assert_help_contains scripts/install-termd.sh "--uninstall"
assert_help_contains scripts/install-termctl.sh "--uninstall"
assert_help_contains scripts/install-termrelay.sh "--uninstall"

assert_help_contains scripts/install-termd.sh "--web"
assert_help_contains scripts/install-termd.sh "--listen <HOST:PORT>"
assert_help_contains scripts/install-termd.sh "--proxy <URL>"
assert_help_excludes scripts/install-termd.sh "--allow-open-relay"
assert_help_excludes scripts/install-termrelay.sh "--allow-open-relay"
assert_help_contains scripts/install-termd.sh "--supervisor-version <VER>"
assert_help_contains scripts/install-termd.sh "--user <USER>"
assert_help_contains scripts/install-termd.sh "--purge"
assert_help_excludes scripts/install-termd.sh "--relay-auth-token"
assert_help_excludes scripts/install-termd.sh "--relay-auth-token-file"
assert_help_excludes scripts/install-termd.sh "--relay-daemon-token <TOKEN>"
assert_help_excludes scripts/install-termd.sh "--relay-setup-token <TOKEN>"
assert_help_excludes scripts/install-termd.sh "--relay-token <TOKEN>"
assert_help_contains scripts/update-local-termd.sh "--workspace-tests"
assert_help_contains scripts/update-local-termd.sh "--health-url <URL>"

assert_help_contains scripts/install-termrelay.sh "--web"
assert_help_contains scripts/install-termrelay.sh "--listen <HOST:PORT>"
assert_help_excludes scripts/install-termrelay.sh "--auth-token"
assert_help_excludes scripts/install-termrelay.sh "--auth-token-file"
assert_help_excludes scripts/install-termrelay.sh "--http-tunnel"
assert_help_contains scripts/install-termrelay.sh "--purge"

grep -q "KillMode=process" "${ROOT_DIR}/scripts/install-termd.sh"
grep -q "KillMode=process" "${ROOT_DIR}/scripts/install-termrelay.sh"
grep -q "KillMode" "${ROOT_DIR}/scripts/update-local-termd.sh"
grep -q "__session-supervisor" "${ROOT_DIR}/scripts/update-local-termd.sh"
grep -q "cargo build --release -p termd --bin termd --locked" "${ROOT_DIR}/scripts/update-local-termd.sh"
grep -q "systemctl restart" "${ROOT_DIR}/scripts/update-local-termd.sh"
grep -Fq '"${INSTALL_PREFIX}/bin/${BIN_NAME}" pair --qr --url "$base_url"' "${ROOT_DIR}/scripts/install-termd.sh"
grep -q 'json.load(sys.stdin)\["daemon_public_key"\]' "${ROOT_DIR}/scripts/install-termd.sh"
grep -q '"daemon_public_key": daemon_public_key' "${ROOT_DIR}/scripts/install-termd.sh"
grep -Fq '/api/relay/daemon/status' "${ROOT_DIR}/scripts/install-termd.sh"
grep -Fq 'trap cleanup_relay_registration_files EXIT' "${ROOT_DIR}/scripts/install-termd.sh"
grep -Fq 'trap cleanup_relay_status_files EXIT' "${ROOT_DIR}/scripts/install-termd.sh"
[[ "$(grep -Fc "trap 'exit 130' INT" "${ROOT_DIR}/scripts/install-termd.sh")" -eq 2 ]]
[[ "$(grep -Fc "trap 'exit 143' TERM" "${ROOT_DIR}/scripts/install-termd.sh")" -eq 2 ]]
[[ "$(grep -c 'unset_env_var "TERMD_RELAY_AUTH_TOKEN' "${ROOT_DIR}/scripts/install-termd.sh")" -eq 2 ]]
[[ "$(grep -c 'TERMRELAY_AUTH_TOKEN' "${ROOT_DIR}/scripts/install-termrelay.sh")" -eq 2 ]]
[[ "$(grep -c 'TERMRELAY_HTTP_TUNNEL' "${ROOT_DIR}/scripts/install-termrelay.sh")" -eq 1 ]]
grep -q 'SUPERVISOR_VERSION="${TERMD_SUPERVISOR_VERSION:-}"' "${ROOT_DIR}/scripts/install-termd.sh"
grep -Fq 'STATE_DIR="${TERMD_STATE_DIR:-/var/lib/termd}"' "${ROOT_DIR}/scripts/install-termd.sh"
grep -q 'REQUIRED_SUPERVISOR_VERSION="${TERMD_REQUIRED_SUPERVISOR_VERSION:-}"' "${ROOT_DIR}/scripts/install-termd.sh"
grep -q 'TERMD_REQUIRED_SUPERVISOR_VERSION:-' "${ROOT_DIR}/.github/workflows/release.yml"
! grep -q 'TERMD_SUPERVISOR_VERSION:-.*supervisor_version' "${ROOT_DIR}/.github/workflows/release.yml"
grep -Fq '"terminstall/Cargo.toml"' "${ROOT_DIR}/.github/workflows/release.yml"
grep -Fq 'install -m 0755 "$binary_path" "$out_dir/${binary}-linux-amd64"' "${ROOT_DIR}/.github/workflows/release.yml"
grep -Fq 'sha256sum *-linux-amd64 *.tar.gz > checksums.txt' "${ROOT_DIR}/.github/workflows/release.yml"
grep -Fq 'files=(*-linux-amd64 *.tar.gz checksums.txt install-*.sh)' "${ROOT_DIR}/.github/workflows/release.yml"
grep -Fq 'releases/latest/download/termd-linux-amd64' "${ROOT_DIR}/README.md"
grep -Fq 'releases/latest/download/${component}-linux-amd64' "${ROOT_DIR}/docs/installation.md"
grep -Fq 'sha256sum --ignore-missing --check checksums.txt' "${ROOT_DIR}/README.md"
grep -Fq 'sha256sum --ignore-missing --check checksums.txt' "${ROOT_DIR}/docs/installation.md"
grep -Fq 'releases/latest/download/install-termd.sh' "${ROOT_DIR}/docs/installation.md"
test -s "${ROOT_DIR}/SUPERVISOR_VERSION"
python3 - "${ROOT_DIR}" <<'PY'
import json
import pathlib
import re
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
workspace_version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
package = json.loads((root / "termui/frontend/package.json").read_text())
package_lock = json.loads((root / "termui/frontend/package-lock.json").read_text())
versions = {
    "Cargo.toml": workspace_version,
    "package.json": package["version"],
    "package-lock.json": package_lock["version"],
    "package-lock.json root package": package_lock["packages"][""]["version"],
}
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", workspace_version):
    raise SystemExit(f"workspace version is not plain semver: {workspace_version!r}")
if any(version != workspace_version for version in versions.values()):
    raise SystemExit(f"release version mismatch: {versions}")
PY
[[ "$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")" == "2026-07-12-dual-ws" ]]

_load_termd_installer_functions_source() {
  # shellcheck source=/dev/null
  source <(sed '/^main "\$@"/,$d' "${ROOT_DIR}/scripts/install-termd.sh")
}

load_termd_installer_functions() {
  # 测试只加载函数和默认变量，跳过脚本末尾的 main 调用，避免触发真实安装。
  unset SUPERVISOR_VERSION REQUIRED_SUPERVISOR_VERSION TERMD_SUPERVISOR_VERSION TERMD_REQUIRED_SUPERVISOR_VERSION TERMD_INSTALL_CONFIRM_FD
  _load_termd_installer_functions_source
}

load_termd_installer_functions_with_required_supervisor_version() {
  local required_supervisor_version="$1"

  unset SUPERVISOR_VERSION REQUIRED_SUPERVISOR_VERSION TERMD_SUPERVISOR_VERSION TERMD_INSTALL_CONFIRM_FD
  TERMD_REQUIRED_SUPERVISOR_VERSION="$required_supervisor_version" _load_termd_installer_functions_source
}

load_termrelay_installer_functions() {
  # shellcheck source=/dev/null
  source <(sed '/^main "\$@"/,$d' "${ROOT_DIR}/scripts/install-termrelay.sh")
}

load_update_local_functions() {
  # 本地更新测试只加载函数，避免触发真实 build、systemctl restart 或清理本机 session。
  unset SUPERVISOR_VERSION_TARGET SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR TERMD_SUPERVISOR_VERSION_FILE
  # shellcheck source=/dev/null
  source <(sed '/^main "\$@"/,$d' "${ROOT_DIR}/scripts/update-local-termd.sh")
}

test_installers_normalize_standard_proxy_environment() (
  local script

  for script in install-termd.sh install-termrelay.sh install-termctl.sh; do
    (
      unset http_proxy HTTP_PROXY https_proxy HTTPS_PROXY all_proxy ALL_PROXY no_proxy NO_PROXY
      http_proxy="http://lower-proxy.example:3128"
      HTTP_PROXY="http://upper-proxy.example:3128"
      https_proxy="http://secure-proxy.example:3128"
      ALL_PROXY="socks5h://socks-proxy.example:1080"
      no_proxy="127.0.0.1,localhost"
      export http_proxy HTTP_PROXY https_proxy ALL_PROXY no_proxy

      # shellcheck source=/dev/null
      source <(sed '/^main "\$@"/,$d' "${ROOT_DIR}/scripts/${script}")
      normalize_proxy_environment

      [[ "$http_proxy" == "http://lower-proxy.example:3128" ]]
      [[ "$HTTP_PROXY" == "$http_proxy" ]]
      [[ "$HTTPS_PROXY" == "$https_proxy" ]]
      [[ "$all_proxy" == "$ALL_PROXY" ]]
      [[ "$NO_PROXY" == "$no_proxy" ]]
      bash -c '[[ "$HTTP_PROXY" == "$http_proxy" && "$HTTPS_PROXY" == "$https_proxy" && "$ALL_PROXY" == "$all_proxy" && "$NO_PROXY" == "$no_proxy" ]]'
    )
  done
)

test_termctl_embedded_self_binary_is_strict_and_isolated() (
  local root script source_binary pinned_binary installed_binary probe output status
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT
  script="${ROOT_DIR}/scripts/install-termctl.sh"
  source_binary="${root}/source-termctl"
  pinned_binary="${root}/pinned-termctl"
  installed_binary="${root}/prefix/bin/termctl"
  probe="${root}/env-probe"

  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'printf "%s:%s\n" "${TERMD_INSTALL_SELF_MODE+x}" "${TERMD_INSTALL_SELF_BINARY+x}" >>"$SELF_ENV_PROBE"' \
    'if [[ "${1:-}" == "--version" ]]; then printf "%s\n" "${SELF_BINARY_VERSION:-termctl 0.8.2}"; exit 0; fi' \
    'exit 64' >"$source_binary"
  chmod 0755 "$source_binary"

  output="$(
    exec {self_binary_fd}<"$source_binary"
    mv "$source_binary" "$pinned_binary"
    printf '#!/usr/bin/env bash\nprintf "termctl 9.9.9\\n"\n' >"$source_binary"
    chmod 0755 "$source_binary"
    TERMD_INSTALL_PREFIX="${root}/prefix" \
      TERMD_VERSION=0.8.2 \
      TERMD_INSTALL_SELF_MODE=embedded-v1 \
      TERMD_INSTALL_SELF_BINARY="/proc/${BASHPID}/fd/${self_binary_fd}" \
      SELF_ENV_PROBE="$probe" \
      bash -c '
        source <(sed '\''/^main "\$@"/,$d'\'' "$1")
        require_root() { :; }
        main
      ' bash "$script" 2>&1
  )"
  [[ "$output" == *"installed termctl 0.8.2"* ]]
  [[ -x "$installed_binary" ]]
  cmp "$pinned_binary" "$installed_binary"
  [[ "$(wc -l <"$probe")" -eq 2 ]]
  [[ "$(sort -u "$probe")" == ":" ]]

  assert_rejected() {
    local mode_set="$1"
    local mode="$2"
    local path_set="$3"
    local path="$4"
    local reported_version="$5"
    local -a environment=(
      "TERMD_INSTALL_PREFIX=${root}/rejected-prefix"
      "TERMD_VERSION=0.8.2"
      "SELF_ENV_PROBE=${root}/rejected-probe"
      "SELF_BINARY_VERSION=${reported_version}"
    )
    if [[ "$mode_set" -eq 1 ]]; then
      environment+=("TERMD_INSTALL_SELF_MODE=${mode}")
    fi
    if [[ "$path_set" -eq 1 ]]; then
      environment+=("TERMD_INSTALL_SELF_BINARY=${path}")
    fi

    set +e
    output="$(
      env "${environment[@]}" bash -c '
        source <(sed '\''/^main "\$@"/,$d'\'' "$1")
        require_root() { :; }
        main
      ' bash "$script" 2>&1
    )"
    status=$?
    set -e
    [[ "$status" -ne 0 ]]
    [[ "$output" == *"embedded self-install"* ]]
    [[ "$output" != *"$path"* ]]
    [[ ! -e "${root}/rejected-prefix/bin/termctl" ]]
  }

  assert_rejected 1 wrong-mode-marker 1 "$source_binary" "termctl 0.8.2"
  assert_rejected 1 embedded-v1 0 absent-path-marker "termctl 0.8.2"
  (
    cd "$root"
    assert_rejected 1 embedded-v1 1 source-termctl "termctl 0.8.2"
  )
  ln -s "$source_binary" "${root}/source-link"
  assert_rejected 1 embedded-v1 1 "${root}/source-link" "termctl 0.8.2"
  assert_rejected 1 embedded-v1 1 "$source_binary" "termd 0.8.2"
  assert_rejected 1 embedded-v1 1 "$source_binary" "termctl 9.9.9"

  assert_pinned_rejected() {
    local reported_version="$1"

    set +e
    output="$(
      exec {self_binary_fd}<"$pinned_binary"
      TERMD_INSTALL_PREFIX="${root}/rejected-prefix" \
        TERMD_VERSION=0.8.2 \
        TERMD_INSTALL_SELF_MODE=embedded-v1 \
        TERMD_INSTALL_SELF_BINARY="/proc/${BASHPID}/fd/${self_binary_fd}" \
        SELF_ENV_PROBE="${root}/rejected-probe" \
        SELF_BINARY_VERSION="$reported_version" \
        bash -c '
          source <(sed '\''/^main "\$@"/,$d'\'' "$1")
          require_root() { :; }
          main
        ' bash "$script" 2>&1
    )"
    status=$?
    set -e
    [[ "$status" -ne 0 ]]
    [[ "$output" == *"embedded self-install identity is invalid"* ]]
    [[ ! -e "${root}/rejected-prefix/bin/termctl" ]]
  }

  assert_pinned_rejected "termd 0.8.2"
  assert_pinned_rejected "termctl 9.9.9"
)

test_termrelay_embedded_self_binary_stages_in_isolation() (
  local root script source_binary candidate probe output setup_token tls_key
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT
  script="${ROOT_DIR}/scripts/install-termrelay.sh"
  source_binary="${root}/source-termrelay"
  candidate="${root}/candidate-termrelay"
  probe="${root}/env-probe"
  setup_token="${root}/private/setup-token"
  tls_key="${root}/private/tls-key.pem"

  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'for name in TERMD_INSTALL_SELF_MODE TERMD_INSTALL_SELF_BINARY TERMD_INSTALL_ARG_SETUP_TOKEN_FILE TERMD_INSTALL_ARG_TLS_KEY; do' \
    '  if [[ -v "$name" ]]; then printf "%s\n" "$name" >>"$SELF_ENV_PROBE"; fi' \
    'done' \
    '[[ "${1:-}" == "--version" ]] || exit 64' \
    'printf "%s\n" "termrelay 0.8.2"' >"$source_binary"
  chmod 0755 "$source_binary"

  output="$(
    exec {self_binary_fd}<"$source_binary"
    TERMD_VERSION=0.8.2 \
      TERMD_INSTALL_SELF_MODE=embedded-v1 \
      TERMD_INSTALL_SELF_BINARY="/proc/${BASHPID}/fd/${self_binary_fd}" \
      TERMD_INSTALL_ARG_SETUP_TOKEN_FILE="$setup_token" \
      TERMD_INSTALL_ARG_TLS_KEY="$tls_key" \
      SELF_ENV_PROBE="$probe" \
      EXPECTED_SETUP_TOKEN="$setup_token" \
      EXPECTED_TLS_KEY="$tls_key" \
      bash -c '
        source <(sed '\''/^main "\$@"/,$d'\'' "$1")
        validate_internal_install_request
        apply_internal_install_arguments
        [[ "$TERMRELAY_SETUP_TOKEN_FILE" == "$EXPECTED_SETUP_TOKEN" ]]
        [[ "$TERMRELAY_TLS_KEY" == "$EXPECTED_TLS_KEY" ]]
        [[ -z "$INTERNAL_ARG_SETUP_TOKEN_FILE" && -z "$INTERNAL_ARG_TLS_KEY" ]]
        install_from_self_binary "$2"
      ' bash "$script" "$candidate" 2>&1
  )"

  [[ -z "$output" ]]
  [[ -x "$candidate" ]]
  cmp "$source_binary" "$candidate"
  [[ ! -s "$probe" ]]
)

test_termd_embedded_self_binary_stages_in_isolation() (
  local root script source_binary candidate probe output relay daemon_token setup_token proxy tls_key
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT
  script="${ROOT_DIR}/scripts/install-termd.sh"
  source_binary="${root}/source-termd"
  candidate="${root}/candidate-termd"
  probe="${root}/env-probe"
  relay="wss://relay-user:relay-password@relay.example/ws"
  daemon_token="${root}/private/daemon-token"
  setup_token="direct-relay-setup-secret"
  proxy="http://proxy-user:proxy-password@proxy.example"
  tls_key="${root}/private/tls-key.pem"

  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'for name in TERMD_INSTALL_SELF_MODE TERMD_INSTALL_SELF_BINARY TERMD_INSTALL_ARG_RELAY_URL TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN TERMD_INSTALL_ARG_PROXY TERMD_INSTALL_ARG_TLS_KEY; do' \
    '  if [[ -v "$name" ]]; then printf "%s\n" "$name" >>"$SELF_ENV_PROBE"; fi' \
    'done' \
    '[[ "${1:-}" == "--version" ]] || exit 64' \
    'printf "%s\n" "termd 0.8.2"' >"$source_binary"
  chmod 0755 "$source_binary"

  output="$(
    exec {self_binary_fd}<"$source_binary"
    TERMD_VERSION=0.8.2 \
      TERMD_INSTALL_SELF_MODE=embedded-v1 \
      TERMD_INSTALL_SELF_BINARY="/proc/${BASHPID}/fd/${self_binary_fd}" \
      TERMD_INSTALL_ARG_RELAY_URL="$relay" \
      TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE="$daemon_token" \
      TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN="$setup_token" \
      TERMD_INSTALL_ARG_PROXY="$proxy" \
      TERMD_INSTALL_ARG_TLS_KEY="$tls_key" \
      SELF_ENV_PROBE="$probe" \
      EXPECTED_RELAY="$relay" \
      EXPECTED_DAEMON_TOKEN="$daemon_token" \
      EXPECTED_SETUP_TOKEN="$setup_token" \
      EXPECTED_PROXY="$proxy" \
      EXPECTED_TLS_KEY="$tls_key" \
      bash -c '
        source <(sed '\''/^main "\$@"/,$d'\'' "$1")
        parse_args --allow-session-loss
        validate_internal_install_request
        apply_internal_install_arguments
        [[ "$TERMD_RELAY_URLS" == "$EXPECTED_RELAY" ]]
        [[ "$TERMD_RELAY_DAEMON_TOKEN_FILE" == "$EXPECTED_DAEMON_TOKEN" ]]
        [[ "$TERMD_RELAY_SETUP_TOKEN" == "$EXPECTED_SETUP_TOKEN" ]]
        [[ "$http_proxy" == "$EXPECTED_PROXY" && "$https_proxy" == "$EXPECTED_PROXY" ]]
        [[ "$TERMD_TLS_KEY" == "$EXPECTED_TLS_KEY" ]]
        [[ -z "$INTERNAL_ARG_RELAY_URL" && -z "$INTERNAL_ARG_RELAY_DAEMON_TOKEN_FILE" ]]
        [[ -z "$INTERNAL_ARG_RELAY_SETUP_TOKEN" && -z "$INTERNAL_ARG_PROXY" && -z "$INTERNAL_ARG_TLS_KEY" ]]
        prompt_confirmation "must not read from a terminal"
        install_from_self_binary "$2"
      ' bash "$script" "$candidate" 2>&1
  )"

  [[ -z "$output" ]]
  [[ -x "$candidate" ]]
  cmp "$source_binary" "$candidate"
  [[ ! -s "$probe" ]]
)

test_termd_installer_supports_stdin_pipe_execution() (
  local output

  output="$(cat "${ROOT_DIR}/scripts/install-termd.sh" | bash -s -- --help 2>&1)"
  [[ "$output" == *"usage: install-termd.sh"* ]]
  if [[ "$output" == *"BASH_SOURCE"* || "$output" == *"unbound variable"* ]]; then
    printf 'termd installer emitted an error when executed from stdin:\n%s\n' "$output" >&2
    return 1
  fi
)

test_installers_reject_removed_open_relay_flag() (
  local script output

  for script in install-termd.sh install-termrelay.sh; do
    if output="$(bash "${ROOT_DIR}/scripts/${script}" --allow-open-relay 2>&1)"; then
      printf '%s unexpectedly accepted --allow-open-relay\n' "$script" >&2
      return 1
    fi
    [[ "$output" == *"unknown installer argument: --allow-open-relay"* ]]
  done
)

test_termd_initial_pairing_uses_real_qr_command() (
  load_termd_installer_functions

  local tmp_dir output args_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  INSTALL_PREFIX="${tmp_dir}/prefix"
  args_file="${tmp_dir}/pair-args"
  mkdir -p "${INSTALL_PREFIX}/bin"
  cat >"${INSTALL_PREFIX}/bin/termd" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$PAIR_ARGS_FILE"
printf 'REAL_QR_OUTPUT\ntermd-pair:v2:real-invite\n'
EOF
  chmod 0755 "${INSTALL_PREFIX}/bin/termd"
  PAIR_ARGS_FILE="$args_file"
  export PAIR_ARGS_FILE
  TERMD_LISTEN="127.0.0.1:9876"
  unset TERMD_RELAY_URLS TERMD_RELAY_SETUP_TOKEN TERMD_RELAY_SETUP_TOKEN_FILE
  get_local_healthz() {
    printf '{"server_id":"00000000-0000-0000-0000-000000000001","daemon_public_key":"ed25519-v1:test"}'
  }

  output="$(print_initial_pairing_token)"

  [[ "$output" == *"REAL_QR_OUTPUT"* ]]
  [[ "$output" == *"termd-pair:v2:real-invite"* ]]
  [[ "$(<"$args_file")" == "pair --qr --url http://127.0.0.1:9876" ]]
)

test_termrelay_install_reports_sensitive_setup_token() (
  load_termrelay_installer_functions

  local tmp_dir token_file token trusted_output
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  token_file="${tmp_dir}/relay-setup-token"
  token="relay-setup-secret-for-installer-test"
  printf '%s\n' "$token" >"$token_file"
  TERMRELAY_SETUP_TOKEN_FILE="$token_file"
  INSTALL_SET_DAEMON_REGISTRY=0
  INSTALL_SET_SETUP_TOKEN_FILE=0

  trusted_output="$(print_relay_setup_token)"
  [[ "$trusted_output" == *"SENSITIVE relay setup token"* ]]
  [[ "$trusted_output" == *"$token"* ]]
  [[ "$trusted_output" == *"termd install --relay <WS_URL>"* ]]
  [[ "$trusted_output" == *"--relay-token <TOKEN>"* ]]
)

test_termrelay_old_open_env_migrates_to_trusted_files() (
  load_termrelay_installer_functions

  local root output token token_file registry_before
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT
  ENV_DIR="${root}/etc"
  ENV_FILE="${ENV_DIR}/termrelay.env"
  STATE_DIR="${root}/state"
  WRAPPER_FILE="${root}/lib/termrelay-run"
  TERMRELAY_SETUP_TOKEN_FILE="${ENV_DIR}/termrelay_setup_token"
  TERMRELAY_DAEMON_REGISTRY="${STATE_DIR}/daemon-registry.json"
  INSTALL_SET_SETUP_TOKEN_FILE=0
  INSTALL_SET_DAEMON_REGISTRY=0
  INSTALL_STAGING_ONLY=0
  mkdir -p "$ENV_DIR" "$STATE_DIR"
  printf 'TERMRELAY_LISTEN=127.0.0.1:9000\nTERMRELAY_ALLOW_OPEN_RELAY=1\n' >"$ENV_FILE"
  token_file="$TERMRELAY_SETUP_TOKEN_FILE"
  printf ' \n\t\n' >"$token_file"
  printf ' \n\t\n' >"$TERMRELAY_DAEMON_REGISTRY"
  install() {
    if [[ "${1:-}" == "-d" ]]; then
      mkdir -p "${@: -1}"
      return 0
    fi
    command install "$@"
  }
  chown() { :; }

  write_env_file

  [[ "$TERMRELAY_SETUP_TOKEN_FILE" == "$token_file" ]]
  [[ -s "$TERMRELAY_SETUP_TOKEN_FILE" ]]
  LC_ALL=C grep -q '[^[:space:]]' -- "$TERMRELAY_SETUP_TOKEN_FILE"
  [[ -s "$TERMRELAY_DAEMON_REGISTRY" ]]
  [[ "$(<"$TERMRELAY_DAEMON_REGISTRY")" == '{"daemons":[]}' ]]
  ! grep -Fq 'TERMRELAY_ALLOW_OPEN_RELAY' "$ENV_FILE"
  grep -Fq "TERMRELAY_SETUP_TOKEN_FILE=${TERMRELAY_SETUP_TOKEN_FILE}" "$ENV_FILE"
  grep -Fq "TERMRELAY_DAEMON_REGISTRY=${TERMRELAY_DAEMON_REGISTRY}" "$ENV_FILE"
  token="$(<"$TERMRELAY_SETUP_TOKEN_FILE")"
  registry_before="$(sha256sum "$TERMRELAY_DAEMON_REGISTRY")"
  output="$(print_relay_setup_token)"
  [[ "$output" == *"$token"* ]]

  unset TERMRELAY_SETUP_TOKEN_FILE TERMRELAY_DAEMON_REGISTRY
  write_env_file
  [[ "$(<"$TERMRELAY_SETUP_TOKEN_FILE")" == "$token" ]]
  [[ "$(sha256sum "$TERMRELAY_DAEMON_REGISTRY")" == "$registry_before" ]]
)

assert_secure_relay_curl_invocation() {
  local expected_url="$1"
  local expected_setup_token="$2"
  shift 2
  local argument header_file payload_file expected_headers token_declaration

  [[ "$#" -eq 13 ]]
  [[ "$1" == "--disable" ]]
  [[ "$2" == "--globoff" ]]
  [[ "$3" == "--fail" && "$4" == "--silent" && "$5" == "--show-error" ]]
  [[ "$6" == "--request" && "$7" == "POST" ]]
  [[ "$8" == "--header" && "$9" == @* ]]
  [[ "${10}" == "--data-binary" && "${11}" == @* ]]
  [[ "${12}" == "--url" && "${13}" == "$expected_url" ]]
  for argument in "$@"; do
    [[ "$argument" != "--config" && "$argument" != "-K" ]]
    [[ "$argument" != *"$expected_setup_token"* ]]
  done

  header_file="${9#@}"
  payload_file="${11#@}"
  [[ -f "$header_file" && "$(stat -c %a "$header_file")" == "600" ]]
  [[ -f "$payload_file" && "$(stat -c %a "$payload_file")" == "600" ]]
  printf -v expected_headers \
    'content-type: application/json\nx-termd-relay-setup-token: %s' \
    "$expected_setup_token"
  [[ "$(<"$header_file")" == "$expected_headers" ]]
  token_declaration="$(declare -p TERMD_RELAY_SETUP_TOKEN 2>/dev/null || true)"
  [[ "$token_declaration" != "declare -x "* ]]

  printf '%s' "$payload_file"
}

test_termd_relay_verification_requires_matching_server_id() (
  load_termd_installer_functions

  local health_response failed_output success_output
  health_response='{"server_id":"00000000-0000-0000-0000-000000000001"}'
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_SETUP_TOKEN="relay-setup-secret"
  unset TERMD_RELAY_SETUP_TOKEN_FILE
  sleep() { :; }
  curl() {
    assert_secure_relay_curl_invocation \
      "https://relay.example/api/relay/daemon/status" \
      "relay-setup-secret" \
      "$@" >/dev/null
    printf '{"server_id":"00000000-0000-0000-0000-000000000002","connected":true}'
  }

  if failed_output="$(verify_daemon_relay_connected "$health_response")"; then
    printf 'relay verification accepted a different daemon server_id\n' >&2
    return 1
  fi
  [[ "$failed_output" == *"FAILED: daemon 00000000-0000-0000-0000-000000000001"* ]]

  curl() {
    assert_secure_relay_curl_invocation \
      "https://relay.example/api/relay/daemon/status" \
      "relay-setup-secret" \
      "$@" >/dev/null
    printf '{"server_id":"00000000-0000-0000-0000-000000000001","connected":true}'
  }
  success_output="$(verify_daemon_relay_connected "$health_response")"
  [[ "$success_output" == *"SUCCESS: daemon 00000000-0000-0000-0000-000000000001"* ]]
)

test_termd_relay_verification_requires_setup_token() (
  load_termd_installer_functions

  local health_response output curl_marker
  health_response='{"server_id":"00000000-0000-0000-0000-000000000003"}'
  TERMD_RELAY_URLS="ws://relay.example/ws"
  INSTALL_SET_RELAY_URLS=1
  unset TERMD_RELAY_SETUP_TOKEN TERMD_RELAY_SETUP_TOKEN_FILE
  curl_marker="$(mktemp)"
  rm -f "$curl_marker"
  curl() {
    : >"$curl_marker"
    return 0
  }

  if output="$(verify_daemon_relay_connected "$health_response")"; then
    printf 'relay verification unexpectedly accepted a missing setup token\n' >&2
    return 1
  fi
  [[ "$output" == *"FAILED: relay setup token is empty, unreadable, or contains CR/LF"* ]]
  [[ ! -e "$curl_marker" ]]
)

test_termd_relay_curl_treats_hostile_values_as_data() (
  load_termd_installer_functions

  local tmp_dir daemon_token_file hostile_token hostile_register_endpoint hostile_status_endpoint output
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  daemon_token_file="${tmp_dir}/daemon-token"
  printf 'daemon-secret\n' >"$daemon_token_file"
  hostile_token='relay-secret-" config = "/tmp/not-a-config" --config /tmp/not-a-config'
  hostile_register_endpoint='https://{relay.example,attacker.example}/register[1-2]"--config=/tmp/not-a-config'
  hostile_status_endpoint='https://{relay.example,attacker.example}/status[1-2]"--config=/tmp/not-a-config'
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_DAEMON_TOKEN_FILE="$daemon_token_file"
  TERMD_RELAY_SETUP_TOKEN="$hostile_token"
  unset TERMD_RELAY_SETUP_TOKEN_FILE
  export TERMD_RELAY_SETUP_TOKEN
  sleep() { :; }
  relay_api_url() {
    case "$2" in
      /api/relay/daemon/register) printf '%s' "$hostile_register_endpoint" ;;
      /api/relay/daemon/status) printf '%s' "$hostile_status_endpoint" ;;
      *) return 1 ;;
    esac
  }
  curl() {
    local payload_file
    case "${13}" in
      "$hostile_register_endpoint")
        payload_file="$(assert_secure_relay_curl_invocation "$hostile_register_endpoint" "$hostile_token" "$@")" || return 1
        python3 - "$payload_file" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as payload_file:
    payload = json.load(payload_file)
assert payload["server_id"] == "server-v070", payload
PY
        ;;
      "$hostile_status_endpoint")
        assert_secure_relay_curl_invocation "$hostile_status_endpoint" "$hostile_token" "$@" >/dev/null || return 1
        printf '{"server_id":"server-v070","connected":true}'
        ;;
      *) return 1 ;;
    esac
  }

  output="$(register_daemon_with_relay \
    '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public"}')"
  [[ "$output" == *"registered local daemon server-v070"* ]]
  [[ "$output" != *"$hostile_token"* ]]
  output="$(verify_daemon_relay_connected '{"server_id":"server-v070"}')"
  [[ "$output" == *"SUCCESS: daemon server-v070"* ]]
  [[ "$output" != *"$hostile_token"* ]]
)

test_termd_relay_curl_rejects_crlf_inputs_without_disclosure() (
  load_termd_installer_functions

  local tmp_dir daemon_token_file hostile_token hostile_url output curl_marker
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  daemon_token_file="${tmp_dir}/daemon-token"
  curl_marker="${tmp_dir}/curl-called"
  printf 'daemon-secret\n' >"$daemon_token_file"
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_DAEMON_TOKEN_FILE="$daemon_token_file"
  unset TERMD_RELAY_SETUP_TOKEN_FILE
  curl() {
    : >"$curl_marker"
    return 0
  }

  hostile_token=$'relay-secret-"\r\noutput = "/tmp/not-created"'
  TERMD_RELAY_SETUP_TOKEN="$hostile_token"
  export TERMD_RELAY_SETUP_TOKEN
  if output="$(register_daemon_with_relay \
    '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public"}')"; then
    printf 'relay registration unexpectedly accepted a CR/LF setup token\n' >&2
    return 1
  fi
  [[ "$output" == *"relay setup token is empty, unreadable, or contains CR/LF"* ]]
  [[ "$output" != *"$hostile_token"* && ! -e "$curl_marker" ]]
  if output="$(verify_daemon_relay_connected '{"server_id":"server-v070"}')"; then
    printf 'relay status unexpectedly accepted a CR/LF setup token\n' >&2
    return 1
  fi
  [[ "$output" == *"relay setup token is empty, unreadable, or contains CR/LF"* ]]
  [[ "$output" != *"$hostile_token"* && ! -e "$curl_marker" ]]

  TERMD_RELAY_SETUP_TOKEN="safe-relay-setup-token"
  export TERMD_RELAY_SETUP_TOKEN
  for hostile_url in \
    $'wss://relay.example\r@attacker.example/ws' \
    $'wss://relay.example\n@attacker.example/ws' \
    $'wss://relay.example\r\n@attacker.example/ws'
  do
    TERMD_RELAY_URLS="$hostile_url"
    if output="$(register_daemon_with_relay \
      '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public"}')"; then
      printf 'relay registration unexpectedly accepted a CR/LF URL\n' >&2
      return 1
    fi
    [[ "$output" == *"relay URL contains CR/LF"* ]]
    [[ "$output" != *"safe-relay-setup-token"* && ! -e "$curl_marker" ]]
    if output="$(verify_daemon_relay_connected '{"server_id":"server-v070"}')"; then
      printf 'relay status unexpectedly accepted a CR/LF URL\n' >&2
      return 1
    fi
    [[ "$output" == *"FAILED: relay URL contains CR/LF"* ]]
    [[ "$output" != *"safe-relay-setup-token"* && ! -e "$curl_marker" ]]
  done
)

test_termd_upgrade_skips_inherited_relay_verification_without_explicit_options() (
  load_termd_installer_functions

  local tmp_dir output register_marker status_marker
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  INSTALL_PREFIX="${tmp_dir}/prefix"
  mkdir -p "${INSTALL_PREFIX}/bin"
  cat >"${INSTALL_PREFIX}/bin/termd" <<'EOF'
#!/usr/bin/env bash
printf 'PAIR_OK\n'
EOF
  chmod 0755 "${INSTALL_PREFIX}/bin/termd"
  printf 'TERMD_RELAY_URLS=%q\n' "wss://existing-relay.example/ws" >"${tmp_dir}/existing.env"
  # shellcheck source=/dev/null
  source "${tmp_dir}/existing.env"
  TERMD_LISTEN="127.0.0.1:8765"
  INSTALL_SET_RELAY_URLS=0
  INSTALL_SET_RELAY_DAEMON_TOKEN_FILE=0
  INSTALL_SET_RELAY_SETUP_TOKEN_FILE=0
  INSTALL_SET_RELAY_SETUP_TOKEN=0
  unset TERMD_RELAY_SETUP_TOKEN TERMD_RELAY_SETUP_TOKEN_FILE
  register_marker="${tmp_dir}/register-called"
  status_marker="${tmp_dir}/status-called"
  register_daemon_with_relay() {
    : >"$register_marker"
    return 1
  }
  verify_daemon_relay_connected() {
    : >"$status_marker"
    return 1
  }
  get_local_healthz() {
    printf '{"server_id":"00000000-0000-0000-0000-000000000004","daemon_public_key":"ed25519-v1:test"}'
  }

  output="$(print_initial_pairing_token)"

  [[ "$output" == *"SKIPPED: existing relay configuration preserved; setup token was not provided for connection verification"* ]]
  [[ "$output" == *"PAIR_OK"* ]]
  [[ "$output" != *"SUCCESS: daemon"* ]]
  [[ ! -e "$register_marker" && ! -e "$status_marker" ]]
)

test_termd_postinstall_health_timeout_is_failure() (
  load_termd_installer_functions

  local output
  INSTALL_PREFIX="/usr/local"
  TERMD_LISTEN="127.0.0.1:8765"
  unset TERMD_RELAY_URLS TERMD_RELAY_SETUP_TOKEN TERMD_RELAY_SETUP_TOKEN_FILE
  get_local_healthz() { return 1; }
  sleep() { :; }

  if output="$(complete_post_install)"; then
    printf 'post-install health timeout unexpectedly succeeded\n' >&2
    return 1
  fi
  [[ "$output" == *"local service installed but post-install verification/pairing failed"* ]]
  [[ "$output" == *"retry with: sudo systemctl restart termd"* ]]
  [[ "$output" == *"then run: /usr/local/bin/termd pair --qr --url http://127.0.0.1:8765"* ]]

  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_SETUP_TOKEN="relay-setup-secret"
  INSTALL_SET_RELAY_URLS=1
  INSTALL_SET_RELAY_SETUP_TOKEN=1
  if output="$(complete_post_install)"; then
    printf 'relay post-install health timeout unexpectedly succeeded\n' >&2
    return 1
  fi
  [[ "$output" == *"the trusted relay setup token will be requested again"* ]]
  [[ "$output" == *"retry with: sudo /usr/local/bin/termd install --relay wss://relay.example/ws"* ]]
  [[ "$output" != *"then run: /usr/local/bin/termd pair"* ]]
  [[ "$output" != *"relay-setup-secret"* ]]
)

test_termd_invalid_pairing_url_retries_full_relay_install() (
  load_termd_installer_functions

  local output
  INSTALL_PREFIX="/usr/local"
  TERMD_LISTEN="invalid-listen"
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_SETUP_TOKEN="relay-setup-secret"
  INSTALL_SET_RELAY_URLS=1
  INSTALL_SET_RELAY_SETUP_TOKEN=1

  if output="$(print_initial_pairing_token)"; then
    printf 'invalid pairing listen unexpectedly succeeded\n' >&2
    return 1
  fi
  [[ "$output" == *"the trusted relay setup token will be requested again"* ]]
  [[ "$output" == *"retry with: sudo /usr/local/bin/termd install --listen 127.0.0.1:8765 --relay wss://relay.example/ws"* ]]
  [[ "$output" != *"relay-setup-secret"* ]]
)

test_termd_postinstall_pair_failure_is_failure() (
  load_termd_installer_functions

  local tmp_dir output
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  INSTALL_PREFIX="${tmp_dir}/prefix"
  TERMD_LISTEN="127.0.0.1:8765"
  unset TERMD_RELAY_URLS TERMD_RELAY_SETUP_TOKEN TERMD_RELAY_SETUP_TOKEN_FILE
  mkdir -p "${INSTALL_PREFIX}/bin"
  cat >"${INSTALL_PREFIX}/bin/termd" <<'EOF'
#!/usr/bin/env bash
exit 42
EOF
  chmod 0755 "${INSTALL_PREFIX}/bin/termd"
  get_local_healthz() { printf '{}'; }

  if output="$(complete_post_install)"; then
    printf 'post-install pair command failure unexpectedly succeeded\n' >&2
    return 1
  fi
  [[ "$output" == *"initial pairing failed; run '${INSTALL_PREFIX}/bin/termd pair --qr --url http://127.0.0.1:8765' manually"* ]]
  [[ "$output" == *"local service installed but post-install verification/pairing failed"* ]]
)

test_termd_sensitive_curl_temp_files_are_removed_on_failure() (
  load_termd_installer_functions

  local tmp_dir daemon_token_file setup_token_file marker sensitive_dir
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  daemon_token_file="${tmp_dir}/daemon-token"
  setup_token_file="${tmp_dir}/setup-token"
  marker="${tmp_dir}/sensitive-dir"
  printf 'daemon-secret\n' >"$daemon_token_file"
  printf 'setup-secret\n' >"$setup_token_file"
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_DAEMON_TOKEN_FILE="$daemon_token_file"
  TERMD_RELAY_SETUP_TOKEN_FILE="$setup_token_file"
  unset TERMD_RELAY_SETUP_TOKEN
  curl() {
    local payload_file
    payload_file="$(assert_secure_relay_curl_invocation \
      "https://relay.example/api/relay/daemon/register" \
      "setup-secret" \
      "$@")" || return 1
    dirname "$payload_file" >"$marker"
    return 22
  }

  if register_daemon_with_relay '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public"}' >/dev/null; then
    printf 'failed relay registration unexpectedly succeeded\n' >&2
    return 1
  fi
  sensitive_dir="$(<"$marker")"
  [[ ! -e "$sensitive_dir" ]]
)

test_termd_sensitive_curl_stops_when_temp_directory_creation_fails() (
  load_termd_installer_functions

  local tmp_dir daemon_token_file setup_token_file output downstream_marker
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  daemon_token_file="${tmp_dir}/daemon-token"
  setup_token_file="${tmp_dir}/setup-token"
  downstream_marker="${tmp_dir}/downstream-called"
  printf 'daemon-secret\n' >"$daemon_token_file"
  printf 'setup-secret\n' >"$setup_token_file"
  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_DAEMON_TOKEN_FILE="$daemon_token_file"
  TERMD_RELAY_SETUP_TOKEN_FILE="$setup_token_file"
  unset TERMD_RELAY_SETUP_TOKEN
  mktemp() { return 1; }
  chmod() {
    : >"$downstream_marker"
    return 0
  }
  curl() {
    : >"$downstream_marker"
    return 0
  }

  if output="$(register_daemon_with_relay '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public"}')"; then
    printf 'relay registration unexpectedly survived mktemp failure\n' >&2
    return 1
  fi
  [[ "$output" == *"failed to create temporary directory for relay registration"* ]]
  [[ ! -e "$downstream_marker" ]]

  if output="$(verify_daemon_relay_connected '{"server_id":"server-v070"}')"; then
    printf 'relay verification unexpectedly survived mktemp failure\n' >&2
    return 1
  fi
  [[ "$output" == *"FAILED: failed to create temporary directory for relay verification"* ]]
  [[ ! -e "$downstream_marker" ]]
)

test_termd_fresh_install_initializes_schema_before_supervisor_baseline() (
  load_termd_installer_functions

  local tmp_dir sqlite_path
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  STATE_DIR="${tmp_dir}/state"
  sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  mkdir -p "$STATE_DIR"
  INSTALL_SET_SUPERVISOR_VERSION=1
  SUPERVISOR_VERSION="test-supervisor-version"
  SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR=0
  chown_state_dir() { :; }

  persist_supervisor_version
  [[ "$SUPERVISOR_VERSION_PERSIST_DEFERRED" -eq 1 && ! -e "$sqlite_path" ]]
  initialize_fake_daemon_schema "$sqlite_path"
  persist_deferred_supervisor_version

  [[ "$(read_sqlite_meta_value "$sqlite_path" state_schema_version)" == 3 ]]
  [[ "$(read_sqlite_meta_value "$sqlite_path" supervisor_version)" == test-supervisor-version ]]
)

test_termd_reinstall_recovers_installer_poisoned_state() (
  load_termd_installer_functions

  local tmp_dir sqlite_path
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  STATE_DIR="${tmp_dir}/state"
  sqlite_path="${STATE_DIR}/daemon-state.sqlite"
  seed_installer_poisoned_state_db "$sqlite_path"
  INSTALL_SET_SUPERVISOR_VERSION=1
  SUPERVISOR_VERSION="test-supervisor-version"
  SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR=0
  chown_state_dir() { :; }

  persist_supervisor_version
  [[ "$SUPERVISOR_VERSION_PERSIST_DEFERRED" -eq 1 && -e "$sqlite_path" ]]
  [[ -z "$(read_sqlite_meta_value "$sqlite_path" supervisor_version)" ]]
  initialize_fake_daemon_schema "$sqlite_path"
  persist_deferred_supervisor_version

  [[ "$(read_sqlite_meta_value "$sqlite_path" state_schema_version)" == 3 ]]
  [[ "$(read_sqlite_meta_value "$sqlite_path" supervisor_version)" == test-supervisor-version ]]
)

test_termd_poisoned_state_repair_never_modifies_user_state() (
  load_termd_installer_functions

  local tmp_dir variant sqlite_path before after socket_path
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT

  for variant in \
    extra-meta trusted_devices runtime_sessions http_uploads daemon_clients \
    daemon_client_attached_sessions daemon_sessions session_ownership unknown-table socket
  do
    STATE_DIR="${tmp_dir}/${variant}"
    sqlite_path="${STATE_DIR}/daemon-state.sqlite"
    mkdir -p "${STATE_DIR}/termd-supervisors"
    seed_installer_poisoned_state_db "$sqlite_path"
    python3 - "$sqlite_path" "$variant" <<'PY'
import sqlite3
import sys

path, variant = sys.argv[1:]
conn = sqlite3.connect(path)
try:
    if variant == "extra-meta":
        conn.execute("INSERT INTO daemon_meta VALUES ('server_id', 'keep-me', 2)")
    elif variant == "unknown-table":
        conn.execute("CREATE TABLE user_notes (marker TEXT)")
        conn.execute("INSERT INTO user_notes VALUES ('keep-me')")
    elif variant != "socket":
        conn.execute(f'INSERT INTO "{variant}" VALUES (\'keep-me\')')
    conn.commit()
finally:
    conn.close()
PY
    if [[ "$variant" == "socket" ]]; then
      socket_path="${STATE_DIR}/termd-supervisors/keep.sock"
      python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()' "$socket_path"
    fi

    before="$(sha256sum "$sqlite_path")"
    if repair_installer_poisoned_state_db "$sqlite_path" "${STATE_DIR}/termd-supervisors"; then
      printf 'unexpectedly repaired state containing %s\n' "$variant" >&2
      return 1
    fi
    after="$(sha256sum "$sqlite_path")"
    [[ "$after" == "$before" ]]
    if [[ "$variant" == "socket" ]]; then
      [[ -S "$socket_path" ]]
    fi
  done
)

seed_installer_poisoned_state_db() {
  local sqlite_path="$1"

  mkdir -p "$(dirname "$sqlite_path")"
  python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
conn.executescript(
    """
    CREATE TABLE daemon_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
    INSERT INTO daemon_meta VALUES ('supervisor_version', 'old-installer-value', 7);
    CREATE TABLE trusted_devices (marker TEXT);
    CREATE TABLE runtime_sessions (marker TEXT);
    CREATE TABLE http_uploads (marker TEXT);
    CREATE TABLE daemon_clients (marker TEXT);
    CREATE TABLE daemon_client_attached_sessions (marker TEXT);
    CREATE TABLE daemon_sessions (marker TEXT);
    CREATE TABLE session_ownership (phase TEXT);
    """
)
conn.commit()
conn.close()
PY
}

initialize_fake_daemon_schema() {
  local sqlite_path="$1"

  python3 - "$sqlite_path" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
conn.execute("CREATE TABLE IF NOT EXISTS daemon_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at_ms INTEGER NOT NULL)")
values = dict(conn.execute("SELECT key, value FROM daemon_meta"))
assert values == {}, values
conn.execute("INSERT INTO daemon_meta VALUES ('state_schema_version', '3', 8)")
conn.commit()
conn.close()
PY
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

test_termd_relay_registration_uses_only_daemon_token_and_public_key() (
  load_termd_installer_functions

  local tmp_dir daemon_token_file setup_token_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  daemon_token_file="$tmp_dir/daemon-token"
  setup_token_file="$tmp_dir/setup-token"
  printf 'daemon-secret\n' >"$daemon_token_file"
  printf 'setup-secret\n' >"$setup_token_file"
  chmod 0600 "$daemon_token_file" "$setup_token_file"

  TERMD_RELAY_URLS="wss://relay.example/ws"
  TERMD_RELAY_DAEMON_TOKEN_FILE="$daemon_token_file"
  TERMD_RELAY_SETUP_TOKEN_FILE="$setup_token_file"

  curl() {
    local payload_file
    payload_file="$(assert_secure_relay_curl_invocation \
      "https://relay.example/api/relay/daemon/register" \
      "setup-secret" \
      "$@")" || return 1
    python3 - "$payload_file" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as payload_file:
    payload = json.load(payload_file)
assert payload == {
    "server_id": "server-v070",
    "daemon_token": "daemon-secret",
    "daemon_public_key": "ed25519-v1:daemon-public",
}, payload
PY
  }

  register_daemon_with_relay \
    '{"server_id":"server-v070","daemon_public_key":"ed25519-v1:daemon-public","pair_ticket":"must-not-register","device_certificate":"must-not-register","access_token":"must-not-register"}' \
    >/dev/null
)

previous_supervisor_version_from_file() {
  local version_file="$1"
  local repo_version

  repo_version="$(tr -d '\n' <"$version_file")"
  printf '%s-previous\n' "$repo_version"
}

install_fake_termd_system_commands() {
  SYSTEMCTL_CALLS=()
  INSTALL_EVENTS=()
  TERMD_FAKE_SERVICE_ACTIVE=1

  eval "$(declare -f persist_supervisor_version | sed '1s/persist_supervisor_version/real_persist_supervisor_version/')"
  persist_supervisor_version() {
    # Default-path fixtures verify generated configuration and must not write host service state.
    [[ "$STATE_DIR" == "/var/lib/termd" ]] && return 0
    real_persist_supervisor_version
  }

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
  resolve_version() { VERSION="1970-01-01"; }
  install_from_release() {
    local destination="$1"
    mkdir -p "$(dirname "$destination")"
    printf '#!/usr/bin/env bash\n' >"$destination"
    chmod +x "$destination"
  }
  install_from_source() { return 1; }
  ensure_system_user() { :; }
  chown() { :; }
  chmod() { :; }
  systemctl() {
    SYSTEMCTL_CALLS+=("$*")
    INSTALL_EVENTS+=("systemctl:$*")
    case "${1:-}" in
      is-active)
        [[ "${TERMD_FAKE_SERVICE_ACTIVE}" -eq 1 ]]
        ;;
      stop)
        TERMD_FAKE_SERVICE_ACTIVE=0
        ;;
      start|restart)
        TERMD_FAKE_SERVICE_ACTIVE=1
        if [[ "${SUPERVISOR_VERSION_PERSIST_DEFERRED:-0}" -eq 1 && "$STATE_DIR" != "/var/lib/termd" ]]; then
          mkdir -p "$STATE_DIR"
          python3 - "${STATE_DIR}/daemon-state.sqlite" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
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
    conn.execute(
        """
        INSERT INTO daemon_meta (key, value, updated_at_ms)
        VALUES ('state_schema_version', '3', 1)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        """
    )
    conn.commit()
finally:
    conn.close()
PY
        fi
        ;;
      *)
        :
        ;;
    esac
  }
  print_initial_pairing_token() { :; }
}

install_fake_supervisor_termination_tracker() {
  TERMINATED_SUPERVISOR_DIRS=()

  terminate_session_supervisors() {
    TERMINATED_SUPERVISOR_DIRS+=("$1")
    INSTALL_EVENTS+=("terminate-supervisors:$1")
  }
}

seed_termd_runtime_sqlite() {
  local sqlite_file="$1"
  local supervisor_version="$2"

  # GitHub Actions 上偶发会在 Python 连接 SQLite 时看见父目录尚未就绪。
  # 先显式补目录，避免测试只因为临时路径状态而失败。
  mkdir -p "$(dirname "$sqlite_file")"
  touch "$sqlite_file"
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
conn.execute("INSERT INTO daemon_meta VALUES ('state_schema_version', '3', 1)")
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

  mkdir -p "$(dirname "$sqlite_file")"
  touch "$sqlite_file"
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
INSERT INTO daemon_meta VALUES ('state_schema_version', '3', 1);
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
  local fixture_state_dir="${unit_file%.service}-state"
  local canonical_state_dir canonical_default_state_dir
  shift

  if ! canonical_state_dir="$(realpath -m -- "$fixture_state_dir")" ||
    ! canonical_default_state_dir="$(realpath -m -- /var/lib/termd)" ||
    [[ -z "$canonical_state_dir" || -z "$canonical_default_state_dir" ]]; then
    printf 'failed to validate fake termd state path\n' >&2
    return 1
  fi
  case "${canonical_state_dir}/" in
    "${canonical_default_state_dir}/"*)
      printf 'refusing fake termd state path under %s\n' "$canonical_default_state_dir" >&2
      return 1
      ;;
  esac

  local TERMD_STATE_DIR="$fixture_state_dir"
  export TERMD_STATE_DIR
  REPO="example/termd"
  VERSION=""
  UNIT_FILE="$unit_file"
  ENV_FILE="${unit_file%.service}.env"
  ENV_DIR="$(dirname "$ENV_FILE")"
  WRAPPER_FILE="${unit_file%.service}-run"
  WRAPPER_DIR="$(dirname "$WRAPPER_FILE")"
  INSTALL_PREFIX="${unit_file%.service}-prefix"
  STATE_DIR="$TERMD_STATE_DIR"
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
  assert_file_contains "$unit_file" "WorkingDirectory=${unit_file%.service}-state"
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

  TERMD_STATE_DIR="/var/lib/termd/"
  export TERMD_STATE_DIR
  run_fake_termd_install "$unit_file"
  unset TERMD_STATE_DIR

  assert_file_contains "$unit_file" "User=alice"
  assert_file_contains "$unit_file" "Group=deploy"
  assert_file_contains "$unit_file" "WorkingDirectory=${unit_file%.service}-state"
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
  assert_file_contains "$unit_file" "WorkingDirectory=${unit_file%.service}-state"
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
  assert_file_contains "$unit_file" "WorkingDirectory=${unit_file%.service}-state"
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

  [[ "$http_proxy" == "http://127.0.0.1:3128" ]]
  [[ "$https_proxy" == "http://127.0.0.1:3128" ]]
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

  mkdir -p "$(dirname "$sqlite_file")"
  touch "$sqlite_file"
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

  local tmp_dir unit_file sqlite_file socket_file supervisor_version
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  supervisor_version="$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")"
  seed_termd_runtime_sqlite "$sqlite_file" "$supervisor_version"
  create_stale_supervisor_socket "$socket_file"

  SUPERVISOR_VERSION="$supervisor_version"
  run_fake_termd_install "$unit_file" >/dev/null
  unset SUPERVISOR_VERSION

  python3 - "$sqlite_file" "$socket_file" "$ROOT_DIR/SUPERVISOR_VERSION" <<'PY'
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
    assert version == pathlib.Path(sys.argv[3]).read_text().strip(), version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_default_supervisor_version_uses_repository_version_file() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  STATE_DIR="${tmp_dir}/termd"

  resolve_supervisor_version

  local supervisor_version
  supervisor_version="$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")"
  [[ "$SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR" -eq 0 ]]
  [[ "$SUPERVISOR_VERSION" == "$supervisor_version" ]]
)

test_termd_baked_supervisor_default_keeps_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file baked_supervisor_version
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite_without_supervisor_version "$sqlite_file"
  create_stale_supervisor_socket "$socket_file"

  # 这里模拟一个旧的、已经烘进脚本里的 supervisor 默认值。
  # 即使数据库里还没有 supervisor_version 元数据，普通二进制更新也不能清掉已有 runtime session。
  baked_supervisor_version="$(previous_supervisor_version_from_file "${ROOT_DIR}/SUPERVISOR_VERSION")"
  SUPERVISOR_VERSION="$baked_supervisor_version"
  export TERMD_INSTALL_CONFIRM_FD=0
  run_fake_termd_install "$unit_file" <<<"y" >/dev/null
  unset SUPERVISOR_VERSION TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" "$baked_supervisor_version" <<'PY'
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
    assert version == sys.argv[3], version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_required_supervisor_version_mismatch_prompts_and_clears_runtime_state() (
  load_termd_installer_functions_with_required_supervisor_version "$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")"
  install_fake_termd_system_commands
  install_fake_supervisor_termination_tracker

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  run_fake_termd_install "$unit_file" --allow-session-loss </dev/null >/dev/null

  python3 - "$sqlite_file" "$socket_file" "$ROOT_DIR/SUPERVISOR_VERSION" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    attached = conn.execute("SELECT COUNT(*) FROM daemon_client_attached_sessions").fetchone()[0]
    assert attached == 0, attached
    daemon_sessions = conn.execute("SELECT COUNT(*) FROM daemon_sessions").fetchone()[0]
    assert daemon_sessions == 0, daemon_sessions
    runtime_sessions = conn.execute("SELECT COUNT(*) FROM runtime_sessions").fetchone()[0]
    assert runtime_sessions == 0, runtime_sessions
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == pathlib.Path(sys.argv[3]).read_text().strip(), version
finally:
    conn.close()
assert not pathlib.Path(sys.argv[2]).exists()
PY
  [[ "${#TERMINATED_SUPERVISOR_DIRS[@]}" -eq 1 ]]
  [[ "${TERMINATED_SUPERVISOR_DIRS[0]}" == "${STATE_DIR}/termd-supervisors" ]]
)

test_termd_missing_supervisor_meta_keeps_runtime_state_on_default_update() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file supervisor_version
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite_without_supervisor_version "$sqlite_file"
  create_stale_supervisor_socket "$socket_file"

  # 旧版本可能还没有 supervisor_version 元数据；默认更新只能补 baseline，
  # 不能把已有 session 当成需要清理的旧 runtime。
  supervisor_version="$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")"
  export TERMD_INSTALL_CONFIRM_FD=0
  run_fake_termd_install "$unit_file" <<<"y" >/dev/null
  unset TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" "$supervisor_version" <<'PY'
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
    assert version == sys.argv[3], version
finally:
    conn.close()
assert pathlib.Path(sys.argv[2]).exists()
PY
)

test_termd_supervisor_version_mismatch_prompts_and_clears_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands
  install_fake_supervisor_termination_tracker

  local tmp_dir unit_file sqlite_file socket_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  export TERMD_INSTALL_CONFIRM_FD=0
  run_fake_termd_install "$unit_file" --supervisor-version v-new <<<"y" >/dev/null
  unset TERMD_INSTALL_CONFIRM_FD

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    attached = conn.execute("SELECT COUNT(*) FROM daemon_client_attached_sessions").fetchone()[0]
    assert attached == 0, attached
    daemon_sessions = conn.execute("SELECT COUNT(*) FROM daemon_sessions").fetchone()[0]
    assert daemon_sessions == 0, daemon_sessions
    runtime_sessions = conn.execute("SELECT COUNT(*) FROM runtime_sessions").fetchone()[0]
    assert runtime_sessions == 0, runtime_sessions
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-new", version
    assert conn.execute("SELECT COUNT(*) FROM daemon_meta").fetchone()[0] == 3
    assert conn.execute("SELECT COUNT(*) FROM trusted_devices").fetchone()[0] == 1
    assert conn.execute("SELECT COUNT(*) FROM daemon_clients").fetchone()[0] == 1
finally:
    conn.close()
assert not pathlib.Path(sys.argv[2]).exists()
PY
  [[ "${#TERMINATED_SUPERVISOR_DIRS[@]}" -eq 1 ]]
  [[ "${TERMINATED_SUPERVISOR_DIRS[0]}" == "${STATE_DIR}/termd-supervisors" ]]
  [[ "${INSTALL_EVENTS[*]}" == *"systemctl:stop termd"* ]]
  [[ "${INSTALL_EVENTS[*]}" == *"systemctl:restart termd"* ]]
  python3 - "${INSTALL_EVENTS[@]}" <<'PY'
import sys

events = sys.argv[1:]
stop_index = events.index("systemctl:stop termd")
terminate_index = next(
    index for index, event in enumerate(events) if event.startswith("terminate-supervisors:")
)
restart_index = events.index("systemctl:restart termd")
assert stop_index < terminate_index < restart_index, events
PY
)

test_termd_supervisor_version_mismatch_decline_preserves_runtime_state() (
  load_termd_installer_functions
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file status
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  export TERMD_INSTALL_CONFIRM_FD=0
  set +e
  (run_fake_termd_install "$unit_file" --supervisor-version v-new <<<"n" >/dev/null 2>/dev/null)
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

test_termd_required_supervisor_version_mismatch_decline_preserves_runtime_state() (
  load_termd_installer_functions_with_required_supervisor_version "$(tr -d '\n' <"${ROOT_DIR}/SUPERVISOR_VERSION")"
  install_fake_termd_system_commands

  local tmp_dir unit_file sqlite_file socket_file status
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  unit_file="${tmp_dir}/termd.service"
  TERMD_STATE_DIR="${unit_file%.service}-state"
  export TERMD_STATE_DIR
  STATE_DIR="${TERMD_STATE_DIR}"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="${STATE_DIR}/daemon-state.sqlite"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"

  # The outer binary's --yes must not turn into session-loss authorization.
  # Keep the old internal marker hostile to prove it has no remaining semantics.
  TERMD_INSTALL_ASSUME_YES=1
  export TERMD_INSTALL_ASSUME_YES TERMD_INSTALL_CONFIRM_FD=0
  set +e
  (run_fake_termd_install "$unit_file" </dev/null >/dev/null 2>/dev/null)
  status=$?
  set -e
  unset TERMD_INSTALL_ASSUME_YES TERMD_INSTALL_CONFIRM_FD

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

test_update_local_supervisor_version_mismatch_clears_runtime_state() (
  load_update_local_functions
  INSTALL_EVENTS=()
  install_fake_supervisor_termination_tracker

  local tmp_dir sqlite_file socket_file version_file
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  STATE_DIR="${tmp_dir}/termd"
  STATE_DB="${STATE_DIR}/daemon-state.sqlite"
  mkdir -p "${STATE_DIR}/termd-supervisors"
  sqlite_file="$STATE_DB"
  socket_file="${STATE_DIR}/termd-supervisors/stale.sock"
  version_file="${tmp_dir}/SUPERVISOR_VERSION"
  seed_termd_runtime_sqlite "$sqlite_file" "v-old"
  create_stale_supervisor_socket "$socket_file"
  printf 'v-new\n' >"$version_file"
  TERMD_SUPERVISOR_VERSION_FILE="$version_file"

  resolve_local_supervisor_version
  [[ "$SUPERVISOR_VERSION_NEEDS_RUNTIME_CLEAR" -eq 1 ]]
  [[ "$SUPERVISOR_VERSION_TARGET" == "v-new" ]]
  clear_runtime_session_state_for_supervisor_upgrade "$STATE_DB" "${STATE_DIR}/termd-supervisors"
  persist_local_supervisor_version

  python3 - "$sqlite_file" "$socket_file" <<'PY'
import pathlib
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
try:
    attached = conn.execute("SELECT COUNT(*) FROM daemon_client_attached_sessions").fetchone()[0]
    assert attached == 0, attached
    daemon_sessions = conn.execute("SELECT COUNT(*) FROM daemon_sessions").fetchone()[0]
    assert daemon_sessions == 0, daemon_sessions
    runtime_sessions = conn.execute("SELECT COUNT(*) FROM runtime_sessions").fetchone()[0]
    assert runtime_sessions == 0, runtime_sessions
    version = conn.execute(
        "SELECT value FROM daemon_meta WHERE key = 'supervisor_version'"
    ).fetchone()[0]
    assert version == "v-new", version
finally:
    conn.close()
assert not pathlib.Path(sys.argv[2]).exists()
PY
  [[ "${#TERMINATED_SUPERVISOR_DIRS[@]}" -eq 1 ]]
  [[ "${TERMINATED_SUPERVISOR_DIRS[0]}" == "${STATE_DIR}/termd-supervisors" ]]
)

source_installer_fixture_setup() {
  SOURCE_INSTALLER_FIXTURE_ROOT="$(mktemp -d)"
  SOURCE_INSTALLER_FIXTURE_REPO="${SOURCE_INSTALLER_FIXTURE_ROOT}/repo"
  SOURCE_INSTALLER_FIXTURE_BIN="${SOURCE_INSTALLER_FIXTURE_ROOT}/bin"
  SOURCE_INSTALLER_FIXTURE_PREFIX="${SOURCE_INSTALLER_FIXTURE_ROOT}/prefix"
  SOURCE_INSTALLER_FIXTURE_NPM_CALLS="${SOURCE_INSTALLER_FIXTURE_ROOT}/npm-calls"
  SOURCE_INSTALLER_FIXTURE_CARGO_CALLS="${SOURCE_INSTALLER_FIXTURE_ROOT}/cargo-calls"
  SOURCE_INSTALLER_FIXTURE_SERVICE_STATE="${SOURCE_INSTALLER_FIXTURE_ROOT}/service-state"
  export SOURCE_INSTALLER_FIXTURE_REPO SOURCE_INSTALLER_FIXTURE_NPM_CALLS
  export SOURCE_INSTALLER_FIXTURE_CARGO_CALLS

  mkdir -p "${SOURCE_INSTALLER_FIXTURE_REPO}/termui/frontend/src" \
    "${SOURCE_INSTALLER_FIXTURE_BIN}" "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin"
  printf '{"name":"installer-fixture","version":"1.0.0"}\n' \
    >"${SOURCE_INSTALLER_FIXTURE_REPO}/termui/frontend/package.json"
  printf '{"name":"installer-fixture","lockfileVersion":3}\n' \
    >"${SOURCE_INSTALLER_FIXTURE_REPO}/termui/frontend/package-lock.json"
  printf 'fixture frontend source\n' \
    >"${SOURCE_INSTALLER_FIXTURE_REPO}/termui/frontend/src/main.ts"
  printf 'running:previous-binary\n' >"$SOURCE_INSTALLER_FIXTURE_SERVICE_STATE"

  cat >"${SOURCE_INSTALLER_FIXTURE_BIN}/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == "clone" ]] || exit 1
dest="${!#}"
mkdir -p "$dest"
cp -a "${SOURCE_INSTALLER_FIXTURE_REPO}/." "$dest/"
EOF
  chmod +x "${SOURCE_INSTALLER_FIXTURE_BIN}/git"

  cat >"${SOURCE_INSTALLER_FIXTURE_BIN}/node" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "${SOURCE_INSTALLER_FIXTURE_BIN}/node"

  cat >"${SOURCE_INSTALLER_FIXTURE_BIN}/npm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${SOURCE_INSTALLER_FIXTURE_NPM_CALLS}"
if [[ "$*" == "ci" && "${SOURCE_INSTALLER_FIXTURE_NPM_MODE:-success}" == "fail-ci-then-build-success" ]]; then
  printf 'fixture npm ci partial failure\n' >&2
  exit 41
fi
if [[ "$*" == "run build" ]]; then
  if [[ "${SOURCE_INSTALLER_FIXTURE_NPM_MODE:-success}" == "fail-build" ]]; then
    printf 'fixture npm build failure\n' >&2
    exit 42
  fi
  mkdir -p dist
  printf '<!doctype html><title>fixture-real-web-ui</title>\n' >dist/index.html
  if [[ "${SOURCE_INSTALLER_FIXTURE_NPM_MODE:-success}" == "build-index-then-fail" ]]; then
    printf 'fixture npm build failed after writing index\n' >&2
    exit 43
  fi
fi
EOF
  chmod +x "${SOURCE_INSTALLER_FIXTURE_BIN}/npm"

  cat >"${SOURCE_INSTALLER_FIXTURE_BIN}/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${SOURCE_INSTALLER_FIXTURE_CARGO_CALLS}"
bin_name=""
while (($#)); do
  if [[ "$1" == "--bin" ]]; then
    bin_name="${2:?}"
    break
  fi
  shift
done
[[ -n "$bin_name" ]]
mkdir -p target/release
if [[ -f termui/frontend/dist/index.html ]]; then
  embedded="$(<termui/frontend/dist/index.html)"
else
  embedded='这个二进制没有嵌入已构建的 Web UI'
fi
{
  printf '#!/usr/bin/env bash\n'
  printf 'printf "%%s\\n" %q\n' "$embedded"
} >"target/release/${bin_name}"
chmod +x "target/release/${bin_name}"
EOF
  chmod +x "${SOURCE_INSTALLER_FIXTURE_BIN}/cargo"
}

source_installer_fixture_teardown() {
  unset SOURCE_INSTALLER_FIXTURE_NPM_MODE SOURCE_INSTALLER_FIXTURE_INSTALL_MODE
  rm -rf "$SOURCE_INSTALLER_FIXTURE_ROOT"
}

run_source_installer_fixture() (
  local component="$1"

  case "$component" in
    termd)
      load_termd_installer_functions
      TERMD_WEB_ENABLED=1
      ;;
    termrelay)
      load_termrelay_installer_functions
      TERMRELAY_WEB_ENABLED=1
      ;;
    *)
      return 1
      ;;
  esac

  COMPONENT="$component"
  BIN_NAME="$component"
  INSTALL_PREFIX="$SOURCE_INSTALLER_FIXTURE_PREFIX"
  INSTALL_SET_WEB=1
  REPO="fixture/termd"
  VERSION="fixture-version"
  PATH="${SOURCE_INSTALLER_FIXTURE_BIN}:/usr/bin:/bin"
  install_from_source >/dev/null
)

run_source_installer_main_fixture() (
  local component="$1"
  local install_mode="$2"
  local test_owner test_group
  shift 2

  case "$component" in
    termd)
      load_termd_installer_functions
      ;;
    termrelay)
      load_termrelay_installer_functions
      ;;
    *)
      return 1
      ;;
  esac

  COMPONENT="$component"
  BIN_NAME="$component"
  SERVICE_NAME="$component"
  INSTALL_PREFIX="$SOURCE_INSTALLER_FIXTURE_PREFIX"
  ENV_DIR="${SOURCE_INSTALLER_FIXTURE_ROOT}/etc"
  ENV_FILE="${ENV_DIR}/${component}.env"
  STATE_DIR="${SOURCE_INSTALLER_FIXTURE_ROOT}/state/${component}"
  UNIT_FILE="${SOURCE_INSTALLER_FIXTURE_ROOT}/${component}.service"
  WRAPPER_DIR="${SOURCE_INSTALLER_FIXTURE_ROOT}/lib"
  WRAPPER_FILE="${WRAPPER_DIR}/${component}-run"
  REPO="fixture/termd"
  VERSION="fixture-version"
  PATH="${SOURCE_INSTALLER_FIXTURE_BIN}:/usr/sbin:/usr/bin:/bin"
  test_owner="$(id -u)"
  test_group="$(id -g)"

  eval "$(declare -f commit_install_file | sed '1s/commit_install_file/real_commit_install_file/')"
  commit_install_file() {
    real_commit_install_file "$1" "$2" "$3" "$4" "$test_owner" "$test_group"
  }

  require_root() { :; }
  inherit_existing_service_identity() { :; }
  resolve_version() { :; }
  resolve_service_identity() { :; }
  resolve_supervisor_version() { :; }
  install_from_release() {
    [[ "$install_mode" == "release" ]] || return 1
    local destination="${1:-${INSTALL_PREFIX}/bin/${component}}"
    printf '#!/usr/bin/env bash\nprintf "release-archive-%s\\n"\n' "$component" \
      >"$destination"
    chmod +x "$destination"
  }
  ensure_system_user() { :; }
  stop_service_before_supervisor_runtime_clear() { :; }
  clear_session_state_after_state_dir_change() { :; }
  chown_state_dir() { :; }
  persist_supervisor_version() { :; }
  prepare_runtime_support_files() { :; }
  write_env_file() {
    mkdir -p "$(dirname "$ENV_FILE")"
    if [[ ! -e "$ENV_FILE" ]]; then
      case "$component" in
        termd) printf 'TERMD_WEB_ENABLED=%q\n' "${TERMD_WEB_ENABLED:-0}" >"$ENV_FILE" ;;
        termrelay) printf 'TERMRELAY_WEB_ENABLED=%q\n' "${TERMRELAY_WEB_ENABLED:-0}" >"$ENV_FILE" ;;
      esac
    fi
  }
  write_wrapper() {
    [[ "$component" == "termrelay" ]] || return 0
    local destination="${1:-$WRAPPER_FILE}"
    mkdir -p "$(dirname "$destination")"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$destination"
    chmod 0755 "$destination"
  }
  write_unit() {
    [[ "$component" == "termrelay" ]] || return 0
    local destination="${1:-$UNIT_FILE}"
    local runtime_wrapper="${2:-$WRAPPER_FILE}"
    mkdir -p "$(dirname "$destination")"
    printf '[Service]\nExecStart=%s\n' "$runtime_wrapper" >"$destination"
    chmod 0644 "$destination"
  }
  print_initial_pairing_token() { :; }
  if [[ "$component" == "termrelay" ]]; then
    SERVICE_NAME=root
    verify_service_healthy() { systemctl is-active --quiet "$SERVICE_NAME"; }
    print_relay_setup_token() { :; }
  fi
  systemctl() {
    printf '%s\n' "$*" >>"${SOURCE_INSTALLER_FIXTURE_ROOT}/systemctl-calls"
    if [[ "${1:-}" == "restart" ]]; then
      printf 'running:new-binary\n' >"$SOURCE_INSTALLER_FIXTURE_SERVICE_STATE"
    fi
  }

  if [[ "$install_mode" == "source" ]]; then
    install_from_release() { return 1; }
  fi
  main "$@" >/dev/null
)

assert_source_install_embeds_real_web_ui() (
  local component="$1"
  local output

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  [[ ! -e "${SOURCE_INSTALLER_FIXTURE_REPO}/termui/frontend/dist" ]]

  run_source_installer_fixture "$component"

  output="$("${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")"
  [[ "$output" == *"fixture-real-web-ui"* ]]
  [[ "$output" != *"这个二进制没有嵌入已构建的 Web UI"* ]]
  assert_file_contains "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" "ci"
  assert_file_contains "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" "run build"
)

test_termd_clean_source_install_embeds_real_web_ui() (
  assert_source_install_embeds_real_web_ui termd
)

test_termrelay_clean_source_install_embeds_real_web_ui() (
  assert_source_install_embeds_real_web_ui termrelay
)

assert_source_web_failure_preserves_installed_binary() (
  local component="$1"
  local failure_mode="$2"
  local output old_checksum new_checksum

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  printf 'previous-%s-binary\n' "$component" \
    >"${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}"
  chmod +x "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}"
  old_checksum="$(sha256sum "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")"

  case "$failure_mode" in
    missing-npm)
      rm -f "${SOURCE_INSTALLER_FIXTURE_BIN}/npm"
      ;;
    fail-build)
      export SOURCE_INSTALLER_FIXTURE_NPM_MODE=fail-build
      ;;
    *)
      return 1
      ;;
  esac

  if output="$(run_source_installer_fixture "$component" 2>&1)"; then
    printf 'expected %s source install to fail in %s mode\n' "$component" "$failure_mode" >&2
    exit 1
  fi

  new_checksum="$(sha256sum "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")"
  [[ "$new_checksum" == "$old_checksum" ]]
  case "$failure_mode" in
    missing-npm)
      [[ "$output" == *"missing required command: npm"* ]]
      ;;
    fail-build)
      [[ "$output" == *"failed to build Web UI"* ]]
      [[ ! -e "$SOURCE_INSTALLER_FIXTURE_CARGO_CALLS" ]]
      ;;
  esac
)

test_source_web_install_missing_npm_preserves_installed_binaries() (
  assert_source_web_failure_preserves_installed_binary termd missing-npm
  assert_source_web_failure_preserves_installed_binary termrelay missing-npm
)

test_source_web_build_failure_preserves_installed_binaries() (
  assert_source_web_failure_preserves_installed_binary termd fail-build
  assert_source_web_failure_preserves_installed_binary termrelay fail-build
)

assert_source_web_partial_failure_preserves_install() (
  local component="$1"
  local failure_mode="$2"
  local output old_binary_checksum old_service_state

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  printf 'previous-%s-binary\n' "$component" \
    >"${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}"
  chmod +x "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}"
  old_binary_checksum="$(sha256sum "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")"
  old_service_state="$(<"$SOURCE_INSTALLER_FIXTURE_SERVICE_STATE")"
  export SOURCE_INSTALLER_FIXTURE_NPM_MODE="$failure_mode"

  if output="$(run_source_installer_main_fixture "$component" source --web 2>&1)"; then
    printf 'expected %s source install to fail in %s mode\n' "$component" "$failure_mode" >&2
    exit 1
  fi

  [[ "$(sha256sum "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")" == "$old_binary_checksum" ]]
  [[ "$(<"$SOURCE_INSTALLER_FIXTURE_SERVICE_STATE")" == "$old_service_state" ]]
  [[ "$output" == *"failed to build Web UI"* ]]
  [[ ! -e "$SOURCE_INSTALLER_FIXTURE_CARGO_CALLS" ]]
)

test_source_web_partial_failures_preserve_binary_and_service() (
  local component failure_mode
  for component in termd termrelay; do
    for failure_mode in build-index-then-fail fail-ci-then-build-success; do
      assert_source_web_partial_failure_preserves_install "$component" "$failure_mode"
    done
  done
)

assert_release_web_path_skips_node_and_npm() (
  local component="$1"

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  rm -f "${SOURCE_INSTALLER_FIXTURE_BIN}/node" "${SOURCE_INSTALLER_FIXTURE_BIN}/npm"

  run_source_installer_main_fixture "$component" release --web

  [[ "$("${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}")" == "release-archive-${component}" ]]
  [[ ! -e "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" ]]
)

assert_no_web_source_path_skips_node_and_npm() (
  local component="$1"

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  rm -f "${SOURCE_INSTALLER_FIXTURE_BIN}/node" "${SOURCE_INSTALLER_FIXTURE_BIN}/npm"

  run_source_installer_main_fixture "$component" source --no-web

  [[ -x "${SOURCE_INSTALLER_FIXTURE_PREFIX}/bin/${component}" ]]
  [[ ! -e "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" ]]
)

assert_existing_web_env_reinstall_invokes_npm() (
  local component="$1"

  source_installer_fixture_setup
  trap source_installer_fixture_teardown EXIT
  mkdir -p "${SOURCE_INSTALLER_FIXTURE_ROOT}/etc"
  case "$component" in
    termd) printf 'TERMD_WEB_ENABLED=1\n' >"${SOURCE_INSTALLER_FIXTURE_ROOT}/etc/${component}.env" ;;
    termrelay) printf 'TERMRELAY_WEB_ENABLED=1\n' >"${SOURCE_INSTALLER_FIXTURE_ROOT}/etc/${component}.env" ;;
  esac

  run_source_installer_main_fixture "$component" source

  assert_file_contains "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" "ci"
  assert_file_contains "$SOURCE_INSTALLER_FIXTURE_NPM_CALLS" "run build"
)

test_installer_web_source_selection_matrix() (
  local component
  for component in termd termrelay; do
    assert_release_web_path_skips_node_and_npm "$component"
    assert_no_web_source_path_skips_node_and_npm "$component"
    assert_existing_web_env_reinstall_invokes_npm "$component"
  done
)

test_installers_reject_non_root_before_install() (
  local output script

  for script in scripts/install-termd.sh scripts/install-termrelay.sh; do
    if [[ "$EUID" -eq 0 ]]; then
      command -v runuser >/dev/null 2>&1
      if output="$(runuser -u nobody -- bash -s -- --web <"${ROOT_DIR}/${script}" 2>&1)"; then
        printf 'expected %s to reject a non-root install\n' "$script" >&2
        exit 1
      fi
    else
      if output="$(bash "${ROOT_DIR}/${script}" --web 2>&1)"; then
        printf 'expected %s to reject a non-root install\n' "$script" >&2
        exit 1
      fi
    fi
    [[ "$output" == *"please run this installer with sudo/root"* ]]
  done
)

snapshot_prepare_release_caller() {
  local repo_dir="$1"
  local path
  shift

  for path in "$@"; do
    printf '%s ' "$path"
    if [[ -e "${repo_dir}/${path}" || -L "${repo_dir}/${path}" ]]; then
      git -C "$repo_dir" hash-object -- "$path"
    else
      printf 'MISSING\n'
    fi
  done
  printf '%s\n' '-- index --'
  git -C "$repo_dir" ls-files --stage -- "$@"
}

assert_prepare_release_snapshot_unchanged() {
  local description="$1"
  local before="$2"
  local after="$3"

  if [[ "$after" != "$before" ]]; then
    printf '%s changed caller state:\nbefore:\n%s\nafter:\n%s\n' "$description" "$before" "$after" >&2
    exit 1
  fi
}

assert_ref_missing() {
  local repo_dir="$1"
  local ref="$2"

  if git -C "$repo_dir" rev-parse --verify "$ref" >/dev/null 2>&1; then
    printf 'unexpected ref exists: %s\n' "$ref" >&2
    exit 1
  fi
}

prepare_release_fixture_setup() {
  PREPARE_FIXTURE_ROOT="$(mktemp -d)"
  PREPARE_REMOTE="${PREPARE_FIXTURE_ROOT}/origin.git"
  PREPARE_REPO="${PREPARE_FIXTURE_ROOT}/repo"
  PREPARE_BIN="${PREPARE_FIXTURE_ROOT}/bin"
  PREPARE_REAL_GIT="$(command -v git)"
  PREPARE_REAL_RM="$(command -v rm)"
  PREPARE_RELEASE_GENERATE_LOCKFILE_CALLED_FILE="${PREPARE_FIXTURE_ROOT}/generate-lockfile-called"
  export PREPARE_RELEASE_REAL_GIT="$PREPARE_REAL_GIT"
  export PREPARE_RELEASE_REAL_RM="$PREPARE_REAL_RM"
  export PREPARE_RELEASE_GENERATE_LOCKFILE_CALLED_FILE

  mkdir -p "${PREPARE_REPO}/scripts" "${PREPARE_REPO}/termui/frontend" \
    "${PREPARE_REPO}/docs/releases" "${PREPARE_REPO}/fixture-core" \
    "${PREPARE_REPO}/fixture-cli" "$PREPARE_BIN"
  git -c init.defaultBranch=main init --bare "$PREPARE_REMOTE" >/dev/null
  git -C "$PREPARE_REPO" -c init.defaultBranch=main init >/dev/null
  git -C "$PREPARE_REPO" config user.name "installer tests"
  git -C "$PREPARE_REPO" config user.email "installer-tests@example.invalid"
  git -C "$PREPARE_REPO" remote add origin "$PREPARE_REMOTE"

  cp "${ROOT_DIR}/scripts/prepare-release.sh" "${PREPARE_REPO}/scripts/prepare-release.sh"
  cat >"${PREPARE_REPO}/scripts/release-notes.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
version="${1:?}"
notes_file="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/docs/releases/${version}.md"
if [[ -f "$notes_file" ]]; then
  cat "$notes_file"
else
  printf 'termd %s\n\n用户可见变化:\n' "$version"
  printf '%s\n' '- 请在 scripts/release-notes.sh 中补充此版本的功能、修复和兼容性说明。'
fi
EOF
  chmod +x "${PREPARE_REPO}/scripts/release-notes.sh"

  cat >"${PREPARE_REPO}/scripts/qa.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${PREPARE_RELEASE_FIXTURE_QA_MODE:-}" in
  fail-status-42)
    printf 'fixture QA failure with status 42\n' >&2
    exit 42
    ;;
  mutate-caller-fail)
    caller="${PREPARE_RELEASE_CALLER_REPO:?}"
    printf '# concurrent caller worktree write\n' >>"${caller}/Cargo.toml"
    blob="$(printf 'concurrent caller index write\n' | git -C "$caller" hash-object -w --stdin)"
    git -C "$caller" update-index --add --cacheinfo 100644 "$blob" Cargo.lock
    printf 'fixture concurrent caller mutation\n' >&2
    exit 1
    ;;
  stage-caller-success)
    caller="${PREPARE_RELEASE_CALLER_REPO:?}"
    printf 'concurrent staged caller write\n' >>"${caller}/README.md"
    git -C "$caller" add -- README.md
    printf 'fixture staged caller mutation\n' >&2
    ;;
esac
EOF
  chmod +x "${PREPARE_REPO}/scripts/qa.sh"

  cat >"${PREPARE_REPO}/Cargo.toml" <<'EOF'
[workspace]
members = ["fixture-core", "fixture-cli"]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
EOF
  cat >"${PREPARE_REPO}/fixture-core/Cargo.toml" <<'EOF'
[package]
name = "fixture-core"
version.workspace = true
edition.workspace = true
EOF
  cat >"${PREPARE_REPO}/fixture-cli/Cargo.toml" <<'EOF'
[package]
name = "fixture-cli"
version.workspace = true
edition.workspace = true

[dependencies]
fixture-core = { path = "../fixture-core" }
EOF
  cat >"${PREPARE_REPO}/Cargo.lock" <<'EOF'
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "fixture-cli"
version = "0.1.0"
dependencies = [
 "fixture-core",
 "third-party",
]

[[package]]
name = "fixture-core"
version = "0.1.0"

[[package]]
name = "third-party"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
EOF
  printf '{"name":"fixture","version":"0.1.0"}\n' >"${PREPARE_REPO}/termui/frontend/package.json"
  cat >"${PREPARE_REPO}/termui/frontend/package-lock.json" <<'EOF'
{
  "name": "fixture",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "version": "0.1.0"
    }
  }
}
EOF
  printf 'initial readme\n' >"${PREPARE_REPO}/README.md"
  git -C "$PREPARE_REPO" add .
  git -C "$PREPARE_REPO" commit -m "Initial fixture" >/dev/null
  git -C "$PREPARE_REPO" push -u origin main >/dev/null 2>/dev/null

  cat >"${PREPARE_BIN}/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "generate-lockfile" ]]; then
  : >"${PREPARE_RELEASE_GENERATE_LOCKFILE_CALLED_FILE:?}"
  printf 'fixture cargo generate-lockfile must not be called\n' >&2
  exit 1
fi
if [[ "${1:-}" == "build" ]]; then
  if [[ "${PREPARE_RELEASE_FIXTURE_CARGO_MODE:-}" == "mutate-lock-on-build" ]]; then
    sed -i '/^name = "third-party"$/{n;s/^version = "2.0.0"$/version = "9.9.9"/;}' Cargo.lock
  fi
  exit 0
fi
printf 'unexpected cargo command: %s\n' "$*" >&2
exit 1
EOF
  chmod +x "${PREPARE_BIN}/cargo"

  cat >"${PREPARE_BIN}/rm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${PREPARE_RELEASE_FIXTURE_RM_MODE:-}" == "fail" && "${1:-}" == "-rf" ]]; then
  printf '%s\n' "${!#}" >"${PREPARE_RELEASE_RM_FAILURE_PATH_FILE:?}"
  printf 'fixture rm failure\n' >&2
  exit 1
fi
exec "${PREPARE_RELEASE_REAL_RM:?}" "$@"
EOF
  chmod +x "${PREPARE_BIN}/rm"

  cat >"${PREPARE_BIN}/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real_git="${PREPARE_RELEASE_REAL_GIT:?}"
mode="${PREPARE_RELEASE_FIXTURE_GIT_MODE:-}"
candidate_ref="${PREPARE_RELEASE_CANDIDATE_REF:-}"
if [[ "$mode" == "fail-status" && "${1:-}" == "status" ]]; then
  printf 'fixture git status failure\n' >&2
  exit 1
fi
if [[ "$mode" == "fail-stage-query" && "${1:-}" == "diff" &&
      "${2:-}" == "--cached" && "${3:-}" == "--check" ]]; then
  printf 'fixture staged query failure\n' >&2
  exit 1
fi
if [[ "$mode" == "advance-main-before-publish" &&
      "${1:-}" == "update-ref" && "${2:-}" == "--stdin" ]]; then
  if "$real_git" show-ref --verify --quiet "${PREPARE_RELEASE_CANDIDATE_REF:-refs/termd/release-candidates/9.9.8}"; then
    : >"${PREPARE_RELEASE_CANDIDATE_PREEXISTED_FILE:?}"
  fi
  base="$("$real_git" rev-parse refs/heads/main)"
  tree="$("$real_git" rev-parse "${base}^{tree}")"
  concurrent="$(printf 'fixture concurrent branch advance\n' | "$real_git" commit-tree "$tree" -p "$base")"
  "$real_git" update-ref refs/heads/main "$concurrent" "$base"
  "$real_git" "$@"
  exit $?
fi
if [[ ("$mode" == "candidate-create-then-return-74" ||
       "$mode" == "candidate-conflict") &&
      "${1:-}" == "update-ref" && "${2:-}" == "--stdin" ]]; then
  base="$("$real_git" rev-parse refs/heads/main)"
  tree="$("$real_git" rev-parse "${base}^{tree}")"
  concurrent="$(printf 'fixture concurrent branch advance\n' | "$real_git" commit-tree "$tree" -p "$base")"
  "$real_git" update-ref refs/heads/main "$concurrent" "$base"
  if [[ "$mode" == "candidate-conflict" ]]; then
    "$real_git" update-ref "${PREPARE_RELEASE_CANDIDATE_REF:?}" \
      "${PREPARE_RELEASE_CONFLICTING_CANDIDATE_OID:?}" ""
  fi
  "$real_git" "$@"
  exit $?
fi
if [[ "$mode" == "create-tag-before-publish" &&
      "${1:-}" == "update-ref" && "${2:-}" == "--stdin" ]]; then
  base="$("$real_git" rev-parse refs/heads/main)"
  "$real_git" update-ref "${PREPARE_RELEASE_CONCURRENT_TAG_REF:?}" "$base" ""
  "$real_git" "$@"
  exit $?
fi
if [[ "$mode" == "publish-then-fail" &&
      "${1:-}" == "update-ref" && "${2:-}" == "--stdin" ]]; then
  "$real_git" "$@"
  tag_oid="$("$real_git" rev-parse "${PREPARE_RELEASE_FORMAL_TAG_REF:?}")"
  "$real_git" update-ref "${PREPARE_RELEASE_CANDIDATE_REF:?}" "$tag_oid"
  printf 'fixture publication command failed after commit\n' >&2
  exit 73
fi
if [[ "$mode" == "publish-then-fail" && "${1:-}" == "update-ref" &&
      -n "$candidate_ref" && "$*" == *" -d "* && "$*" == *"$candidate_ref"* ]]; then
  printf '%s\n' "$*" >"${PREPARE_RELEASE_CANDIDATE_DELETE_CALL_FILE:?}"
  exec "$real_git" "$@"
fi
if [[ "$mode" == "candidate-create-then-return-74" &&
      "${1:-}" == "update-ref" && -n "$candidate_ref" && "${4:-}" == "$candidate_ref" &&
      -n "${5:-}" ]]; then
  "$real_git" "$@"
  printf 'fixture candidate create returned 74 after success\n' >&2
  exit 74
fi
if [[ "$mode" == "fail-atomic-push" && "${1:-}" == "push" ]]; then
  printf '%s\n' "$*" >>"${PREPARE_RELEASE_PUSH_CALLS_FILE:?}"
  printf 'fixture atomic push failure\n' >&2
  exit 1
fi
exec "$real_git" "$@"
EOF
  chmod +x "${PREPARE_BIN}/git"
}

prepare_release_fixture_teardown() {
  unset PREPARE_RELEASE_FIXTURE_CARGO_MODE PREPARE_RELEASE_FIXTURE_GIT_MODE
  unset PREPARE_RELEASE_FIXTURE_QA_MODE PREPARE_RELEASE_FIXTURE_RM_MODE
  unset PREPARE_RELEASE_GENERATE_LOCKFILE_CALLED_FILE
  unset PREPARE_RELEASE_CALLER_REPO PREPARE_RELEASE_PUSH_CALLS_FILE
  unset PREPARE_RELEASE_RM_FAILURE_PATH_FILE
  unset PREPARE_RELEASE_CANDIDATE_PREEXISTED_FILE PREPARE_RELEASE_CANDIDATE_REF
  unset PREPARE_RELEASE_CONCURRENT_TAG_REF PREPARE_RELEASE_FORMAL_TAG_REF
  unset PREPARE_RELEASE_CANDIDATE_DELETE_CALL_FILE
  unset PREPARE_RELEASE_CONFLICTING_CANDIDATE_OID
  "$PREPARE_REAL_RM" -rf -- "$PREPARE_FIXTURE_ROOT"
}

write_prepare_release_fixture_notes() {
  local version="$1"
  local summary="$2"
  mkdir -p "${PREPARE_REPO}/docs/releases"
  cat >"${PREPARE_REPO}/docs/releases/${version}.md" <<EOF
termd $version

用户可见变化:
- $summary
EOF
}

prepare_release_paths() {
  PREPARE_RELEASE_PATHS=(
    Cargo.toml
    Cargo.lock
    termui/frontend/package.json
    termui/frontend/package-lock.json
    "docs/releases/$1.md"
  )
}

run_prepare_release_fixture() {
  local version="$1"
  shift
  set +e
  PREPARE_OUTPUT="$(cd "$PREPARE_REPO" && PATH="${PREPARE_BIN}:$PATH" \
    bash scripts/prepare-release.sh "$version" "$@" 2>&1)"
  PREPARE_STATUS=$?
  set -e
}

assert_release_commit_shape() {
  local version="$1"
  local commit="$2"
  local expected changed
  changed="$(git -C "$PREPARE_REPO" diff-tree --no-commit-id --name-only -r "$commit" | LC_ALL=C sort)"
  expected="$(printf '%s\n' "${PREPARE_RELEASE_PATHS[@]}" | LC_ALL=C sort)"
  [[ "$changed" == "$expected" ]] || {
    printf 'release commit paths differ:\n%s\n' "$changed" >&2
    exit 1
  }
  git -C "$PREPARE_REPO" show "${commit}:docs/releases/${version}.md" | grep -Fq "用户可见变化"
}

run_reported_local_completion() {
  local repo_dir="$1"
  local output="$2"
  local notes_path="$3"
  local release_commit="$4"
  local commands

  mapfile -t commands < <(printf '%s\n' "$output" | sed -n 's/^\[prepare-release\]   //p')
  [[ "${#commands[@]}" -eq 2 ]]
  [[ "${commands[0]}" == "git add -- '${notes_path}'" ]]
  [[ "${commands[1]}" == "git merge --ff-only ${release_commit}" ]]
  (
    cd "$repo_dir"
    bash -c "${commands[0]}"
    bash -c "${commands[1]}"
  )
}

test_prepare_release_direct_help() (
  local output

  [[ -x "${ROOT_DIR}/scripts/prepare-release.sh" ]] || {
    printf 'prepare-release.sh is not executable\n' >&2
    exit 1
  }
  output="$("${ROOT_DIR}/scripts/prepare-release.sh" --help)"
  [[ "$output" == *"usage: scripts/prepare-release.sh"* ]]
)

test_prepare_release_validates_inputs_without_mutation() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after
  prepare_release_paths 9.9.1

  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  run_prepare_release_fixture 9.9.1 --skip-verify
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"release notes file is missing"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "missing notes" "$before" "$after"

  write_prepare_release_fixture_notes 9.9.1 \
    "请在 scripts/release-notes.sh 中补充此版本的功能、修复和兼容性说明。"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  run_prepare_release_fixture 9.9.1 --skip-verify
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"still the placeholder"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "placeholder notes" "$before" "$after"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=fail-status
  run_prepare_release_fixture 9.9.1 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"failed to inspect worktree status"* ]]
  assert_ref_missing "$PREPARE_REPO" refs/tags/9.9.1
)

test_prepare_release_rejects_dirty_release_paths() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after
  prepare_release_paths 9.9.2
  write_prepare_release_fixture_notes 9.9.2 "dirty release path 回归。"
  printf '# unstaged caller edit\n' >>"${PREPARE_REPO}/Cargo.toml"
  printf '# staged caller edit\n' >>"${PREPARE_REPO}/Cargo.lock"
  git -C "$PREPARE_REPO" add -- Cargo.lock

  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  run_prepare_release_fixture 9.9.2 --allow-dirty --skip-verify
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"release-owned files must be clean"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "dirty release paths" "$before" "$after"
)

test_prepare_release_success_isolated_and_exact() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base commit tag_commit status_before status_after caller_commit caller_paths
  prepare_release_paths 9.9.3
  write_prepare_release_fixture_notes 9.9.3 "隔离发版成功回归。"
  printf 'staged unrelated edit\n' >>"${PREPARE_REPO}/README.md"
  git -C "$PREPARE_REPO" add -- README.md
  printf 'unstaged unrelated edit\n' >>"${PREPARE_REPO}/README.md"
  printf 'untracked unrelated edit\n' >"${PREPARE_REPO}/scratch-untracked.txt"

  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  status_before="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"
  run_prepare_release_fixture 9.9.3 --allow-dirty --skip-verify
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }

  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]] || {
    printf 'dirty release moved the checked-out main branch\n' >&2
    exit 1
  }
  commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.3^{commit}')"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${commit}^")" == "$base" ]]
  assert_release_commit_shape 9.9.3 "$commit"
  [[ "$(git -C "$PREPARE_REPO" cat-file -t refs/tags/9.9.3)" == "tag" ]]
  tag_commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.3^{commit}')"
  [[ "$tag_commit" == "$commit" ]]
  assert_ref_missing "$PREPARE_REPO" refs/termd/release-candidates/9.9.3
  git -C "$PREPARE_REPO" show "${commit}:docs/releases/9.9.3.md" |
    grep -Fq "隔离发版成功回归。"

  status_after="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"
  [[ "$status_after" == "$status_before" ]]
  git -C "$PREPARE_REPO" commit -m "Commit staged caller work" >/dev/null
  caller_commit="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  caller_paths="$(git -C "$PREPARE_REPO" diff-tree --no-commit-id --name-only -r "$caller_commit")"
  [[ "$caller_paths" == "README.md" ]]
  git -C "$PREPARE_REPO" diff --name-only | grep -Fxq README.md
  git -C "$PREPARE_REPO" status --porcelain=v1 -- scratch-untracked.txt | grep -Fq '?? scratch-untracked.txt'
)

test_prepare_release_clean_success_leaves_caller_read_only() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base commit status_before status_after
  prepare_release_paths 9.9.13
  write_prepare_release_fixture_notes 9.9.13 "clean caller 只读回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  status_before="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"

  run_prepare_release_fixture 9.9.13 --skip-verify
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]] || {
    printf 'clean release moved the caller branch\n' >&2
    exit 1
  }
  commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.13^{commit}')"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${commit}^")" == "$base" ]]
  status_after="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"
  [[ "$status_after" == "$status_before" ]]
  assert_ref_missing "$PREPARE_REPO" refs/termd/release-candidates/9.9.13
  [[ "$PREPARE_OUTPUT" == *"local main remains at ${base}"* ]]
  run_reported_local_completion "$PREPARE_REPO" "$PREPARE_OUTPUT" \
    docs/releases/9.9.13.md "$commit"
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$commit" ]]
  [[ -z "$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)" ]]
  [[ -z "$(git -C "$PREPARE_REPO" diff --cached --name-only)" ]]
  assert_release_commit_shape 9.9.13 "$commit"
)

test_prepare_release_postcheck_staged_change_remains_read_only() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base caller_commit caller_paths release_commit
  prepare_release_paths 9.9.14
  write_prepare_release_fixture_notes 9.9.14 "预检后 staged caller 写入回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"

  export PREPARE_RELEASE_FIXTURE_QA_MODE=stage-caller-success
  export PREPARE_RELEASE_CALLER_REPO="$PREPARE_REPO"
  run_prepare_release_fixture 9.9.14
  unset PREPARE_RELEASE_FIXTURE_QA_MODE PREPARE_RELEASE_CALLER_REPO
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]]
  [[ "$(git -C "$PREPARE_REPO" diff --cached --name-only)" == "README.md" ]]
  release_commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.14^{commit}')"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${release_commit}^")" == "$base" ]]
  assert_ref_missing "$PREPARE_REPO" refs/termd/release-candidates/9.9.14
  git -C "$PREPARE_REPO" commit -m "Commit concurrent caller work" >/dev/null
  caller_commit="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  caller_paths="$(git -C "$PREPARE_REPO" diff-tree --no-commit-id --name-only -r "$caller_commit")"
  [[ "$caller_paths" == "README.md" ]]
)

test_prepare_release_preserves_non_workspace_lock_entries() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base commit lock
  prepare_release_paths 9.9.4
  write_prepare_release_fixture_notes 9.9.4 "lockfile 定向版本更新回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"

  run_prepare_release_fixture 9.9.4 --skip-verify
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }
  [[ ! -e "$PREPARE_RELEASE_GENERATE_LOCKFILE_CALLED_FILE" ]]
  commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.4^{commit}')"
  lock="$(git -C "$PREPARE_REPO" show "${commit}:Cargo.lock")"
  [[ "$lock" == *$'name = "fixture-cli"\nversion = "9.9.4"'* ]]
  [[ "$lock" == *$'name = "fixture-core"\nversion = "9.9.4"'* ]]
  [[ "$lock" == *$'name = "third-party"\nversion = "2.0.0"'* ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse HEAD)" == "$base" ]]
)

test_prepare_release_rejects_verification_lockfile_mutation() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after base
  prepare_release_paths 9.9.15
  write_prepare_release_fixture_notes 9.9.15 "lockfile 验证期篡改拒绝回归。"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"

  export PREPARE_RELEASE_FIXTURE_CARGO_MODE=mutate-lock-on-build
  run_prepare_release_fixture 9.9.15
  unset PREPARE_RELEASE_FIXTURE_CARGO_MODE
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"Cargo.lock changed outside workspace package version updates"* ]]
  assert_ref_missing "$PREPARE_REPO" refs/tags/9.9.15
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "verification lockfile mutation" "$before" "$after"
  [[ "$(git -C "$PREPARE_REPO" rev-parse HEAD)" == "$base" ]]
)

test_prepare_release_concurrent_caller_mutation_is_preserved() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base before after
  prepare_release_paths 9.9.5
  write_prepare_release_fixture_notes 9.9.5 "并发 caller 修改隔离回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" \
    termui/frontend/package.json termui/frontend/package-lock.json docs/releases/9.9.5.md)"

  export PREPARE_RELEASE_FIXTURE_QA_MODE=mutate-caller-fail
  export PREPARE_RELEASE_CALLER_REPO="$PREPARE_REPO"
  run_prepare_release_fixture 9.9.5
  unset PREPARE_RELEASE_FIXTURE_QA_MODE PREPARE_RELEASE_CALLER_REPO
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"fixture concurrent caller mutation"* ]]
  grep -Fq '# concurrent caller worktree write' "${PREPARE_REPO}/Cargo.toml"
  git -C "$PREPARE_REPO" show ':Cargo.lock' | grep -Fq 'concurrent caller index write'
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" \
    termui/frontend/package.json termui/frontend/package-lock.json docs/releases/9.9.5.md)"
  assert_prepare_release_snapshot_unchanged "concurrent mutation other paths" "$before" "$after"
  [[ "$(git -C "$PREPARE_REPO" rev-parse HEAD)" == "$base" ]]
)

test_prepare_release_ignores_plausible_hook_oid() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after base
  prepare_release_paths 9.9.6
  write_prepare_release_fixture_notes 9.9.6 "伪造 dangling OID 隔离回归。"
  cat >"${PREPARE_REPO}/.git/hooks/pre-commit" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
base="$(git rev-parse HEAD)"
tree="$(git rev-parse HEAD^{tree})"
dangling="$(printf 'fixture dangling commit\n' | git commit-tree "$tree" -p "$base")"
printf '[detached HEAD %s] plausible hook output\n' "$dangling"
exit 1
EOF
  chmod +x "${PREPARE_REPO}/.git/hooks/pre-commit"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"

  run_prepare_release_fixture 9.9.6 --skip-verify
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"plausible hook output"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "failed hook with plausible OID" "$before" "$after"
  [[ "$(git -C "$PREPARE_REPO" rev-parse HEAD)" == "$base" ]]
)

test_prepare_release_post_stage_query_failure_is_isolated() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after base
  prepare_release_paths 9.9.7
  write_prepare_release_fixture_notes 9.9.7 "staging 后 query failure 隔离回归。"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=fail-stage-query
  run_prepare_release_fixture 9.9.7 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"fixture staged query failure"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "post-stage query failure" "$before" "$after"
  [[ "$(git -C "$PREPARE_REPO" rev-parse HEAD)" == "$base" ]]
)

test_prepare_release_branch_cas_failure_does_not_tag() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base candidate candidate_preexisted candidate_type concurrent release_commit
  prepare_release_paths 9.9.8
  write_prepare_release_fixture_notes 9.9.8 "并发 branch CAS 回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  candidate_preexisted="${PREPARE_FIXTURE_ROOT}/candidate-preexisted"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=advance-main-before-publish
  export PREPARE_RELEASE_CANDIDATE_PREEXISTED_FILE="$candidate_preexisted"
  export PREPARE_RELEASE_CANDIDATE_REF=refs/termd/release-candidates/9.9.8
  run_prepare_release_fixture 9.9.8 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_CANDIDATE_PREEXISTED_FILE
  unset PREPARE_RELEASE_CANDIDATE_REF
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ ! -e "$candidate_preexisted" ]] || {
    printf 'candidate existed before the publication transaction failed\n' >&2
    exit 1
  }
  [[ "$PREPARE_OUTPUT" == *"local publication was rejected; exact annotated tag object is retained"* ]]
  [[ "$PREPARE_OUTPUT" == *"git update-ref -d refs/termd/release-candidates/9.9.8"* ]]
  concurrent="$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${concurrent}^")" == "$base" ]]
  assert_ref_missing "$PREPARE_REPO" refs/tags/9.9.8

  candidate="refs/termd/release-candidates/9.9.8"
  candidate_type="$(git -C "$PREPARE_REPO" cat-file -t "$candidate" 2>/dev/null || true)"
  [[ "$candidate_type" == "tag" ]] || {
    printf 'CAS failure did not preserve the annotated tag object through the candidate ref\n' >&2
    exit 1
  }
  release_commit="$(git -C "$PREPARE_REPO" rev-parse "${candidate}^{commit}")"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${release_commit}^")" == "$base" ]]
  assert_release_commit_shape 9.9.8 "$release_commit"

  git -C "$PREPARE_REPO" reset --hard --quiet "$concurrent"
  run_prepare_release_fixture 9.9.8 --skip-verify
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }
  assert_ref_missing "$PREPARE_REPO" "$candidate"
  release_commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.8^{commit}')"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${release_commit}^")" == \
    "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" ]]
)

test_prepare_release_concurrent_formal_tag_does_not_create_candidate() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base candidate fsck_output old_candidate
  prepare_release_paths 9.9.15
  write_prepare_release_fixture_notes 9.9.15 "并发正式 tag 回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  candidate=refs/termd/release-candidates/9.9.15
  git -C "$PREPARE_REPO" tag -a fixture-old-candidate -m "old candidate" "$base"
  old_candidate="$(git -C "$PREPARE_REPO" rev-parse refs/tags/fixture-old-candidate)"
  git -C "$PREPARE_REPO" update-ref "$candidate" "$old_candidate" ""
  git -C "$PREPARE_REPO" update-ref -d refs/tags/fixture-old-candidate "$old_candidate"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=create-tag-before-publish
  export PREPARE_RELEASE_CONCURRENT_TAG_REF=refs/tags/9.9.15
  run_prepare_release_fixture 9.9.15 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_CONCURRENT_TAG_REF
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/tags/9.9.15)" == "$base" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse "$candidate")" == "$old_candidate" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${candidate}^{commit}")" == "$base" ]]
  fsck_output="$(git -C "$PREPARE_REPO" fsck --no-reflogs --unreachable 2>&1)"
  [[ "$fsck_output" != *"$old_candidate"* ]]
)

test_prepare_release_candidate_create_nonzero_is_verified() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local candidate fsck_output release_commit tag_oid
  prepare_release_paths 9.9.16
  write_prepare_release_fixture_notes 9.9.16 "candidate create nonzero 回归。"
  candidate=refs/termd/release-candidates/9.9.16

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=candidate-create-then-return-74
  export PREPARE_RELEASE_CANDIDATE_REF="$candidate"
  run_prepare_release_fixture 9.9.16 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_CANDIDATE_REF
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"fixture candidate create returned 74 after success"* ]]
  [[ "$PREPARE_OUTPUT" == *"exact annotated tag object is retained"* ]]
  [[ "$PREPARE_OUTPUT" != *"recovery candidate could not be created"* ]]
  tag_oid="$(git -C "$PREPARE_REPO" rev-parse "$candidate")"
  [[ "$(git -C "$PREPARE_REPO" cat-file -t "$tag_oid")" == "tag" ]]
  release_commit="$(git -C "$PREPARE_REPO" rev-parse "${candidate}^{commit}")"
  assert_release_commit_shape 9.9.16 "$release_commit"
  fsck_output="$(git -C "$PREPARE_REPO" fsck --no-reflogs --unreachable 2>&1)"
  [[ "$fsck_output" != *"$tag_oid"* && "$fsck_output" != *"$release_commit"* ]]
)

test_prepare_release_conflicting_candidate_is_not_overwritten() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base candidate conflict_oid fsck_output
  prepare_release_paths 9.9.17
  write_prepare_release_fixture_notes 9.9.17 "candidate conflict 回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  candidate=refs/termd/release-candidates/9.9.17
  git -C "$PREPARE_REPO" tag -a fixture-conflicting-candidate -m "conflict" "$base"
  conflict_oid="$(git -C "$PREPARE_REPO" rev-parse refs/tags/fixture-conflicting-candidate)"
  git -C "$PREPARE_REPO" update-ref -d refs/tags/fixture-conflicting-candidate "$conflict_oid"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=candidate-conflict
  export PREPARE_RELEASE_CANDIDATE_REF="$candidate"
  export PREPARE_RELEASE_CONFLICTING_CANDIDATE_OID="$conflict_oid"
  run_prepare_release_fixture 9.9.17 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_CANDIDATE_REF
  unset PREPARE_RELEASE_CONFLICTING_CANDIDATE_OID
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"recovery candidate could not be created"* ]]
  [[ "$PREPARE_OUTPUT" != *"exact annotated tag object is retained"* ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse "$candidate")" == "$conflict_oid" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${candidate}^{commit}")" == "$base" ]]
  fsck_output="$(git -C "$PREPARE_REPO" fsck --no-reflogs --unreachable 2>&1)"
  [[ "$fsck_output" != *"$conflict_oid"* ]]
)

test_prepare_release_publication_partial_success_is_consistent() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base candidate_delete_call candidate_delete_log commit fsck_output tag_commit tag_oid
  prepare_release_paths 9.9.9
  write_prepare_release_fixture_notes 9.9.9 "local publication partial success 回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)"
  candidate_delete_log="${PREPARE_FIXTURE_ROOT}/candidate-delete-call"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=publish-then-fail
  export PREPARE_RELEASE_CANDIDATE_REF=refs/termd/release-candidates/9.9.9
  export PREPARE_RELEASE_FORMAL_TAG_REF=refs/tags/9.9.9
  export PREPARE_RELEASE_CANDIDATE_DELETE_CALL_FILE="$candidate_delete_log"
  run_prepare_release_fixture 9.9.9 --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_CANDIDATE_REF
  unset PREPARE_RELEASE_FORMAL_TAG_REF PREPARE_RELEASE_CANDIDATE_DELETE_CALL_FILE
  [[ "$PREPARE_STATUS" -eq 73 ]]
  [[ "$PREPARE_OUTPUT" == *"publication command failed after the exact formal tag was created"* ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]]
  tag_commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.9^{commit}')"
  commit="$tag_commit"
  tag_oid="$(git -C "$PREPARE_REPO" rev-parse refs/tags/9.9.9)"
  candidate_delete_call="$(cat "$candidate_delete_log")"
  [[ "$candidate_delete_call" == *"refs/termd/release-candidates/9.9.9 ${tag_oid}" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${commit}^")" == "$base" ]]
  assert_ref_missing "$PREPARE_REPO" refs/termd/release-candidates/9.9.9
  assert_release_commit_shape 9.9.9 "$commit"
  fsck_output="$(git -C "$PREPARE_REPO" fsck --no-reflogs --unreachable 2>&1)"
  [[ "$fsck_output" != *"$tag_oid"* && "$fsck_output" != *"$commit"* ]]
)

test_prepare_release_cleanup_failure_preserves_primary_status() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local before after failed_path
  prepare_release_paths 9.9.10
  write_prepare_release_fixture_notes 9.9.10 "cleanup failure 回归。"
  before="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  failed_path="${PREPARE_FIXTURE_ROOT}/failed-rm-path"

  export PREPARE_RELEASE_FIXTURE_QA_MODE=fail-status-42
  export PREPARE_RELEASE_FIXTURE_RM_MODE=fail
  export PREPARE_RELEASE_RM_FAILURE_PATH_FILE="$failed_path"
  run_prepare_release_fixture 9.9.10
  unset PREPARE_RELEASE_FIXTURE_QA_MODE PREPARE_RELEASE_FIXTURE_RM_MODE
  unset PREPARE_RELEASE_RM_FAILURE_PATH_FILE
  [[ "$PREPARE_STATUS" -eq 42 ]]
  [[ "$PREPARE_OUTPUT" == *"fixture QA failure with status 42"* ]]
  [[ "$PREPARE_OUTPUT" == *"failed to remove temporary release directory"* ]]
  after="$(snapshot_prepare_release_caller "$PREPARE_REPO" "${PREPARE_RELEASE_PATHS[@]}")"
  assert_prepare_release_snapshot_unchanged "cleanup failure" "$before" "$after"
  [[ -s "$failed_path" ]]
  "$PREPARE_REAL_RM" -rf -- "$(cat "$failed_path")"
)

test_prepare_release_atomic_push_failure_has_no_remote_partial_update() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base remote_before remote_after calls commit tag_commit
  prepare_release_paths 9.9.11
  write_prepare_release_fixture_notes 9.9.11 "atomic push failure 回归。"
  base="$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)"
  remote_before="$(git --git-dir="$PREPARE_REMOTE" rev-parse refs/heads/main)"
  calls="${PREPARE_FIXTURE_ROOT}/push-calls"

  export PREPARE_RELEASE_FIXTURE_GIT_MODE=fail-atomic-push
  export PREPARE_RELEASE_PUSH_CALLS_FILE="$calls"
  run_prepare_release_fixture 9.9.11 --push --skip-verify
  unset PREPARE_RELEASE_FIXTURE_GIT_MODE PREPARE_RELEASE_PUSH_CALLS_FILE
  [[ "$PREPARE_STATUS" -ne 0 ]]
  [[ "$PREPARE_OUTPUT" == *"atomic push failed"* ]]
  [[ "$(wc -l <"$calls")" -eq 1 ]]
  grep -Eq '^push --atomic --force-with-lease=refs/heads/main:[0-9a-f]+ origin [0-9a-f]+:refs/heads/main [0-9a-f]+:refs/tags/9\.9\.11$' "$calls"
  remote_after="$(git --git-dir="$PREPARE_REMOTE" rev-parse refs/heads/main)"
  [[ "$remote_after" == "$remote_before" ]]
  assert_ref_missing "$PREPARE_REMOTE" refs/tags/9.9.11

  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]]
  tag_commit="$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.11^{commit}')"
  commit="$tag_commit"
  [[ "$(git -C "$PREPARE_REPO" rev-parse "${commit}^")" == "$base" ]]
)

test_prepare_release_atomic_push_success() (
  prepare_release_fixture_setup
  trap prepare_release_fixture_teardown EXIT
  local base remote_commit remote_tag_commit status_before status_after
  prepare_release_paths 9.9.12
  write_prepare_release_fixture_notes 9.9.12 "atomic push success 回归。"
  printf 'staged dirty push edit\n' >>"${PREPARE_REPO}/README.md"
  git -C "$PREPARE_REPO" add -- README.md
  printf 'unstaged dirty push edit\n' >>"${PREPARE_REPO}/README.md"
  printf 'untracked dirty push edit\n' >"${PREPARE_REPO}/scratch-untracked.txt"
  base="$(git -C "$PREPARE_REPO" rev-parse HEAD)"
  status_before="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"

  run_prepare_release_fixture 9.9.12 --allow-dirty --push --skip-verify
  [[ "$PREPARE_STATUS" -eq 0 ]] || {
    printf '%s\n' "$PREPARE_OUTPUT" >&2
    exit 1
  }
  remote_commit="$(git --git-dir="$PREPARE_REMOTE" rev-parse refs/heads/main)"
  remote_tag_commit="$(git --git-dir="$PREPARE_REMOTE" rev-parse 'refs/tags/9.9.12^{commit}')"
  [[ "$remote_tag_commit" == "$remote_commit" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse refs/heads/main)" == "$base" ]]
  [[ "$(git -C "$PREPARE_REPO" rev-parse 'refs/tags/9.9.12^{commit}')" == "$remote_commit" ]]
  assert_ref_missing "$PREPARE_REPO" refs/termd/release-candidates/9.9.12
  status_after="$(git -C "$PREPARE_REPO" status --porcelain=v1 --untracked-files=all)"
  [[ "$status_after" == "$status_before" ]]
  [[ "$PREPARE_OUTPUT" == *"local main remains at ${base}"* ]]
  [[ "$PREPARE_OUTPUT" == *"git merge --ff-only ${remote_commit}"* ]]
)

test_termd_install_transaction_rolls_back_every_failure_boundary() (
  load_termd_installer_functions

  local root fail_step initially_active status installed candidate service_state enabled_state
  local env_file wrapper_file unit_file chown_calls
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT

  maybe_fail() {
    if [[ "$FAIL_STEP" == "$1" && ( "$FAIL_TRIGGERED" -eq 0 || "$FAIL_PERSISTENT" -eq 1 ) ]]; then
      FAIL_TRIGGERED=1
      return 42
    fi
  }
  ensure_system_user() { maybe_fail ensure-user; }
  chown_state_dir() {
    chown_calls=$((chown_calls + 1))
    if [[ "$chown_calls" -eq 1 ]]; then
      maybe_fail chown-pre
    else
      maybe_fail chown-post
    fi
  }
  write_env_file() {
    printf 'HOME=/tmp\nSHELL=/bin/sh\n' >"$ENV_FILE"
    maybe_fail write-env
  }
  write_wrapper() {
    printf 'new-wrapper\n' >"$WRAPPER_FILE"
    maybe_fail write-wrapper
  }
  write_unit() {
    printf 'new-unit\n' >"$UNIT_FILE"
    maybe_fail write-unit
  }
  stop_service_before_supervisor_runtime_clear() { maybe_fail stop-runtime-clear; }
  clear_session_state_after_state_dir_change() { maybe_fail clear-state; }
  persist_supervisor_version() { maybe_fail persist-version; }
  print_initial_pairing_token() { :; }
  systemctl() {
    case "${1:-}" in
      is-active) [[ "$(cat "$service_state")" == "active" ]] ;;
      is-enabled) [[ "$(cat "$enabled_state")" == "enabled" ]] ;;
      stop) printf 'inactive\n' >"$service_state" ;;
      start) printf 'active\n' >"$service_state" ;;
      daemon-reload) maybe_fail daemon-reload ;;
      enable)
        printf 'enabled\n' >"$enabled_state"
        maybe_fail enable
        ;;
      disable) printf 'disabled\n' >"$enabled_state" ;;
      restart)
        printf 'active\n' >"$service_state"
        maybe_fail restart
        ;;
      *) : ;;
    esac
  }

  for initially_active in 0 1; do
    for fail_step in \
      ensure-user chown-pre write-env write-wrapper write-unit \
      stop-runtime-clear clear-state persist-version chown-post \
      daemon-reload enable restart
    do
      local case_dir="${root}/${initially_active}-${fail_step}"
      mkdir -p "${case_dir}/prefix/bin" "${case_dir}/etc" "${case_dir}/state"
      INSTALL_PREFIX="${case_dir}/prefix"
      STATE_DIR="${case_dir}/state"
      ENV_FILE="${case_dir}/etc/termd.env"
      WRAPPER_FILE="${case_dir}/etc/termd-run"
      UNIT_FILE="${case_dir}/etc/termd.service"
      INSTALL_STAGING_DIR="${case_dir}/staging"
      mkdir -p "$INSTALL_STAGING_DIR"
      SERVICE_NAME=termd
      installed="${INSTALL_PREFIX}/bin/termd"
      candidate="${case_dir}/candidate"
      service_state="${case_dir}/service-state"
      enabled_state="${case_dir}/enabled-state"
      env_file="$ENV_FILE"
      wrapper_file="$WRAPPER_FILE"
      unit_file="$UNIT_FILE"
      printf 'old-binary\n' >"$installed"
      printf 'new-binary\n' >"$candidate"
      printf 'old-env\n' >"$env_file"
      printf 'old-wrapper\n' >"$wrapper_file"
      printf 'old-unit\n' >"$unit_file"
      if [[ "$initially_active" -eq 1 ]]; then
        printf 'active\n' >"$service_state"
      else
        printf 'inactive\n' >"$service_state"
      fi
      printf 'enabled\n' >"$enabled_state"
      FAIL_STEP="$fail_step"
      FAIL_TRIGGERED=0
      FAIL_PERSISTENT=0
      chown_calls=0

      set +e
      install_staged_candidate "$candidate"
      status=$?
      set -e
      [[ "$status" -eq 42 ]]
      [[ "$(cat "$installed")" == "old-binary" ]]
      [[ "$(cat "$env_file")" == "old-env" ]]
      [[ "$(cat "$wrapper_file")" == "old-wrapper" ]]
      [[ "$(cat "$unit_file")" == "old-unit" ]]
      [[ "$(cat "$enabled_state")" == "enabled" ]]
      if [[ "$initially_active" -eq 1 ]]; then
        [[ "$(cat "$service_state")" == "active" ]]
      else
        [[ "$(cat "$service_state")" == "inactive" ]]
      fi
    done
  done

  local success_dir="${root}/success"
  mkdir -p "${success_dir}/prefix/bin" "${success_dir}/etc" "${success_dir}/state" "${success_dir}/staging"
  INSTALL_PREFIX="${success_dir}/prefix"
  STATE_DIR="${success_dir}/state"
  ENV_FILE="${success_dir}/etc/termd.env"
  WRAPPER_FILE="${success_dir}/etc/termd-run"
  UNIT_FILE="${success_dir}/etc/termd.service"
  INSTALL_STAGING_DIR="${success_dir}/staging"
  SERVICE_NAME=termd
  installed="${INSTALL_PREFIX}/bin/termd"
  candidate="${success_dir}/candidate"
  service_state="${success_dir}/service-state"
  enabled_state="${success_dir}/enabled-state"
  printf 'old-binary\n' >"$installed"
  printf 'new-binary\n' >"$candidate"
  printf 'old-env\n' >"$ENV_FILE"
  printf 'old-wrapper\n' >"$WRAPPER_FILE"
  printf 'old-unit\n' >"$UNIT_FILE"
  printf 'active\n' >"$service_state"
  printf 'enabled\n' >"$enabled_state"
  FAIL_STEP=none
  FAIL_TRIGGERED=0
  FAIL_PERSISTENT=0
  chown_calls=0
  install_staged_candidate "$candidate"
  [[ "$(cat "$installed")" == "new-binary" ]]
  [[ "$(cat "$ENV_FILE")" != "old-env" ]]
  [[ "$(cat "$WRAPPER_FILE")" == "new-wrapper" ]]
  [[ "$(cat "$UNIT_FILE")" == "new-unit" ]]
  [[ "$(cat "$service_state")" == "active" ]]

  printf 'old-binary\n' >"$installed"
  printf 'old-env\n' >"$ENV_FILE"
  printf 'old-wrapper\n' >"$WRAPPER_FILE"
  printf 'old-unit\n' >"$UNIT_FILE"
  printf 'active\n' >"$service_state"
  FAIL_STEP=daemon-reload
  FAIL_TRIGGERED=0
  FAIL_PERSISTENT=1
  chown_calls=0
  set +e
  install_staged_candidate "$candidate"
  status=$?
  set -e
  [[ "$status" -eq 42 ]]
  [[ "$(cat "$installed")" == "old-binary" ]]
  [[ "$(cat "$service_state")" == "active" ]]
)

test_termrelay_builds_all_install_candidates_in_isolated_staging() (
  load_termrelay_installer_functions

  local root candidate_binary env_checksum wrapper_checksum unit_checksum
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT

  INSTALL_PREFIX="${root}/prefix"
  ENV_DIR="${root}/etc"
  ENV_FILE="${ENV_DIR}/termrelay.env"
  WRAPPER_DIR="${root}/lib"
  WRAPPER_FILE="${WRAPPER_DIR}/termrelay-run"
  UNIT_FILE="${root}/systemd/termrelay.service"
  STATE_DIR="${root}/state"
  INSTALL_STAGING_DIR="${root}/staging"
  SERVICE_NAME=root
  mkdir -p "$ENV_DIR" "$WRAPPER_DIR" "$(dirname "$UNIT_FILE")" "$INSTALL_STAGING_DIR"
  printf 'TERMRELAY_LISTEN=127.0.0.1:9000\nTERMRELAY_ALLOW_OPEN_RELAY=1\n' >"$ENV_FILE"
  printf 'old-wrapper\n' >"$WRAPPER_FILE"
  printf 'old-unit\n' >"$UNIT_FILE"
  candidate_binary="${INSTALL_STAGING_DIR}/termrelay"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$candidate_binary"
  chmod 0755 "$candidate_binary"
  env_checksum="$(sha256sum "$ENV_FILE")"
  wrapper_checksum="$(sha256sum "$WRAPPER_FILE")"
  unit_checksum="$(sha256sum "$UNIT_FILE")"

  chown() { :; }
  build_install_candidates "$candidate_binary"

  [[ "$(sha256sum "$ENV_FILE")" == "$env_checksum" ]]
  [[ "$(sha256sum "$WRAPPER_FILE")" == "$wrapper_checksum" ]]
  [[ "$(sha256sum "$UNIT_FILE")" == "$unit_checksum" ]]
  [[ -s "${INSTALL_STAGING_DIR}/termrelay.env" ]]
  [[ -x "${INSTALL_STAGING_DIR}/termrelay-run" ]]
  [[ -s "${INSTALL_STAGING_DIR}/termrelay.service" ]]
  bash -n "${INSTALL_STAGING_DIR}/termrelay.env"
  bash -n "${INSTALL_STAGING_DIR}/termrelay-run"
  grep -Fq "ENV_FILE=${ENV_FILE}" "${INSTALL_STAGING_DIR}/termrelay-run"
  grep -Fq "ExecStart=${WRAPPER_FILE}" "${INSTALL_STAGING_DIR}/termrelay.service"
  ! grep -Fq 'TERMRELAY_ALLOW_OPEN_RELAY' "${INSTALL_STAGING_DIR}/termrelay.env"
  grep -Fq 'TERMRELAY_SETUP_TOKEN_FILE=' "${INSTALL_STAGING_DIR}/termrelay.env"
  grep -Fq 'TERMRELAY_DAEMON_REGISTRY=' "${INSTALL_STAGING_DIR}/termrelay.env"
  ! grep -Fq -- '--allow-open-relay' "${INSTALL_STAGING_DIR}/termrelay-run"
)

test_termrelay_install_transaction_rolls_back_every_failure_boundary() (
  load_termrelay_installer_functions

  local root test_owner test_group variant fail_step status output_file
  local case_dir installed service_state enabled_state unknown_file first_install
  local old_binary_owner old_env_owner old_wrapper_owner old_unit_owner
  root="$(mktemp -d)"
  trap 'rm -rf "$root"' EXIT
  test_owner="$(id -un)"
  test_group="$(id -gn)"

  eval "$(declare -f commit_install_file | sed '1s/commit_install_file/real_commit_install_file/')"

  maybe_fail() {
    if [[ "$FAIL_STEP" == "$1" && ( "$FAIL_TRIGGERED" -eq 0 || "$FAIL_PERSISTENT" -eq 1 ) ]]; then
      FAIL_TRIGGERED=1
      return 42
    fi
  }
  commit_install_file() {
    local boundary="$1"
    if [[ "$boundary" == "env" ]]; then
      boundary=config
    fi
    maybe_fail "$boundary" || return $?
    real_commit_install_file "$1" "$2" "$3" "$4" "$test_owner" "$test_group"
  }
  prepare_runtime_support_files() { :; }
  verify_service_healthy() {
    maybe_fail health || return $?
    health_checks=$((health_checks + 1))
    systemctl is-active --quiet "$SERVICE_NAME" || return $?
    health_checks=$((health_checks + 1))
    systemctl is-active --quiet "$SERVICE_NAME"
  }
  systemctl() {
    case "${1:-}" in
      is-active) [[ "$(cat "$service_state")" == "active" ]] ;;
      is-enabled) [[ "$(cat "$enabled_state")" == "enabled" ]] ;;
      daemon-reload)
        daemon_reload_calls=$((daemon_reload_calls + 1))
        if [[ "$ROLLBACK_RELOAD_FAIL" -eq 1 && "$daemon_reload_calls" -ge 2 ]]; then
          return 43
        fi
        maybe_fail reload
        ;;
      enable)
        printf 'enabled\n' >"$enabled_state"
        maybe_fail enable
        ;;
      disable) printf 'disabled\n' >"$enabled_state" ;;
      restart)
        printf 'active\n' >"$service_state"
        maybe_fail restart
        ;;
      stop) printf 'inactive\n' >"$service_state" ;;
      *) : ;;
    esac
  }

  setup_termrelay_case() {
    local case_name="$1"
    local initial_state="$2"
    local initial_enabled="$3"
    first_install="$4"
    case_dir="${root}/${case_name}"
    INSTALL_PREFIX="${case_dir}/prefix"
    ENV_DIR="${case_dir}/etc"
    ENV_FILE="${ENV_DIR}/termrelay.env"
    WRAPPER_DIR="${case_dir}/lib"
    WRAPPER_FILE="${WRAPPER_DIR}/termrelay-run"
    UNIT_FILE="${case_dir}/systemd/termrelay.service"
    STATE_DIR="${case_dir}/state"
    INSTALL_STAGING_DIR="${case_dir}/staging"
    SERVICE_NAME="$test_owner"
    installed="${INSTALL_PREFIX}/bin/termrelay"
    service_state="${case_dir}/service-state"
    enabled_state="${case_dir}/enabled-state"
    unknown_file="${case_dir}/etc/keep-existing"
    output_file="${case_dir}/install-output"
    mkdir -p "$(dirname "$installed")" "$ENV_DIR" "$WRAPPER_DIR" \
      "$(dirname "$UNIT_FILE")" "$INSTALL_STAGING_DIR"
    printf 'untouched\n' >"$unknown_file"
    if [[ "$first_install" -eq 0 ]]; then
      printf 'old-binary\n' >"$installed"
      printf 'old-env\n' >"$ENV_FILE"
      printf 'old-wrapper\n' >"$WRAPPER_FILE"
      printf 'old-unit\n' >"$UNIT_FILE"
      chmod 0711 "$installed"
      chmod 0600 "$ENV_FILE"
      chmod 0701 "$WRAPPER_FILE"
      chmod 0640 "$UNIT_FILE"
      if [[ "$EUID" -eq 0 ]]; then
        chown 65534:65534 "$installed" "$ENV_FILE" "$WRAPPER_FILE" "$UNIT_FILE"
      fi
      old_binary_owner="$(stat -c %u:%g "$installed")"
      old_env_owner="$(stat -c %u:%g "$ENV_FILE")"
      old_wrapper_owner="$(stat -c %u:%g "$WRAPPER_FILE")"
      old_unit_owner="$(stat -c %u:%g "$UNIT_FILE")"
    fi
    printf 'new-binary\n' >"${INSTALL_STAGING_DIR}/termrelay"
    printf 'new-env\n' >"${INSTALL_STAGING_DIR}/termrelay.env"
    printf 'new-wrapper\n' >"${INSTALL_STAGING_DIR}/termrelay-run"
    printf 'new-unit\n' >"${INSTALL_STAGING_DIR}/termrelay.service"
    printf '%s\n' "$initial_state" >"$service_state"
    printf '%s\n' "$initial_enabled" >"$enabled_state"
    FAIL_TRIGGERED=0
    FAIL_PERSISTENT=0
    ROLLBACK_RELOAD_FAIL=0
    daemon_reload_calls=0
    health_checks=0
  }

  assert_termrelay_case_rolled_back() {
    local expected_state="$1"
    local expected_enabled="$2"
    if [[ "$first_install" -eq 1 ]]; then
      [[ ! -e "$installed" ]]
      [[ ! -e "$ENV_FILE" ]]
      [[ ! -e "$WRAPPER_FILE" ]]
      [[ ! -e "$UNIT_FILE" ]]
    else
      [[ "$(cat "$installed")" == "old-binary" ]]
      [[ "$(cat "$ENV_FILE")" == "old-env" ]]
      [[ "$(cat "$WRAPPER_FILE")" == "old-wrapper" ]]
      [[ "$(cat "$UNIT_FILE")" == "old-unit" ]]
      [[ "$(stat -c %a "$installed")" == "711" ]]
      [[ "$(stat -c %a "$ENV_FILE")" == "600" ]]
      [[ "$(stat -c %a "$WRAPPER_FILE")" == "701" ]]
      [[ "$(stat -c %a "$UNIT_FILE")" == "640" ]]
      [[ "$(stat -c %u:%g "$installed")" == "$old_binary_owner" ]]
      [[ "$(stat -c %u:%g "$ENV_FILE")" == "$old_env_owner" ]]
      [[ "$(stat -c %u:%g "$WRAPPER_FILE")" == "$old_wrapper_owner" ]]
      [[ "$(stat -c %u:%g "$UNIT_FILE")" == "$old_unit_owner" ]]
    fi
    [[ "$(cat "$unknown_file")" == "untouched" ]]
    [[ "$(cat "$service_state")" == "$expected_state" ]]
    [[ "$(cat "$enabled_state")" == "$expected_enabled" ]]
  }

  for variant in active inactive first-install; do
    for fail_step in binary config wrapper unit reload enable restart health; do
      case "$variant" in
        active) setup_termrelay_case "${variant}-${fail_step}" active enabled 0 ;;
        inactive) setup_termrelay_case "${variant}-${fail_step}" inactive enabled 0 ;;
        first-install) setup_termrelay_case "${variant}-${fail_step}" inactive disabled 1 ;;
      esac
      FAIL_STEP="$fail_step"
      set +e
      install_staged_candidate "${INSTALL_STAGING_DIR}/termrelay" >"$output_file" 2>&1
      status=$?
      set -e
      [[ "$status" -eq 42 ]]
      grep -Fq 'installation failed with status 42; attempting rollback' "$output_file"
      case "$variant" in
        active) assert_termrelay_case_rolled_back active enabled ;;
        inactive) assert_termrelay_case_rolled_back inactive enabled ;;
        first-install) assert_termrelay_case_rolled_back inactive disabled ;;
      esac
      if [[ "$fail_step" != "binary" ]]; then
        [[ "$daemon_reload_calls" -ge 1 ]]
      fi
    done
  done

  setup_termrelay_case success active enabled 0
  FAIL_STEP=none
  install_staged_candidate "${INSTALL_STAGING_DIR}/termrelay"
  [[ "$(cat "$installed")" == "new-binary" ]]
  [[ "$(cat "$ENV_FILE")" == "new-env" ]]
  [[ "$(cat "$WRAPPER_FILE")" == "new-wrapper" ]]
  [[ "$(cat "$UNIT_FILE")" == "new-unit" ]]
  [[ "$(stat -c %a "$installed")" == "755" ]]
  [[ "$(stat -c %a "$ENV_FILE")" == "640" ]]
  [[ "$(stat -c %a "$WRAPPER_FILE")" == "755" ]]
  [[ "$(stat -c %a "$UNIT_FILE")" == "644" ]]
  [[ "$health_checks" -eq 2 ]]
  [[ "$(cat "$service_state")" == "active" ]]
  [[ "$(cat "$enabled_state")" == "enabled" ]]

  setup_termrelay_case rollback-failure active enabled 0
  FAIL_STEP=health
  ROLLBACK_RELOAD_FAIL=1
  set +e
  install_staged_candidate "${INSTALL_STAGING_DIR}/termrelay" >"$output_file" 2>&1
  status=$?
  set -e
  [[ "$status" -eq 42 ]]
  grep -Fq 'installation failed with status 42; attempting rollback' "$output_file"
  grep -Fq 'rollback after installation failure was incomplete; primary installation failure is preserved' "$output_file"
  assert_termrelay_case_rolled_back active enabled
)

run_test() {
  local test_name="$1"
  if [[ -n "${INSTALLER_TEST_FILTER:-}" && "$test_name" != "$INSTALLER_TEST_FILTER" ]]; then
    return 0
  fi
  printf '[installer-tests] %s\n' "$test_name"
  "$test_name"
}

run_test test_termd_installer_supports_stdin_pipe_execution
run_test test_installers_reject_removed_open_relay_flag
run_test test_termd_initial_pairing_uses_real_qr_command
run_test test_termrelay_install_reports_sensitive_setup_token
run_test test_termrelay_old_open_env_migrates_to_trusted_files
run_test test_termd_relay_verification_requires_matching_server_id
run_test test_termd_relay_verification_requires_setup_token
run_test test_termd_relay_curl_treats_hostile_values_as_data
run_test test_termd_relay_curl_rejects_crlf_inputs_without_disclosure
run_test test_termd_upgrade_skips_inherited_relay_verification_without_explicit_options
run_test test_termd_postinstall_health_timeout_is_failure
run_test test_termd_invalid_pairing_url_retries_full_relay_install
run_test test_termd_postinstall_pair_failure_is_failure
run_test test_termd_sensitive_curl_temp_files_are_removed_on_failure
run_test test_termd_sensitive_curl_stops_when_temp_directory_creation_fails
run_test test_installers_normalize_standard_proxy_environment
run_test test_termctl_embedded_self_binary_is_strict_and_isolated
run_test test_termrelay_embedded_self_binary_stages_in_isolation
run_test test_termd_embedded_self_binary_stages_in_isolation
run_test test_termd_fresh_install_initializes_schema_before_supervisor_baseline
run_test test_termd_reinstall_recovers_installer_poisoned_state
run_test test_termd_poisoned_state_repair_never_modifies_user_state
run_test test_termd_default_install_uses_managed_user
run_test test_termd_relay_registration_uses_only_daemon_token_and_public_key
run_test test_termd_upgrade_inherits_existing_user_without_user_arg
run_test test_termd_upgrade_uses_fixed_state_dir_when_existing_unit_has_no_working_directory
run_test test_termd_explicit_user_overrides_existing_service_user
run_test test_termd_proxy_arg_writes_common_proxy_env_vars
run_test test_termd_state_dir_change_clears_only_session_state
run_test test_termd_default_supervisor_version_uses_repository_version_file
run_test test_termd_supervisor_version_match_keeps_runtime_state
run_test test_termd_baked_supervisor_default_keeps_runtime_state
run_test test_termd_required_supervisor_version_mismatch_prompts_and_clears_runtime_state
run_test test_termd_missing_supervisor_meta_keeps_runtime_state_on_default_update
run_test test_termd_supervisor_version_mismatch_prompts_and_clears_runtime_state
run_test test_termd_supervisor_version_mismatch_decline_preserves_runtime_state
run_test test_termd_required_supervisor_version_mismatch_decline_preserves_runtime_state
run_test test_update_local_supervisor_version_mismatch_clears_runtime_state
run_test test_termd_clean_source_install_embeds_real_web_ui
run_test test_termrelay_clean_source_install_embeds_real_web_ui
run_test test_source_web_install_missing_npm_preserves_installed_binaries
run_test test_source_web_build_failure_preserves_installed_binaries
run_test test_source_web_partial_failures_preserve_binary_and_service
run_test test_installer_web_source_selection_matrix
run_test test_termd_install_transaction_rolls_back_every_failure_boundary
run_test test_termrelay_builds_all_install_candidates_in_isolated_staging
run_test test_termrelay_install_transaction_rolls_back_every_failure_boundary
run_test test_installers_reject_non_root_before_install
run_test test_prepare_release_direct_help
run_test test_prepare_release_validates_inputs_without_mutation
run_test test_prepare_release_rejects_dirty_release_paths
run_test test_prepare_release_success_isolated_and_exact
run_test test_prepare_release_clean_success_leaves_caller_read_only
run_test test_prepare_release_postcheck_staged_change_remains_read_only
run_test test_prepare_release_preserves_non_workspace_lock_entries
run_test test_prepare_release_rejects_verification_lockfile_mutation
run_test test_prepare_release_concurrent_caller_mutation_is_preserved
run_test test_prepare_release_ignores_plausible_hook_oid
run_test test_prepare_release_post_stage_query_failure_is_isolated
run_test test_prepare_release_branch_cas_failure_does_not_tag
run_test test_prepare_release_concurrent_formal_tag_does_not_create_candidate
run_test test_prepare_release_candidate_create_nonzero_is_verified
run_test test_prepare_release_conflicting_candidate_is_not_overwritten
run_test test_prepare_release_publication_partial_success_is_consistent
run_test test_prepare_release_cleanup_failure_preserves_primary_status
run_test test_prepare_release_atomic_push_failure_has_no_remote_partial_update
run_test test_prepare_release_atomic_push_success

printf 'installer tests passed\n'
