# Termd Relay Web One-Shot Refactor Plan

> Source session log:
> `/root/.codex/sessions/2026/06/01/rollout-2026-06-01T07-07-14-019e8202-142e-7a81-bb1a-a09594e24698.jsonl:18900`
>
> Original assistant timestamp:
> `2026-06-03T07:17:41.854Z`
>
> Notes:
> This file is a direct landing of the plan text produced in that session.
> It preserves the original intent and scope of the proposal at that time.

## 整理说明

这份文档不是后来实际落地改造的 diff 摘要，而是当时会话中产出的“一步到位架构改造计划”整理版。

为了方便后续检索和对照，这里只做了两类整理：

- 保留原计划的目标、边界、不变量和拆分方向。
- 按仓库现有 `docs/plans` 风格把内容折叠成更稳定的章节结构。

这里没有额外补写“已完成 / 未完成 / 实际裁剪范围”；那部分应该放在单独的对照文档里，避免把原计划和事后状态混写。

## 文档定位

- 这是一次“目标架构与实施边界”文档，不是逐任务打勾的执行清单。
- 它描述的是当时希望一步收口的整体方向，不等价于后来实际实施的最小切片。
- 如果后续要继续推进大范围重构，应以此文档表达的边界原则为上位约束，再单独拆可执行计划。

## Summary

目标是把当前链路重构成边界清楚、可恢复、低耦合的架构，同时拆掉超大文件里的混杂职责，避免 relay web 断连、终端卡住、session 状态误判这类问题继续靠补丁堆叠。

两个 subagent 审核都指出原草案不能直接执行：必须修正 `supervisor controller` 语义、daemon 本地缓存职责、startup 真相源，以及 `route_ready` / E2EE sequence / `ProtocolPacket.seq` / `terminal_seq` 的边界。以下计划已合并这些修正。

核心不变量：

- relay 永远是 dumb pipe，不解密、不理解 session、不决定 PTY 生命周期。
- transport close 不等于 session close。
- 真实 PTY、权威 terminal journal、snapshot/tail 真相源在 session supervisor。
- daemon state 只是恢复索引；live supervisor / reconnect 结果优先于 persisted state。
- `terminal_seq`、`ProtocolPacket.seq`、E2EE sequence、`route_ready` 生命周期绝不混用。

## 计划主体

下面各节是原计划的整理版正文。

## Key Changes

### 1. termd / supervisor 分层

建立四层边界：

- `SessionSupervisor`
  - 每个 session 一个 supervisor 进程。
  - 持有真实 PTY、`SupervisorTerminalCache`、terminal journal、`next_terminal_seq`、snapshot/tail、attach_sync。
  - 支持 multi-controller shared-control：新 attach 只新增 controller，不替换旧 controller。
  - 所有 controller 都能收到 live terminal frame；关闭一个 controller 不能影响其他 controller 或 PTY。

- `SupervisorClient / PtyBackend`
  - daemon 侧 IPC client，只负责 launch、reconnect、request/response、output signal、read/write/resize/snapshot。
  - `SupervisorTerminalMirror` 只能作为 daemon 侧 read replica，用于减少回源；一旦发现 seq gap 或跨 resize 不连续，必须回源 supervisor。
  - 不做 auth、E2EE、relay、pairing、session policy。

- `SessionRuntime`
  - daemon 内 glue：连接 `SessionManager` attach 状态与 `PtyBackend` 句柄。
  - 只处理本地 session 生命周期、attach/detach、write/read/resize/close。
  - 不解析 WebSocket、HTTP、relay frame、E2EE frame、业务 packet。
  - detach 只移除连接状态，不能 terminate PTY。

- `DaemonProtocol`
  - 业务协议层：pairing、auth、E2EE、`ProtocolPacket` dispatch、terminal stream、file API、HTTP E2EE request。
  - 持有 `SessionTerminalFrameLog` 作为 protocol fanout cache；该 log 不是权威源。
  - terminal resume 优先用本地连续 cache；不连续时必须回源 supervisor snapshot/tail。

### 2. startup / recovery 单独重构

把 daemon 启动恢复从 relay 生命周期里完全拆出来：

- 启动时先扫描 live supervisor。
- 对 live supervisor 对应 session：
  - persisted state 缺失时补 Running record。
  - persisted state 误标 Closed 或缺 `restore_info` 时修回 Running。
  - size、socket path、pid 以 live supervisor 为准。
- persisted Running 但 reconnect 失败：
  - 标记为 Closed / stale，不能继续展示为 running。
- orphan supervisor：
  - 启动阶段只 warn，不主动 kill。
  - 后续可提供显式 prune 命令，但不能自动清理用户可能仍在跑的 shell。
- `PtyRestoreInfo` 继续只保存 socket path、pid、supervisor status；不保存 terminal 明文、输入历史或密钥。

### 3. protocol / E2EE / transport 统一边界

不新增 `SecurePacket`。

- E2EE 外壳：
  - WS 和 HTTP 都继续使用 `EncryptedFrame` / binary encrypted frame。
  - 每个 E2EE session 内 sequence 只服务 AEAD nonce/replay，每方向从 0 递增。
  - HTTP E2EE request 继续每 request 独立 context，不能多个并发 request 共享同一个 sequence context。

- 业务包：
  - 继续使用 `proto::ProtocolPacket` / `BinaryProtocolPacket`。
  - `ProtocolPacket.seq/ack/credit` 只属于 packet stream，按 `stream_id` 工作。
  - 不跨 WebSocket、不跨 reconnect、不代表 terminal 历史位置。

- terminal 恢复：
  - 只使用 session 级 `terminal_seq`。
  - `last_terminal_seq` 只表达 terminal renderer 已渲染位置。
  - snapshot/tail、缺口检测、resize rebase 都围绕 `terminal_seq`，不能借用 packet seq 或 E2EE sequence。

- route lifecycle：
  - `route_ready` 只表示 transport route accepted。
  - 它不表示 daemon data pipe ready、E2EE ready、auth ready、terminal attach ready。
  - relay path 可增加 `epoch`、`close_reason`、accepted/ready 诊断字段；direct path 第一阶段保持兼容。

### 4. relay 重构

把 `termrelay/src/ws.rs` 拆成清晰组件：

- `Registry`
  - 管理 `server_id`、daemon control、pending client、daemon data pipe、client data pipe。
  - 不保存明文业务状态。

- `RouteBinder`
  - 处理 route hello、role、client/data pairing、pending deadline。
  - client `route_ready` 可以先于 daemon data pair 返回。
  - 早到 opaque frame 只允许 bounded FIFO 暂存，deadline 到必须关闭 pending client。

- `PipePump`
  - 只搬 opaque WS frame / HTTP tunnel body chunk。
  - 慢连接、writer failure、队列满只关闭当前 pipe。
  - 不能关闭 daemon control，不能修改 session 状态。

- `TransportPolicy`
  - 管理 timeout、queue cap、close reason、日志字段。
  - 输出可追踪链路日志：route id、role、generation/epoch、queue depth、close reason、delivered/dropped count。

HTTP tunnel 保持文件 API compatibility tunnel：

- router 仍只暴露现有文件 API 白名单。
- 默认关闭，开启需显式 `--http-tunnel`。
- 不扩成通用 HTTP proxy。
- relay 不解密 HTTP E2EE body。

### 5. frontend 重构

把 `App.tsx` 和 `direct-client.ts` 的职责拆开：

- `ConnectionSupervisor`
  - 管理 direct/relay 连接生命周期、abort、reconnect、backoff、current epoch。
  - route_ready 卡住或切 session 时必须 abort 旧连接。
  - 半开连接 close/error 后触发重连，但不能清空已存在 session UI 状态。

- transport adapters
  - `DirectWsTransport`
  - `RelayWsTransport`
  - `HttpE2eeFileTransport`
  - 只负责 open/send/receive/close，不理解 terminal rendering。

- protocol clients
  - `TerminalStreamClient`
  - `SessionClient`
  - `PairingClient`
  - `FileClient`
  - 都基于 `ProtocolPacket`，不直接操作 WebSocket。

- terminal renderer
  - 继续以 `terminal_seq` 做连续性确认。
  - snapshot 后重置 base seq。
  - output gap 触发 resync，不能推进到跳号 frame。
  - resize 跨越时使用 snapshot rebase，避免 mixed tail 导致光标在屏幕中间。

第一阶段不强拆两条 WebSocket；terminal 和 sidecar 先做逻辑解耦，仍可共用同一 E2EE packet connection。

### 6. 文件拆分

按职责拆，不做无意义搬家：

- `termd/src/pty/supervisor.rs`
  - `supervisor/backend.rs`
  - `supervisor/client.rs`
  - `supervisor/ipc.rs`
  - `supervisor/process.rs`
  - `supervisor/server.rs`
  - `supervisor/terminal_journal.rs`
  - `supervisor/recovery.rs`

- `termd/src/net/protocol.rs`
  - `protocol/e2ee.rs`
  - `protocol/packet_dispatch.rs`
  - `protocol/terminal_stream.rs`
  - `protocol/file_api.rs`
  - `protocol/session_api.rs`
  - `protocol/recovery.rs`

- `termd/src/net/relay.rs`
  - `relay/connector.rs`
  - `relay/control.rs`
  - `relay/data_pipe.rs`
  - `relay/reconnect.rs`
  - `relay/diagnostics.rs`

- `termrelay/src/ws.rs`
  - `ws/registry.rs`
  - `ws/route_binder.rs`
  - `ws/pipe_pump.rs`
  - `ws/http_tunnel.rs`
  - `ws/policy.rs`

- frontend
  - `protocol/direct-client.ts` 拆成 transport、packet client、terminal stream、file client。
  - `App.tsx` 拆出 connection/session/terminal/file/pairing hooks。
  - UI 组件只消费状态和 actions，不直接持有协议细节。

拆分策略：

- 先加 characterization tests。
- 再移动代码，保持行为不变。
- 最后删除 legacy。
- 每个阶段保持可编译、可运行、可回滚。

## Cleanup Rules

- `control_request/control_grant` 继续作为 shared-control 兼容 noop，不引入 takeover。
- legacy pre-pair / old route_ready / terminal flow noop 先隔离到 legacy 模块。
- 只有测试覆盖 direct、relay、HTTP E2EE、旧客户端兼容后才删除旧路径。
- 不把 relay 日志、queue、route epoch 泄露进 daemon session 状态。
- 不把 terminal ACK/credit 恢复成强流控；terminal 下行背压由 bounded queue、慢连接关闭、reconnect + snapshot/tail 解决。

## Test Plan

必须新增或保留以下验收测试：

- supervisor
  - daemon restart adopts live supervisor。
  - persisted Closed 被 live supervisor 修回 Running。
  - orphan supervisor 启动时只 warn，不 kill。
  - multi-controller attach：关闭一个 controller，另一个仍可 input/output。
  - attach_sync snapshot/tail 使用 `terminal_seq`，不重放 `<= base_seq` 的 live frame。
  - output backlog 下 input/ping/attach response 不被整体饿死。

- daemon / protocol
  - transport close 只 detach，不 close session。
  - reconnect 失败的 persisted Running 标 stale/Closed。
  - daemon-side terminal seq gap 后新 attach 必须回源 supervisor。
  - resize 前后的 `last_terminal_seq` 触发 snapshot rebase。
  - `ProtocolPacket.seq` 与 `terminal_seq` 分别断言，防止混用。

- relay
  - client route_ready 先返回、daemon data 后配对、早到 frame FIFO flush。
  - pending client 超 deadline 被关闭并带 close reason。
  - slow client / writer failure 只关闭当前 pipe，不关闭 daemon control。
  - control/data/client 任一断连不污染下一代 route epoch。
  - HTTP tunnel 只允许文件 API 白名单，默认 disabled。

- frontend
  - route_ready 卡住时切 session，旧 connect abort，新 session 正常 attach。
  - WebSocket close/error 发生在 receive backlog 时，先 drain 可用 frame，再进入 reconnect。
  - terminal output gap 触发 resync，不推进坏 seq。
  - 大 snapshot + 持续输出 + relay reconnect 后，xterm 不卸载、不半屏悬停。
  - 文件 upload/download abort 后只清理短连接 watcher，不影响 session。

- E2E / browser
  - 真实 relay 下新建 session、持续输出、长时间空闲、键盘输入、刷新页面、断网重连。
  - 满屏终端连续回车，光标必须保持在底部。
  - relay web 无下行时记录 route epoch、queue depth、pipe close reason，能定位卡在哪一层。

## Assumptions

- 保持单用户、设备级信任、shared-control。
- relay 继续可被 nginx 代理，但 relay 本身不变成 nginx/http proxy。
- session 不因 client 断开终止。
- 第一阶段目标是稳定架构和清晰边界，不引入多人权限、账号系统或平台策略。
- 当前仍在 Plan Mode；本计划只定义实施方案，尚未修改代码。

## 使用建议

如果后续继续围绕这份计划推进，建议额外补两份伴随文档：

- “原计划 vs 实际已落地范围” 对照文档。
- “下一轮可执行切片计划” 文档，把剩余大范围改造拆成可验证的小任务。
