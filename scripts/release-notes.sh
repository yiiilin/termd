#!/usr/bin/env bash

set -euo pipefail

# 集中维护发版说明，保证本地 tag message 和 GitHub Release 使用同一份用户可见摘要。
# 如需新增版本，只改这里；prepare-release 和 CI 都会复用这个输出。

version="${1:-}"
[[ -n "$version" ]] || {
  printf 'usage: %s <version>\n' "$0" >&2
  exit 2
}

PLACEHOLDER_TEXT="请在 scripts/release-notes.sh 中补充此版本的功能、修复和兼容性说明。"

case "$version" in
  0.3.12)
    cat <<'EOF'
termd 0.3.12

用户可见变化:
- 修复 Web 终端新建 session 或 resize 后内容只在上方 24 行滚动、底部大片空白的问题；默认全屏 scroll region 会随 PTY 新高度扩展，direct 和公网 relay 下连续回车、`seq 1 120` 都已覆盖真实浏览器回归。
- Web 终端在 snapshot 重放完成后会按当前聚焦浏览器的真实容器尺寸补一次本地 fit，但不会向 xterm 注入额外控制字节；TUI、alternate screen、saved cursor 和 snapshot/tail 边界不会被前端改写。
- 旧 PWA/service worker 缓存会被主动清理并注销，避免浏览器继续运行旧 bundle 造成 WebSocket、attach 或终端渲染行为和 daemon/relay 版本不一致。
- relay 新增 `--auth-token-file` 和显式 `--http-tunnel` 开关；部署模板、Caddy/openresty 反代说明和 release QA 覆盖 relay HTTP tunnel/代理配置，relay 默认仍不额外暴露 HTTP tunnel 面。
- termctl、Web 和 daemon 共用更多协议 method/packet codec 定义；direct daemon E2EE、二进制 packet、文件/终端 stream 的兼容测试更完整，减少 CLI/Web/daemon 字符串协议漂移。
- native 设备身份存储收敛为单条版本化 secure-storage 记录，并会迁移旧的分散 device id/public key/signing secret；损坏或缺字段时会给出明确 native error，而不是继续使用半残身份。
- release 流程会校验前端 package/package-lock 版本、release notes 和完整 QA；打 tag 或 GitHub Release 时会复用同一份用户可见说明。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.11 更新到 0.3.12 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3，binary protocol version 仍为 2；建议 daemon、Web UI、termctl 和 termrelay 同步升级到 0.3.12。
- relay 仍是 dumb pipe，不解密、不解析 E2EE 业务明文；HTTP tunnel 必须显式开启，未开启时公网 relay 只提供原有 WebSocket relay/Web UI 行为。
- PWA 离线外壳缓存会被清理；升级后浏览器应从 daemon/relay 重新加载最新 Web UI，不再依赖旧缓存离线打开旧版本工作台。
EOF
    ;;
  0.3.11)
    cat <<'EOF'
termd 0.3.11

用户可见变化:
- 文件上传/下载新增 E2EE HTTP 传输路径；大文件不再通过 WebSocket RPC/base64 传输，relay 和 direct 下都可以走更接近原生 HTTP 的二进制 body。
- Web 文件上传改为 10MiB 分片、最多 2 并发提交；上传时会显示悬浮进度，切换 session 后进度不会立刻丢失，300MB 文件通过真实 relay 上传到 `/tmp` 已覆盖回归测试。
- daemon 端 HTTP 上传会直接在目标路径 `create_new + set_len`，再按 offset seek 写入目标文件；不再先写临时文件再 rename，避免大文件上传中途失败后目标状态不清晰。
- 上传中的目标文件会被 daemon active guard 保护；文件列表、编辑读写、删除、下载和 Git 操作都会避开或拒绝正在上传的目标，避免半成品文件被误读、覆盖或提交。
- 上传失败、浏览器取消、daemon 重启后的上传清理更保守：只有能证明目标仍是同一个上传文件时才删除；遇到文件被替换、hardlink alias 或文件 identity 缺失时 fail-closed，保留 guard/recovery 记录等待人工确认。
- relay 新增 HTTP file tunnel，但仍保持 dumb pipe：只转发允许的文件传输 route，不解密、不解析业务明文；client 断开会关闭对应 daemon data pipe，旧连接不会继续污染新上传或新终端连接。
- Web terminal 连接在浏览器失焦/后台时不再主动断开；只有浏览器 offline 才中断 terminal transport，并且 WebSocket 建连增加短超时重试/hedging，降低“切出去一会儿回来只发不收”的概率。
- RPC `file_read` / `file_write` 收口为内置文本编辑器的小文件通道；大文件传输必须走 HTTP E2EE 或 binary stream，避免再次把大文件塞回 JSON/base64。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.10 更新到 0.3.11 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3，但新增 HTTP E2EE 文件传输接口和 relay HTTP tunnel；为了使用 relay 大文件上传/下载，daemon、Web UI 和 termrelay 应同步升级到 0.3.11。
- 旧 daemon/relay 不支持 HTTP 文件端点时，Web 上传小文件可回退到旧 RPC/binary stream；HTTP 下载不做旧协议回退，需同步升级 daemon/relay/Web UI 才能使用新下载路径。
EOF
    ;;
  0.3.10)
    cat <<'EOF'
termd 0.3.10

用户可见变化:
- direct 和 relay 的终端输出推送 drain 增加短时间预算；多个大输出 session 快速切换时，单个连接不会长时间占住发送循环，输入和切换恢复更及时。
- 修复 resize、exit 等零字节 terminal frame 后，Web 终端可能等到下一次按键才刷新或滚到底部的问题；切换 session、调整尺寸和命令结束后的尾部内容会主动完成渲染收尾。
- Vite/Monaco 构建拆分为更明确的 lazy chunks，消除前端构建的大 chunk 警告；编辑器仍按需加载，不增加终端首屏主路径负担。
- relay 大帧发送日志降噪：快速发送的大 frame 不再刷 info 日志，真正慢发送或异常仍会保留可诊断日志。
- 补充 direct Web 和真实 relay Web 的大输出快速切换回归测试，覆盖 0.5 秒切换 20 次后仍能贴底、恢复状态和继续输入。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.9 更新到 0.3.10 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；relay 仍保持 dumb pipe，不解密、不解析业务明文。
- 本版本没有引入 xterm 动态 chunk sizing；主要变化集中在 daemon/relay 发送调度、Web 渲染收尾和前端构建拆包。
EOF
    ;;
  0.3.9)
    cat <<'EOF'
termd 0.3.9

用户可见变化:
- relay 架构从旧 daemon mux 收敛为 daemon control 长连接 + 每个 browser client 独立 daemon data 连接；client 只有在 data pipe 配对完成后才收到 route_ready，减少 relay 长时间使用和快速切换 session 后卡住或操作超时的情况。
- relay 进一步保持 dumb pipe：route prelude 后只原样转发 browser 与 daemon data 之间的 text/binary WebSocket frame，不再解析或封装 terminal/session 等业务内容。
- daemon 通过 control 线接收 OpenData 后为每个外部 client 反连独立 data pipe；client 断开时 relay 会关闭对应 data pipe 并通知 daemon 清理该 client 上下文，旧客户端不再影响新客户端。
- direct 和 relay 下的 Web 连接生命周期继续收敛：session 切换会取消正在进行的建连/auth/attach，后台状态轮询的 transient timeout 不再误伤当前终端。
- 移动端终端交互修复：中文输入组合态下按键处理更稳，长按终端/输入框可让系统复制粘贴菜单接管，键盘/viewport 变化时如果本来在底部会保持跟随底部。
- 终端滚动跟随修复：只有当前视图已经在 PTY 底部时，新的输出或布局变化才自动跟到底部；用户滚到历史时不会被无关点击强制刷新或拉回。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.8 更新到 0.3.9 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；relay route role 新增 `daemon_control` / `daemon_data`，daemon 与 relay 必须同步升级到 0.3.9 才能使用新 relay 架构。relay 仍不解密、不解析业务明文。
- 旧 `daemon_mux` route 在新 termrelay 中会被明确拒绝；旧 daemon 不能继续通过新版 relay 的生产路径连接。
EOF
    ;;
  0.3.8)
    cat <<'EOF'
termd 0.3.8

用户可见变化:
- daemon 端终端输出恢复改为 poll/cursor 模型：attach/create 只记录客户端上次看到的 terminal_seq，真正发送时再从 daemon 终端缓存或 supervisor 快照读取，减少旧 WebSocket、旧 session 切换和慢客户端对新连接的影响。
- Web 在 relay/direct 下快速切换多个大输出 session 时，不再依赖 per-client 预填 snapshot/tail 队列；旧连接断开后，新连接可直接按 cursor 获取 snapshot 或连续 tail，恢复路径更接近“重新接入终端”而不是“继续消费旧队列”。
- daemon 仍会持续消费 supervisor 输出并更新本地 terminal mirror/cache；relay 短暂断开或浏览器重新连接后，优先用 daemon cache 补画面，cache 不完整时再回源 supervisor 获取权威 snapshot。
- tail 跨过 resize 时会自动退回 snapshot 重绘，避免用旧分辨率的历史输出继续追加到新尺寸终端，降低切换不同分辨率客户端后画面错乱的概率。
- supervisor attach tail 遇到 resize 时也会选择 snapshot 恢复，保持 daemon cache、runtime fallback 和 supervisor 权威恢复规则一致。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.7 更新到 0.3.8 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；前端 wire protocol 不变，浏览器、termctl、daemon 和 relay 建议同步升级到 0.3.8。relay 仍是 dumb pipe，不解密、不解析业务明文。
EOF
    ;;
  0.3.7)
    cat <<'EOF'
termd 0.3.7

用户可见变化:
- 修复浏览器 attach / reconnect 时 snapshot 可能按旧分辨率写入 xterm 的问题；Web 会先按 snapshot 自带的 rows/cols 调整终端，再清屏重绘 snapshot，避免长行换行、光标位置和 TUI 画面在切换 session 后错乱。
- terminal tail 中的 resize frame 现在按 `terminal_seq` 原始顺序消费；如果 resize 前后都有输出，前面的输出会先写完，再应用 resize，再写后续输出，避免 resize 通过侧路提前改变 xterm 尺寸。
- Web 在收到 snapshot/resize frame 时会保留并传递权威 size，不再只依赖 session 列表里的旧 size；快速切换不同分辨率 session 后，首屏恢复更稳定。
- relay/direct 大输出链路继续使用 0.3.6 的批量传输和连接生命周期模型；本次同步保留此前修复的 relay backpressure、旧 client 清理、WebSocket 重建和尾包主动 repaint 行为。
- 本地和远端 relay 服务已按本版本代码验证过 healthz 与 daemon mux 重连路径。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.6 更新到 0.3.7 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；terminal frame 语义不变，只是 Web UI 更严格按 snapshot/resize 携带的 size 顺序渲染。建议 daemon、Web UI、termctl 和 relay 同步升级到 0.3.7。
EOF
    ;;
  0.3.6)
    cat <<'EOF'
termd 0.3.6

用户可见变化:
- relay 建立 route 后进一步收敛成 dumb pipe：不再由 relay 自己的 idle timer 或 WebSocket Pong 判定连接死亡，只按真实 close、读写错误和通道背压清理连接；后台浏览器、手机切出再回来、公网代理延迟 Pong 时不再容易误断。
- daemon 到 relay 的长连接继续由 daemon 空闲保活和 mux keepalive 维持；缺少 WebSocket Pong 不再立即重建 daemon mux，实际可用性由同一条 mux 数据通道里的 keepalive ack 与读写错误判断。
- relay 写侧不再为每个成功发送的 frame 回报 outcome 统计，避免大输出时形成额外“成功回报队列”；成功写入直接在 writer 侧记录，主循环只处理关闭/失败生命周期信号。
- Web 端切换 session 或重连时会关闭旧 WebSocket，并为新 session 重新认证和 attach；relay/daemon 可以直接通过 transport close 清理旧 client context，快速切换多个大输出 session 后不再让旧 client 包继续拖慢新 session。
- Web 侧边栏简化为固定标题和新建按钮，session 列表单独滚动；移除桌面侧边栏里的刷新和断开按钮，避免误操作和多余外层面板。
- 修复 xterm 最后一笔 live 输出可能等到下一次输入或 resize 才 repaint 的问题；输出停止后的尾包会主动刷新，不需要再按一下键才看到最后内容。
- 移动端软键盘弹出时不再把 visual viewport 变小上报成 PTY resize；终端分辨率保持不变，整个工作区按键盘高度上移，键盘收起后再按恢复后的高度上报尺寸。
- 移动端网络状态区域在窄屏下继续保持可读，避免键盘或侧栏改动后被挤压成不可见内容。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.5`；从 0.3.5 更新到 0.3.6 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；relay 仍不解密、不解析业务明文，只负责 route prelude 后转发 WebSocket frame。建议 daemon、Web UI、termctl 和 relay 同步升级到 0.3.6。
EOF
    ;;
  0.3.4)
    cat <<'EOF'
termd 0.3.4

用户可见变化:
- supervisor 现在会保留普通屏幕和替代屏幕的终端快照；daemon 或浏览器重新接入时，会先恢复权威屏幕状态，再继续追赶后续 tail，减少大输出 session 在重连后丢屏、跳屏或回到旧画面的情况。
- daemon 也维护会话级终端镜像缓存；新客户端加入、daemon 和 supervisor 断开重连、或多个客户端轮番 attach 时，都会优先使用本地缓存恢复画面，再对接 live tail。
- 终端流发送改成按字节累计的 chunk/credit 模式，并让 relay/direct 路径更偏向批量转发；大段输出不会再被过细的小片段打散，前端不再明显“一字一字蹦”。
- relay 仍然只是 dumb pipe，但在高延迟、抖动、双客户端和断连恢复场景下更稳；多个 session 轮番切换、长时间大量输出和客户端掉线重连都经过回归测试。
- Web、daemon、termctl 和 relay 都同步更新到 0.3.4，安装/更新时如果 supervisor 兼容版本不匹配，会升级 supervisor 并清空不兼容的 live session。

兼容性:
- 0.3.4 更新了 supervisor 兼容版本到 `0.3.4`；如果本地已有旧版 live supervisor，普通更新会触发重建并清空对应 session。
- packet / terminal frame 语义继续沿用 0.3.x 路线；relay 仍只是转发连接和路由，不解密也不解析明文。
EOF
    ;;
  0.3.3)
    cat <<'EOF'
termd 0.3.3

用户可见变化:
- 修复 Web 工作台频繁快速切换多个 session 后，迟到的 session list 刷新把选中态改回第一行或旧 session，导致用户点击其他 session 后马上跳回原 session 的问题。
- 点击 session 后会立即更新左侧列表选中态；真实 attach 完成后再切换 xterm 数据流，慢 attach、后台刷新或手动 Refresh 不再覆盖用户刚做出的选择。
- 切换 session 时 Web xterm 会随输出 reset 版本重建实例，并忽略旧实例迟到的 write callback；旧 session 的大量输出不再阻塞新 session 首屏，也不会把旧输出确认到新 session。
- 新增快速切 session / 迟到刷新回归测试，以及旧 xterm 异步 write callback 不能阻塞新 session 的回归测试。
- direct daemon、真实 relay、桌面浏览器和移动端浏览器路径都经过 smoke 验证。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.1`；从 0.3.2 更新到 0.3.3 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3；本次主要是 Web UI session 选择态与 xterm 切换稳定性修复。daemon、Web UI、termctl 和 relay 可同步升级到 0.3.3，relay 仍是 dumb pipe，不解密也不解析业务内容。
EOF
    ;;
  0.3.2)
    cat <<'EOF'
termd 0.3.2

用户可见变化:
- terminal stream 输出从“按 frame 数量限速”改为“按已渲染字节数补 credit”；大量小输出不再被少量 frame credit 人为卡住，突然输出大段内容时会更快成批出现在 Web 终端里。
- daemon 会把多个 terminal frame 合成一个 `batch` stream chunk 发送，同时保留每个 frame 原本的 `terminal_seq` 边界；浏览器仍按 snapshot/output/resize/exit 的顺序渲染和确认，不牺牲重连一致性。
- Web 端收到 batch 后会展开成独立 terminal frame，再按每帧实际字节数回补 credit；同一个传输 chunk 内的多帧输出会一起走 xterm 的批量写入路径，降低“一行一行蹦出来”的感觉。
- termctl 同步支持 terminal batch；如果同一个 batch 里包含输出和 exit，会先把退出前的输出写到 stdout，再结束 attach 流。
- 单个 snapshot/output 大于当前 credit 时，daemon 允许它独占一个 stream chunk 发送，并把 credit 饱和扣到 0，避免大 snapshot 因不可拆帧永久卡住。
- 本地源码更新、direct daemon、relay 中转、桌面和移动端浏览器路径都经过回归验证。

兼容性:
- supervisor 兼容版本未变化，仍为 `0.3.1`；从 0.3.1 更新到 0.3.2 不应终止或清空已有 live session supervisor。
- packet protocol version 仍为 3，但 terminal stream 新增 `terminal_frame.batch` payload；daemon、Web UI 和 termctl 建议同步升级到 0.3.2。relay 仍是 dumb pipe，不解密也不解析业务内容。
EOF
    ;;
  0.3.1)
    cat <<'EOF'
termd 0.3.1

用户可见变化:
- supervisor/daemon 的 attach 边界改成事务化 `AttachSync(last_terminal_seq)`；浏览器重新打开、切换或 daemon 重连到 live supervisor 时，会在同一个同步点拿到权威 snapshot 或 tail，减少大终端在重连附近丢输出、重放旧输出或重复清屏的问题。
- terminal tail 恢复现在严格按 session 级 `terminal_seq` 连续推进；snapshot 只负责重绘当前屏幕，之后的 output/resize/exit 都按 `base_seq` 之后的事件补齐，direct 和 relay 路径都复用这套语义。
- Web xterm 在连续收到大量 terminal frame 时会把多帧输出合并成更大的批量 `write`，但 render ack 仍按帧精确回补；大量输出不再明显“一行一行蹦出来”，大块终端刷新更快。
- 本地源码更新脚本现在会读取 `SUPERVISOR_VERSION` / SQLite `supervisor_version` 元数据；如果 supervisor 兼容版本未变化，继续保守热更新并校验 live supervisor PID 不变。
- 如果本地源码更新检测到 supervisor 兼容版本不匹配，会先停 daemon、终止旧 session supervisor、清空 runtime session 状态，再写入新的 `supervisor_version` 并重启，避免 Web 端继续 attach 到已不兼容的旧 session 而卡死。
- installer/update 路径补了针对 supervisor 版本不兼容清理流程的回归测试，确保“兼容时不丢 session，不兼容时明确清空旧 session”的行为稳定。

兼容性:
- 0.3.1 是 supervisor IPC 不兼容版本；0.3.0 的 live supervisor 不能被 0.3.1 daemon 安全复用。
- release installer 会要求 supervisor 兼容版本升级到 `0.3.1`。如果检测到已有 runtime session，会提示升级会丢失现有 session；用户确认后才会停旧 daemon、终止旧 supervisor 并清空 session 运行态。
- `scripts/update-local-termd.sh` 现在也会执行同样的兼容性判断；版本匹配时保留 session，版本不匹配时清空旧 session 后再完成本地更新。
- packet protocol version 仍保持现有 0.3.x 路径；daemon、Web UI、termctl 和 relay 仍建议同步升级到 0.3.1。
EOF
    ;;
  0.3.0)
    cat <<'EOF'
termd 0.3.0

用户可见变化:
- 重新打开或切换大型 session 时，终端输入通道会在 attach 后立即可用；10MB 级历史输出不再阻塞输入。
- supervisor 现在是终端画面的权威来源，会维护最近 1000 行热历史、当前 viewport、样式、光标位置和 session 级 terminal_seq；浏览器收到 snapshot 后替换画面，再按 tail 追平。
- Web xterm 输出改为 render-complete flow control：只有 xterm write callback 完成后才补 credit，慢浏览器不会被后端无限灌输出。
- terminal stream 新增明确的 snapshot/output/resize/exit frame，不再把恢复内容伪装成普通 session_data；重连和切换 session 时会避免历史重复重放。
- 重新 attach 的 snapshot 会恢复当前 SGR 样式状态，未 reset 的彩色输出在后续 tail 中会继续按原样式渲染。
- direct 和 relay 路径都覆盖了新 terminal stream；relay 下创建、attach、输入、daemon 重启后恢复热历史都使用同一套协议语义。

兼容性:
- 0.3.0 是 supervisor IPC 不兼容版本；旧 live supervisor 不能被 0.3.0 daemon 安全复用。
- 本次 release installer 会要求 supervisor 兼容版本升级到 `0.3.0`。如果检测到已有 runtime session，会提示“升级 supervisor 会丢失现有 session”；用户拒绝则退出升级流程，用户确认后才会停旧 daemon、终止旧 supervisor 并清空 session 运行态。
- daemon、Web UI、termctl 和 relay 建议同步升级到 0.3.0；relay 仍保持不可信 dumb pipe，不解密也不解析业务内容。
EOF
    ;;
  0.2.2)
    cat <<'EOF'
termd 0.2.2

用户可见变化:
- 修复 daemon 通过代理链路连接公网 relay 时反复 `websocket connect timed out` 的问题；daemon 到 relay 的 TLS 握手改用兼容性更好的传统 ECDHE key exchange，避开部分代理/TLS 入口吞掉过大的 hybrid ClientHello。
- Web 工作台打开 session 后，状态栏、RTT、session/client 后台刷新会复用当前 attach 的主 WebSocket，不再每秒/每两秒创建短连接，relay Web 和浏览器网络面板不再表现成持续重连。
- RTT 单次测量失败时会保留上一条有效延迟，不再因为一次 ping 抖动就让 session 标题右侧的延迟时有时无。
- 新建或 attach session 后会同步修复 daemon 展示元数据里的 session 状态，避免 live supervisor 已经 running 但 session 列表/本地更新校验仍看到 created。
- 本地更新脚本会在确认 live supervisor 与 runtime session 都仍 running 时，保守修复旧版本遗留的展示状态不一致；普通更新仍会校验 supervisor PID、session id 和 running session 数不下降。

兼容性:
- 这是 0.2.x 稳定性修复版本；协议 packet version 仍为 3，daemon、Web UI、termctl 和 relay 可按 0.2.x 路径同步升级。
- supervisor 兼容版本未更新，仍为 `0.1.0`；普通 termd 更新不应终止或清空已有 session supervisor。
- relay 仍保持不可信 dumb pipe，不解密也不解析 E2EE 内层业务 packet；本次 relay 稳定性修复主要在 daemon outbound 连接和 Web 客户端连接复用侧。
EOF
    ;;
  0.2.1)
    cat <<'EOF'
termd 0.2.1

用户可见变化:
- relay 和 direct WebSocket 的发送、pong、idle 超时放宽，并增加服务端主动 heartbeat；公网 relay 或代理链路短暂抖动时不再轻易被误判为连接超时。
- relay daemon mux 重新连接时会替换半断的旧 mux，并通知旧客户端走统一重连路径；daemon 长连接稳定运行一段时间后再次断开，会从快速重连退避重新开始。
- relay 在 daemon 不在线或 relay 状态不可用时会返回可重试错误，Web 客户端会识别这些错误并自动重连，减少手动刷新页面的情况。
- Web attach 静默重连时会先清空旧 xterm 再消费 daemon 重新发送的 screen snapshot，修复重连后终端已输出内容重复出现的问题。
- 网络 RTT 从底部状态栏移到 session 名称右侧、分辨率右侧；50ms 以内显示绿色，50-150ms 显示黄色，超过 150ms 显示红色，移动端同样显示在该位置。
- 底部状态栏移除 RTT 后重新收紧网络列宽，CPU、内存、磁盘等指标继续保持固定宽度布局。
- Web UI 字体切换为 HarmonyOS Sans SC，终端内容仍使用等宽字体，避免破坏终端列宽对齐。

兼容性:
- 这是 0.2.x 协议内的稳定性和 Web UI 修复版本；daemon、termctl、Web UI 和 relay 建议同步升级到 0.2.1。
- supervisor 兼容版本未更新，仍为 `0.1.0`；普通 termd 更新不应终止或清空已有 session supervisor。
- relay 仍保持不可信 dumb pipe，只转发 WebSocket 数据并管理连接，不解密也不解析 E2EE 内层业务 packet。
EOF
    ;;
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
- ${PLACEHOLDER_TEXT}
EOF
    ;;
esac
