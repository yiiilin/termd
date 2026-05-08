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

for script in \
  scripts/install-termd.sh \
  scripts/install-termctl.sh \
  scripts/install-termrelay.sh
do
  bash -n "${ROOT_DIR}/${script}"
  assert_help_contains "$script" "--uninstall"
done

assert_help_contains scripts/install-termd.sh "--purge"
assert_help_contains scripts/install-termd.sh "--user <USER>"
assert_help_contains scripts/install-termrelay.sh "--purge"

printf 'installer tests passed\n'
