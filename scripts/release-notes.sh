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
  0.2.0)
    cat <<'EOF'
termd 0.2.0

用户可见变化:
- 通信协议升级为 packet v3，所有主要操作统一走带 request id、stream id、错误包、取消包和流控 credit 的 E2EE 内层包；Web、termctl、direct daemon 和 relay 路径使用同一套协议形状。
- 终端 attach/create 改为流式 packet，终端输出带序号和 credit，客户端关闭会发送 cancel，后续扩展一次性请求和流式请求不再需要新增外层消息格式。
- E2EE 握手绑定 daemon 公开身份：daemon 会签名自己的 X25519 key exchange，Web 和 termctl 会校验 daemon public key；auth 签名同时绑定当前 E2EE transcript，降低 relay 转发挑战或跨连接复用的风险。
- pairing invite/QR 现在携带 daemon public key；客户端不会再在新配对流程里猜测 daemon 身份。
- relay 与 direct transport 增加路由前置握手超时、pong/idle 超时、发送截止时间和帧大小限制；relay 仍只做 dumb pipe，不解密也不解析业务 packet。
- Web 端切换到不可用 daemon 时会更快回到后台管理页，修复 WebSocket 已关闭但连接等待直到完整超时的竞态。

兼容性:
- 0.2.0 是协议不兼容版本；0.1.x 的 Web、termctl 或 daemon 不能和 0.2.0 daemon 混用，需要 daemon、termctl 和 Web UI 同步更新。
- supervisor 兼容版本未更新，仍为 `0.1.0`；按现有本地更新原则，普通 termd 更新不应终止或清空已有 session supervisor。
- relay 继续保持不可信 dumb pipe，不引入业务权限判断；建议 relay 与 daemon 同步升级以获得新的 transport 超时和大小限制。
EOF
    ;;
  0.1.34)
    cat <<'EOF'
termd 0.1.34

用户可见变化:
- 终端新增搜索入口，搜索结果会在 xterm 渲染层高亮，并支持上一个/下一个结果跳转；搜索计数区域重新布局，关闭按钮不再遮挡结果文字。
- daemon 支持在当前内存中的终端 screen snapshot 内搜索文本；搜索不会把 PTY 明文写入 SQLite 或状态文件。
- Git panel 支持查看 worktree 或单个文件的 diff，使用只读编辑器打开；仍保留 Stage、Unstage、Discard 和打开文件能力。
- 文件 panel 去掉复制/移动入口，Git panel 去掉 commit/stash 入口；对应浏览器协议与测试桩也同步收口，避免留下未展示的写操作入口。
- 设置里新增浏览器通知和移动端快捷键配置；移动端快捷栏可叠加自定义按钮，后台 session 有新输出时可按偏好触发浏览器通知。
- 关闭 session 后会清理不可再次打开的 closed 展示记录；daemon 启动时也会清理无 live supervisor 保护的 closed 记录，避免列表里长期残留无法打开的 session。
- 本地源码更新新增 `scripts/update-local-termd.sh`，会在重启主 daemon 前后校验 supervisor PID、running session 计数和 healthz，避免普通 termd 更新误杀现有 session。

兼容性:
- 这是 daemon/Web UI/协议更新，不要求 supervisor 兼容版本升级；默认本地更新不应终止或清空现有 session。
- 终端搜索只覆盖 daemon 当前保留的内存 screen snapshot，不是跨历史日志的全量检索。
- 浏览器通知需要当前浏览器授权；未授权或不支持 Notification API 时不会影响终端主链路。
EOF
    ;;
  0.1.33)
    cat <<'EOF'
termd 0.1.33

用户可见变化:
- Web 客户端新增设置入口，可在 Daemons、Clients 旁和移动端菜单中配置语言与主题；语言支持自动、简体中文和英文，主题支持跟随系统、深色和浅色。
- Web 界面完成主要面板国际化覆盖，管理页、工作台、文件/Git panel、二维码配对、文件编辑器、状态栏和常见错误提示会随语言设置切换。
- 深色和浅色主题统一改为 Everforest soft 风格；xterm、Monaco、文件/Git panel、状态栏、弹窗和移动端快捷栏都会随主题切换，浅色主题避免纯白刺眼，深色主题避免霓虹高对比黑绿。
- 移动端终端增强粘贴支持，系统粘贴事件和快捷栏粘贴按钮都可向终端发送剪贴板文本；快捷操作栏可横向滑动，不再压缩按钮宽度。
- 文件侧栏标题去掉右侧路径文本，减少窄屏和长路径下的标题拥挤。

兼容性:
- 这是 Web UI/本地浏览器偏好更新，不要求 supervisor 兼容版本升级；默认安装更新不应终止或清空现有 session。
- 语言和主题偏好只保存在当前浏览器本地状态，不写入 daemon，也不影响其他客户端。
- 移动端剪贴板读取受浏览器权限和安全上下文限制；权限不可用时仍保留原生粘贴事件兜底。
EOF
    ;;
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
