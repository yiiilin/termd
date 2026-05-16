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
  0.1.32)
    cat <<'EOF'
termd 0.1.32

用户可见变化:
- Web 文件侧栏新增 Git 视图，Files/Git 可在同一 panel 内切换；Git 视图展示当前 session cwd 所在仓库的未提交变更和提交图。
- Git Changes 支持多 worktree/分支折叠展示，分支和文件采用文件树层级缩进；溢出文本可 hover 查看完整路径或名称。
- Git 变更文件支持打开文件、Stage、Unstage 和 Discard 操作；Discard 使用撤回图标，操作按钮以 hover 浮层显示，不挤占文件名空间。
- Git Graph 改为更接近 Source Control Graph 的彩色 lane 视图，并支持通过 Changes 与 Graph 之间的横向分隔条上下拖动调整区域高度。
- 文件侧栏与终端之间的宽度调整改为直接拖动 panel 左边框，不再额外显示一条独立拖动线。

兼容性:
- 这是 daemon/Web UI/协议扩展更新，不要求 supervisor 兼容版本升级；默认安装更新不应终止或清空现有 session。
- Git 视图依赖 session 当前 cwd 可被 daemon 读取，并依赖本机 `git` CLI；非 Git 目录会在 Git panel 内显示不可用或空仓库状态。
EOF
    ;;
  0.1.31)
    cat <<'EOF'
termd 0.1.31

用户可见变化:
- Web 文件列表新增“Follow terminal cwd”跟随选项，默认开启；打开 session 后会每 1 秒跟随终端当前目录，终端里 `cd` 后文件列表会自动切换位置。
- 文件列表跟随终端 cwd 时，daemon 会优先读取 PTY 主进程当前目录；在可读取 cwd 的 Linux 环境下，即使终端切到初始 session root 外，也能按当前目录展示文件。
- 底部主机状态栏重新调整宽度策略，CPU、内存、磁盘和网络指标不再随内容动态挤压折叠；窄桌面会按优先级隐藏次要指标，移动端宽度保持稳定。
- 移动端软键盘未打开时隐藏 Tab、Esc、^C、^Z 等快捷输入栏；软键盘打开后快捷栏仍贴近键盘显示。

兼容性:
- 这是 daemon/Web UI 更新，不要求 supervisor 兼容版本升级；默认安装更新不应终止或清空现有 session。
- 终端 cwd 读取当前主要支持 Linux `/proc/<pid>/cwd`；不支持或权限不足时会回退到已保存的文件列表路径。
EOF
    ;;
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
