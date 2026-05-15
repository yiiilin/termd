#!/usr/bin/env bash

set -euo pipefail

# 集中维护发版说明，保证本地 tag message 和 GitHub Release 使用同一份用户可见摘要。
# 如需新增版本，只改这里；prepare-release 和 CI 都会复用这个输出。

version="${1:-}"
[[ -n "$version" ]] || {
  printf 'usage: %s <version>\n' "$0" >&2
  exit 2
}

case "$version" in
  0.1.30)
    cat <<'EOF'
termd 0.1.30

用户可见变化:
- 普通 termd 更新不再清空或终止现有 session supervisor；只要 supervisor 兼容版本没有显式升级，已有 session 会继续保留。
- 如果显式升级 supervisor 兼容版本，安装器会先提示 session 会丢失；用户拒绝则退出，用户确认后才会停 daemon、终止旧 supervisor 并清空 session 状态，避免旧 session 被新 daemon 重新恢复。
- 重启 termd 后会从仍在线的 session supervisor 修复运行态 session，保留 session 名称和排列顺序。
- Web 端修复移动端键盘模式布局：快捷键栏贴近输入法，状态栏/快捷键栏宽度固定，状态指标贴近主机名排列。
- Web 端移动方向手势改为三档速度：一档每 0.5 秒 1 个方向键，二档每 0.5 秒 2 个方向键，三档保持快速移动。
- Web 端多客户端同时打开同一 session 时，非 resize owner 只显示缩放查看，不再争抢 PTY resize；独占移动端 session 不再显示多余虚线框。
- relay 连接支持通过常见代理环境变量使用 HTTP/SOCKS5 代理，包括 HTTP_PROXY、HTTPS_PROXY、ALL_PROXY、NO_PROXY。

兼容性:
- 这是 daemon/Web UI/安装器更新，不要求 supervisor 兼容版本升级；按默认安装流程更新不应丢失现有 session。
- 只有显式传入 --supervisor-version 或 TERMD_SUPERVISOR_VERSION 且版本变更时，才会进入需要确认的 supervisor 升级清理流程。
EOF
    ;;
  *)
    cat <<EOF
termd ${version}

用户可见变化:
- 请在 scripts/release-notes.sh 中补充此版本的功能、修复和兼容性说明。
EOF
    ;;
esac
