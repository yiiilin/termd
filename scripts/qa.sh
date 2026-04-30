#!/usr/bin/env bash

set -euo pipefail

# 统一 QA 入口：只运行本仓库已有验证命令，不安装系统依赖，也不写强审 checklist。
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'USAGE'
用法：bash scripts/qa.sh

从任意目录调用时，脚本会先切回仓库根目录，然后依次运行：
- Rust fmt 和 workspace 测试
- 本地 pairing CLI E2E：termd pair -> termctl pair
- termctl direct daemon E2E
- termrelay E2E
- relay runtime E2E：termd --relay -> termctl pair/new/list
- termui Web typecheck/test/build/e2e/audit
- termui Native Flutter analyze/test，或在缺少 Flutter/Dart 时运行 fallback 静态检查

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

port_8765_is_open() {
  (exec 9<>/dev/tcp/127.0.0.1/8765) >/dev/null 2>&1
}

port_is_open() {
  local port="$1"
  (exec 9<>"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1
}

wait_for_port() {
  local port="$1"
  local label="$2"

  for _ in $(seq 1 100); do
    if port_is_open "$port"; then
      return 0
    fi
    sleep 0.05
  done

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

run_pairing_cli_e2e() (
  set -euo pipefail

  local temp_dir daemon_pid token pair_stdout
  temp_dir="$(mktemp -d)"
  daemon_pid=""

  cleanup() {
    if [[ -n "$daemon_pid" ]]; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$temp_dir"
  }
  trap cleanup EXIT

  if port_8765_is_open; then
    printf '[termctl] 127.0.0.1:8765 已被占用，无法安全启动本地 termd pairing CLI E2E。\n' >&2
    printf '[termctl] 请停止已有 daemon 后重试，避免把 token 发给非本次测试进程。\n' >&2
    exit 1
  fi

  cargo run -q -p termd >"$temp_dir/termd.log" 2>&1 &
  daemon_pid="$!"

  for _ in $(seq 1 100); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      printf '[termctl] 本地 termd 未能启动，daemon 日志如下：\n' >&2
      cat "$temp_dir/termd.log" >&2
      exit 1
    fi
    if port_8765_is_open; then
      break
    fi
    sleep 0.05
  done

  if ! port_8765_is_open; then
    printf '[termctl] 等待本地 termd 监听 127.0.0.1:8765 超时。\n' >&2
    cat "$temp_dir/termd.log" >&2
    exit 1
  fi

  if ! token="$(cargo run -q -p termd -- pair --url http://127.0.0.1:8765 2>"$temp_dir/termd-pair.err")"; then
    printf '[termctl] termd pair 签发 token 失败：\n' >&2
    cat "$temp_dir/termd-pair.err" >&2
    exit 1
  fi

  case "$token" in
    termd-pair-*) ;;
    *)
      printf '[termctl] termd pair 输出不是预期 token 格式。\n' >&2
      exit 1
      ;;
  esac

  if ! pair_stdout="$(
    TERMD_CTL_STATE="$temp_dir/termctl-state.json" \
      cargo run -q -p termctl -- pair --token "$token" --url ws://127.0.0.1:8765/ws \
        2>"$temp_dir/termctl-pair.err"
  )"; then
    printf '[termctl] termctl pair 消费 token 失败：\n' >&2
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

  local relay_port relay_addr temp_dir relay_pid daemon_pid token server_id relay_client_url state_path new_stdout list_stdout
  relay_port="$(pick_free_port)"
  relay_addr="127.0.0.1:${relay_port}"
  temp_dir="$(mktemp -d)"
  relay_pid=""
  daemon_pid=""

  cleanup() {
    if [[ -n "$daemon_pid" ]]; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ -n "$relay_pid" ]]; then
      kill "$relay_pid" 2>/dev/null || true
      wait "$relay_pid" 2>/dev/null || true
    fi
    rm -rf "$temp_dir"
  }
  trap cleanup EXIT

  if port_8765_is_open; then
    printf '[termrelay] 127.0.0.1:8765 已被占用，无法安全启动本地 termd relay E2E。\n' >&2
    exit 1
  fi
  if port_is_open "$relay_port"; then
    printf '[termrelay] 127.0.0.1:%s 已被占用，无法安全启动本地 termrelay E2E。\n' "$relay_port" >&2
    exit 1
  fi

  cargo run -q -p termrelay -- --listen "$relay_addr" >"$temp_dir/termrelay.log" 2>&1 &
  relay_pid="$!"
  if ! wait_for_port "$relay_port" "termrelay"; then
    cat "$temp_dir/termrelay.log" >&2
    exit 1
  fi

  cargo run -q -p termd -- --relay "ws://${relay_addr}" >"$temp_dir/termd-relay.log" 2>&1 &
  daemon_pid="$!"

  for _ in $(seq 1 100); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      printf '[termrelay] termd --relay 过早退出，daemon 日志如下：\n' >&2
      cat "$temp_dir/termd-relay.log" >&2
      exit 1
    fi
    if port_8765_is_open; then
      break
    fi
    sleep 0.05
  done
  if ! port_8765_is_open; then
    printf '[termrelay] 等待 termd --relay 监听 127.0.0.1:8765 超时。\n' >&2
    cat "$temp_dir/termd-relay.log" >&2
    exit 1
  fi

  token="$(cargo run -q -p termd -- pair --url http://127.0.0.1:8765 2>"$temp_dir/termd-pair.err")"
  case "$token" in
    termd-pair-*) ;;
    *)
      printf '[termrelay] termd pair 输出不是预期 token 格式。\n' >&2
      exit 1
      ;;
  esac

  server_id="$(python3 - <<'PY'
import json
import urllib.request
with urllib.request.urlopen("http://127.0.0.1:8765/healthz", timeout=2) as response:
    print(json.load(response)["server_id"])
PY
)"
  relay_client_url="ws://${relay_addr}/ws/${server_id}/client"
  state_path="$temp_dir/termctl-state.json"

  TERMD_CTL_STATE="$state_path" cargo run -q -p termctl -- pair --token "$token" --url "$relay_client_url" >"$temp_dir/termctl-pair.out" 2>"$temp_dir/termctl-pair.err"
  new_stdout="$(TERMD_CTL_STATE="$state_path" cargo run -q -p termctl -- new --url "$relay_client_url" -- /bin/sh -lc 'printf relay-e2e-ready; sleep 1' 2>"$temp_dir/termctl-new.err")"
  list_stdout="$(TERMD_CTL_STATE="$state_path" cargo run -q -p termctl -- list --url "$relay_client_url" 2>"$temp_dir/termctl-list.err")"

  if [[ "$new_stdout" != session=* ]]; then
    printf '[termrelay] termctl new 未通过 relay 返回 session。\n' >&2
    exit 1
  fi
  if [[ "$list_stdout" != session=* ]]; then
    printf '[termrelay] termctl list 未通过 relay 返回 session。\n' >&2
    exit 1
  fi

  printf '[termrelay] runtime relay E2E 通过。\n'
)

section "rust" "cargo fmt --all -- --check"
cargo fmt --all -- --check

section "rust" "cargo test --workspace"
cargo test --workspace

section "termctl" "pairing CLI E2E"
run_pairing_cli_e2e

section "termctl" "direct daemon E2E"
cargo test -p termctl --test direct_daemon_e2e

section "termrelay" "relay E2E"
cargo test -p termrelay --test relay_e2e

section "termrelay" "runtime relay E2E"
run_relay_runtime_e2e

section "termui-web" "安装前端依赖检查"
if [[ ! -d termui/frontend/node_modules ]]; then
  (cd termui/frontend && npm ci)
else
  printf '[termui-web] node_modules 已存在，跳过 npm ci。\n'
fi

section "termui-web" "npm run typecheck"
(cd termui/frontend && npm run typecheck)

section "termui-web" "npm run test -- --run"
(cd termui/frontend && npm run test -- --run)

section "termui-web" "npm run build"
(cd termui/frontend && npm run build)

section "termui-web" "npm run test:e2e"
(cd termui/frontend && npm run test:e2e)

section "termui-web" "npm audit --audit-level=high"
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
