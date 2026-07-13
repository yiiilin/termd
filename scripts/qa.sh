#!/usr/bin/env bash

set -euo pipefail

# 统一 QA 入口：只运行本仓库已有验证命令，不安装系统依赖，也不写强审 checklist。
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'USAGE'
用法：bash scripts/qa.sh

从任意目录调用时，脚本会先切回仓库根目录，然后依次运行：
- shell 脚本语法检查
- Rust fmt 和 workspace 测试
- 本地 pairing CLI E2E：termd pair -> termctl pair
- termctl direct daemon E2E
- termrelay E2E
- relay runtime E2E：termd --relay -> termctl pair/new/list
- termui Web npm ci/typecheck/test/build/e2e/audit
- termui Native Flutter analyze/test，或在缺少 Flutter/Dart 时运行 fallback 静态检查

默认每次都会在 termui/frontend 运行 npm ci；只有显式设置 TERMD_QA_SKIP_NPM_CI=1 才会跳过。
脚本不会安装 Flutter/Dart/Playwright 浏览器，不会启动外部服务，也不会写 checklist。
USAGE
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

cd "$REPO_ROOT"

section() {
  printf '\n[%s] %s\n' "$1" "$2"
}

require_cmd() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    printf '[qa] 缺少必需命令：%s\n' "$name" >&2
    exit 127
  fi
}

run_native_fallback_scan() {
  section "termui-native" "Flutter/Dart 不在 PATH，执行 Native fallback 结构与敏感字符串检查"

  test -d termui/native
  test -f termui/native/pubspec.yaml
  test -f termui/native/lib/main.dart

  section "termui-native" "敏感字符串扫描：打印命中供 reviewer 分类"
  rg -n "pairing_token|server_private_key|terminal_transcript|terminal transcript|session_data|pty_output|SharedPreferences|localStorage|writeAsString|File\\(" termui/native || true

  section "termui-native" "敏感字符串扫描：生产路径自动阻断明显不安全命中"
  scan_for_unexpected "pairing_token|server_private_key|terminal_transcript|terminal transcript|pty_output" "^termui/native/lib/core/errors/native_error\\.dart:"
  scan_for_unexpected "session_data" "^(termui/native/lib/core/errors/native_error\\.dart|termui/native/lib/core/protocol/protocol_types\\.dart):"
  scan_for_unexpected "SharedPreferences|localStorage|writeAsString|File\\(" "^$"

  section "termui-native" "UI 层边界检查：app/features 不应直连 storage 或协议细节"
  if rg -n "SecureStorage|SecureStorageKeys|device_signing_key_secret|JsonEnvelope|ProtocolMessageType|pair_request|session_data|control_request|control_grant" termui/native/lib/app termui/native/lib/features; then
    printf '[termui-native] app/features 层出现 storage 或协议细节直连，请人工复核。\n' >&2
    exit 1
  fi
}

scan_for_unexpected() {
  local pattern="$1"
  local allowed_regex="$2"
  local output
  local unexpected

  output="$(rg -n "$pattern" termui/native/lib termui/native/pubspec.yaml || true)"
  if [[ -z "$output" ]]; then
    return 0
  fi

  unexpected="$(printf '%s\n' "$output" | awk -v allowed="$allowed_regex" '$0 !~ allowed { print }')"
  if [[ -n "$unexpected" ]]; then
    printf '%s\n' "$unexpected" >&2
    printf '[termui-native] 发现未被允许的生产路径敏感字符串命中。\n' >&2
    exit 1
  fi
}

require_cmd cargo
require_cmd npm
require_cmd rg

section "shell" "bash -n scripts/*.sh"
bash -n scripts/*.sh

port_is_open() {
  local port="$1"
  (exec 9<>"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1
}

wait_for_port() {
  local port="$1"
  local label="$2"

  # 预留更长窗口，避免完整 QA 下的 `cargo run` 冷启动、构建锁等待或首次初始化
  # 把刚启动的监听服务误判为失败。
  for _ in $(seq 1 600); do
    if port_is_open "$port"; then
      return 0
    fi
    sleep 0.05
  done

  if port_is_open "$port"; then
    return 0
  fi

  printf '[qa] 等待 %s 监听 127.0.0.1:%s 超时。\n' "$label" "$port" >&2
  return 1
}

pick_free_port() {
  python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

cargo_run_in_temp_dir() (
  local temp_dir="$1"
  shift

  cd "$temp_dir"
  cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -q "$@"
)

debug_binary_path() {
  local binary_name="$1"
  local target_dir="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
  case "$target_dir" in
    /*) ;;
    *) target_dir="${REPO_ROOT}/${target_dir}" ;;
  esac
  printf '%s/debug/%s\n' "$target_dir" "$binary_name"
}

start_process_in_dir() {
  local work_dir="$1"
  local log_path="$2"
  shift 2

  (
    cd "$work_dir"
    exec "$@"
  ) >"$log_path" 2>&1 &
  printf '%s\n' "$!"
}

run_pairing_cli_e2e() (
  set -euo pipefail

  local temp_dir daemon_pid daemon_port invite_code invite_file pair_stdout daemon_url
  temp_dir="$(mktemp -d)"
  daemon_pid=""
  daemon_port="$(pick_free_port)"
  daemon_url="http://127.0.0.1:${daemon_port}"

  cleanup() {
    if [[ -n "$daemon_pid" ]]; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$temp_dir"
  }
  trap cleanup EXIT

  # daemon 默认使用当前工作目录下的 daemon-state.json；QA 必须隔离到临时目录，
  # 避免恢复开发环境或上一次失败留下的持久 session。
  daemon_pid="$(start_process_in_dir "$temp_dir" "$temp_dir/termd.log" "$(debug_binary_path termd)" --listen "127.0.0.1:${daemon_port}")"

  for _ in $(seq 1 200); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      printf '[termctl] 本地 termd 未能启动，daemon 日志如下：\n' >&2
      cat "$temp_dir/termd.log" >&2
      exit 1
    fi
    if port_is_open "$daemon_port"; then
      break
    fi
    sleep 0.05
  done

  if ! port_is_open "$daemon_port"; then
    printf '[termctl] 等待本地 termd 监听 %s 超时。\n' "$daemon_url" >&2
    cat "$temp_dir/termd.log" >&2
    exit 1
  fi

  if ! pair_stdout="$(
    cargo_run_in_temp_dir "$temp_dir" -p termd -- pair --qr --url "$daemon_url" 2>"$temp_dir/termd-pair.err"
  )"; then
    printf '[termctl] termd pair 签发 invite 失败：\n' >&2
    cat "$temp_dir/termd-pair.err" >&2
    exit 1
  fi

  invite_code="$(
    printf '%s\n' "$pair_stdout" | rg -o '^termd-pair:v2:[^[:space:]]+' | tail -n1
  )"

  case "$invite_code" in
    termd-pair:v2:*) ;;
    *)
      printf '[termctl] termd pair 输出不是预期 invite 格式。\n' >&2
      exit 1
      ;;
  esac
  invite_file="$temp_dir/pair-invite"
  printf '%s\n' "$invite_code" >"$invite_file"
  chmod 0600 "$invite_file"

  if ! pair_stdout="$(
    TERMD_CTL_STATE="$temp_dir/termctl-state.json" \
      cargo run -q -p termctl -- pair --payload-file "$invite_file" --url "$daemon_url" \
        2>"$temp_dir/termctl-pair.err"
  )"; then
    printf '[termctl] termctl pair 消费 invite 失败：\n' >&2
    cat "$temp_dir/termctl-pair.err" >&2
    exit 1
  fi

  if [[ "$pair_stdout" != paired\ server=* ]]; then
    printf '[termctl] termctl pair 输出不是预期配对结果。\n' >&2
    exit 1
  fi

  printf '[termctl] pairing CLI E2E 通过。\n'
)

run_relay_runtime_e2e() (
  set -euo pipefail

  local relay_port relay_addr relay_url daemon_port temp_dir relay_pid daemon_pid state_path new_stdout list_stdout close_stdout session_id
  local setup_token setup_token_file daemon_token daemon_token_file registry_file server_id daemon_public_key register_payload register_curl_config
  local pairing_invite pairing_invite_file pairing_output pair_succeeded
  relay_port="$(pick_free_port)"
  relay_addr="127.0.0.1:${relay_port}"
  relay_url="http://${relay_addr}"
  daemon_port="$(pick_free_port)"
  temp_dir="$(mktemp -d)"
  relay_pid=""
  daemon_pid=""

  cleanup() {
    local exit_status=$?
    local cleanup_list="" cleanup_session="" cleanup_termctl=""
    set +e

    cleanup_termctl="$(debug_binary_path termctl)"
    if [[ -n "${daemon_pid:-}" ]] \
      && kill -0 "$daemon_pid" 2>/dev/null \
      && [[ -n "${state_path:-}" && "$state_path" == "${temp_dir:-}/"* && -f "$state_path" ]] \
      && [[ -x "$cleanup_termctl" ]]; then
      cleanup_list="$(timeout 5s "$cleanup_termctl" --state "$state_path" list --url "http://127.0.0.1:${daemon_port}" 2>/dev/null)"
      while read -r cleanup_session _; do
        if [[ "$cleanup_session" =~ ^session=([[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12})$ ]]; then
          timeout 5s "$cleanup_termctl" --state "$state_path" close "${BASH_REMATCH[1]}" --url "http://127.0.0.1:${daemon_port}" >/dev/null 2>&1 || true
        fi
      done <<<"$cleanup_list"
    fi

    if [[ -n "${daemon_pid:-}" ]]; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ -n "${relay_pid:-}" ]]; then
      kill "$relay_pid" 2>/dev/null || true
      wait "$relay_pid" 2>/dev/null || true
    fi
    if [[ -n "${temp_dir:-}" ]]; then
      rm -rf "$temp_dir"
    fi
    return "$exit_status"
  }
  trap cleanup EXIT

  if port_is_open "$relay_port"; then
    printf '[termrelay] 127.0.0.1:%s 已被占用，无法安全启动本地 termrelay E2E。\n' "$relay_port" >&2
    exit 1
  fi
  if port_is_open "$daemon_port"; then
    printf '[termrelay] 127.0.0.1:%s 已被占用，无法安全启动本地 termd relay E2E。\n' "$daemon_port" >&2
    exit 1
  fi

  setup_token="termd-qa-relay-setup-token-$(date +%s)-$(printf '%s' "$relay_port" | sha256sum | cut -c1-16)"
  daemon_token="termd-qa-daemon-token-$(date +%s)-$(printf '%s' "$daemon_port" | sha256sum | cut -c1-16)"
  setup_token_file="$temp_dir/relay-setup-token"
  daemon_token_file="$temp_dir/daemon-token"
  registry_file="$temp_dir/daemon-registry.json"
  printf '%s\n' "$setup_token" >"$setup_token_file"
  printf '%s\n' "$daemon_token" >"$daemon_token_file"
  chmod 0600 "$setup_token_file" "$daemon_token_file"

  relay_pid="$(start_process_in_dir "$temp_dir" "$temp_dir/termrelay.log" "$(debug_binary_path termrelay)" --listen "$relay_addr" --setup-token-file "$setup_token_file" --daemon-registry "$registry_file")"
  if ! wait_for_port "$relay_port" "termrelay"; then
    cat "$temp_dir/termrelay.log" >&2
    exit 1
  fi

  # relay runtime E2E 同样需要隔离 daemon 状态，避免旧 session 恢复影响本轮监听启动。
  daemon_pid="$(start_process_in_dir "$temp_dir" "$temp_dir/termd-relay.log" "$(debug_binary_path termd)" --listen "127.0.0.1:${daemon_port}" --relay "ws://${relay_addr}" --relay-daemon-token-file "$daemon_token_file")"

  for _ in $(seq 1 200); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      printf '[termrelay] termd --relay 过早退出，daemon 日志如下：\n' >&2
      cat "$temp_dir/termd-relay.log" >&2
      exit 1
    fi
    if port_is_open "$daemon_port"; then
      break
    fi
    sleep 0.05
  done
  if ! port_is_open "$daemon_port"; then
    printf '[termrelay] 等待 termd --relay 监听 127.0.0.1:%s 超时。\n' "$daemon_port" >&2
    cat "$temp_dir/termd-relay.log" >&2
    exit 1
  fi

  server_id="$(
    python3 - "$temp_dir/daemon-state.sqlite" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
row = conn.execute("select value from daemon_meta where key = 'server_id'").fetchone()
if not row:
    raise SystemExit("missing daemon server_id")
print(row[0])
PY
  )"
  register_payload="$temp_dir/register-daemon.json"
  daemon_public_key="$(curl -fsS "http://127.0.0.1:${daemon_port}/healthz" | python3 -c 'import json,sys; print(json.load(sys.stdin)["daemon_public_key"])')"
  python3 - "$server_id" "$daemon_public_key" "$daemon_token_file" >"$register_payload" <<'PY'
import json
import sys

with open(sys.argv[3], "r", encoding="utf-8") as token_file:
    daemon_token = token_file.readline().strip()
print(json.dumps({"server_id": sys.argv[1], "daemon_token": daemon_token, "daemon_public_key": sys.argv[2]}, separators=(",", ":")))
PY
  chmod 0600 "$register_payload"
  # 0.7 registry 只注册 daemon token 与 public key；setup token 通过 curl config
  # 传递，避免真实凭据出现在进程 argv、URL 或日志。
  register_curl_config="$temp_dir/register-daemon.curl"
  {
    printf 'url = "%s/api/relay/daemon/register"\n' "$relay_url"
    printf 'request = "POST"\n'
    printf 'fail\n'
    printf 'silent\n'
    printf 'show-error\n'
    printf 'header = "content-type: application/json"\n'
    printf 'header = "x-termd-relay-setup-token: %s"\n' "$setup_token"
    printf 'data-binary = "@%s"\n' "$register_payload"
  } >"$register_curl_config"
  chmod 0600 "$register_curl_config"
  if ! curl --config "$register_curl_config" >/dev/null; then
    printf '[termrelay] trusted relay daemon 注册失败。\n' >&2
    cat "$temp_dir/termrelay.log" >&2
    exit 1
  fi

  state_path="$temp_dir/termctl-state.json"
  pair_succeeded=0
  for _ in $(seq 1 80); do
    pairing_output="$("$(debug_binary_path termd)" pair --url "http://127.0.0.1:${daemon_port}" --qr)"
    pairing_invite="$(printf '%s\n' "$pairing_output" | awk '/^termd-pair:v2:/{print; exit}')"
    pairing_invite_file="$temp_dir/relay-pair-invite"
    printf '%s\n' "$pairing_invite" >"$pairing_invite_file"
    chmod 0600 "$pairing_invite_file"
    python3 - "$pairing_invite_file" <<'PY'
import base64
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as invite_file:
    invite_code = invite_file.readline().strip()
if not invite_code:
    raise SystemExit("missing v2 invite")
raw = invite_code.split(":", 2)[2]
raw += "=" * (-len(raw) % 4)
payload = json.loads(base64.urlsafe_b64decode(raw.encode()).decode())
if payload.get("version") != 2:
    raise SystemExit("unexpected invite version")
PY
    if [[ "$pairing_invite" != termd-pair:v2:* ]]; then
      printf '[termrelay] daemon 本地 pairing 响应未构造出预期 invite payload。\n' >&2
      exit 1
    fi

    rm -f "$state_path"
    if cargo run -q -p termctl -- --state "$state_path" pair --payload-file "$pairing_invite_file" --url "$relay_url" >"$temp_dir/termctl-pair.out" 2>"$temp_dir/termctl-pair.err" \
      && [[ "$(cat "$temp_dir/termctl-pair.out")" == paired\ server=* ]]; then
      pair_succeeded=1
      break
    fi
    sleep 0.25
  done
  if [[ "$pair_succeeded" -ne 1 ]]; then
    printf '[termrelay] termctl pair 未通过 trusted relay 完成。\n' >&2
    cat "$temp_dir/termctl-pair.out" >&2 || true
    cat "$temp_dir/termctl-pair.err" >&2
    exit 1
  fi
  if ! new_stdout="$(cargo run -q -p termctl -- --state "$state_path" new --url "$relay_url" -- /bin/sh -lc 'printf relay-e2e-ready; sleep 15' 2>"$temp_dir/termctl-new.err")"; then
    printf '[termrelay] termctl new 未通过 trusted relay 完成。\n' >&2
    printf '[termrelay] termctl new stdout: %s\n' "$new_stdout" >&2
    cat "$temp_dir/termctl-new.err" >&2
    exit 1
  fi
  if [[ "$new_stdout" != session=* ]]; then
    printf '[termrelay] termctl new 未通过 relay 返回 session。\n' >&2
    printf '[termrelay] termctl new stdout: %s\n' "$new_stdout" >&2
    cat "$temp_dir/termctl-new.err" >&2
    exit 1
  fi
  session_id="${new_stdout#session=}"
  session_id="${session_id%% *}"
  if [[ -z "$session_id" ]]; then
    printf '[termrelay] 无法从 termctl new 输出解析 session ID。\n' >&2
    printf '[termrelay] termctl new stdout: %s\n' "$new_stdout" >&2
    exit 1
  fi
  if ! list_stdout="$(cargo run -q -p termctl -- --state "$state_path" list --url "$relay_url" 2>"$temp_dir/termctl-list.err")"; then
    printf '[termrelay] termctl list 未通过 trusted relay 完成。\n' >&2
    printf '[termrelay] termctl list stdout: %s\n' "$list_stdout" >&2
    cat "$temp_dir/termctl-list.err" >&2
    exit 1
  fi

  if ! printf '%s\n' "$list_stdout" | awk -v expected="session=$session_id" '$1 == expected { found = 1 } END { exit found ? 0 : 1 }'; then
    printf '[termrelay] termctl list 未通过 relay 返回新建 session。\n' >&2
    printf '[termrelay] termctl new stdout: %s\n' "$new_stdout" >&2
    printf '[termrelay] termctl list stdout: %s\n' "$list_stdout" >&2
    cat "$temp_dir/termctl-list.err" >&2
    exit 1
  fi
  if ! close_stdout="$(cargo run -q -p termctl -- --state "$state_path" close "$session_id" --url "$relay_url" 2>"$temp_dir/termctl-close.err")"; then
    printf '[termrelay] termctl close 未通过 trusted relay 完成。\n' >&2
    printf '[termrelay] termctl close stdout: %s\n' "$close_stdout" >&2
    cat "$temp_dir/termctl-close.err" >&2
    exit 1
  fi
  if [[ "$close_stdout" != "closed session=$session_id" ]]; then
    printf '[termrelay] termctl close 未确认新建 session 已关闭。\n' >&2
    printf '[termrelay] termctl close stdout: %s\n' "$close_stdout" >&2
    cat "$temp_dir/termctl-close.err" >&2
    exit 1
  fi

  printf '[termrelay] runtime relay E2E 通过。\n'
)

section "rust" "cargo fmt --all -- --check"
cargo fmt --all -- --check

section "rust" "cargo test --workspace --locked"
cargo test --workspace --locked

section "rust" "cargo build --locked -p termd -p termrelay"
cargo build --locked -p termd -p termrelay

section "installers" "scripts/test-installers.sh"
bash scripts/test-installers.sh

section "termctl" "pairing CLI E2E"
run_pairing_cli_e2e

section "termctl" "direct daemon E2E"
cargo test -p termctl --test direct_daemon_e2e

section "termrelay" "relay E2E"
cargo test -p termrelay --test relay_e2e

section "termrelay" "runtime relay E2E"
run_relay_runtime_e2e

section "termui-web" "npm ci"
if [[ "${TERMD_QA_SKIP_NPM_CI:-}" == "1" ]]; then
  printf '[termui-web] TERMD_QA_SKIP_NPM_CI=1，显式跳过 npm ci。\n'
else
  (cd termui/frontend && npm ci)
fi

section "termui-web" "npm run typecheck"
(cd termui/frontend && npm run typecheck)

section "termui-web" "npm run test -- --run"
(cd termui/frontend && npm run test -- --run)

section "termui-web" "npm run build"
(cd termui/frontend && npm run build)

section "termui-web" "npm run test:e2e"
# 中文注释：release QA 会同时拉起真实 termd/termrelay、浏览器和 Vite preview。
# 在 CI 小机器上并发跑这些重型 E2E 容易触发本机端口/网络状态抖动，发布验收优先要稳定可复现。
(cd termui/frontend && npm run test:e2e -- --workers=1)

section "termui-web" "npm audit --audit-level=high"
# npm audit 需要 registry advisory 数据；空缓存下的 --offline 会无条件漏报。
(cd termui/frontend && npm audit --audit-level=high)

if command -v flutter >/dev/null 2>&1 && command -v dart >/dev/null 2>&1; then
  section "termui-native" "Flutter/Dart 已存在，运行真实 Native analyze/test"
  (cd termui/native && flutter pub get)
  (cd termui/native && flutter analyze)
  (cd termui/native && flutter test)
else
  printf '\n[termui-native] SKIP: Flutter 或 Dart 不在 PATH，未运行 flutter pub get/analyze/test/build。\n'
  run_native_fallback_scan
fi

section "qa" "完成"
