# Termd Relay Web One-Shot Refactor Status Review

> 对照对象：
> [2026-06-03-termd-relay-web-one-shot-refactor.md](./2026-06-03-termd-relay-web-one-shot-refactor.md)
>
> 评估基线：
> 当前分支 `refactor/arch-boundary-split` 的 staged diff 与未改动热点文件现状
>
> 评估口径：
> 这里只回答三件事：
>
> - 原计划里哪些已经实际落地
> - 哪些完全没做
> - 哪些做了第一刀，但没有做到原计划承诺的深度

## 结论先说

这次分支没有完成“termd / relay / web 一步到位重构计划”的整体目标。

实际已经完成的是 15 个经过“实现 + 验证 + 双 review”记账的切片，外加 1 组前端
protocol helper 拆分：

- `termd` terminal frame log 拆分
- `termd` supervisor terminal journal/mirror 拆分
- `termd` startup / recovery 拆分
- `termrelay` route generation 生命周期补齐
- `termrelay` `pipe_pump` 拆分
- `termrelay` `RelayRegistry` 拆分
- `termrelay` `RouteBinder` 拆分
- `termrelay` `TransportPolicy` 拆分
- 前端 `App.tsx` / `useWorkspaceConnection` 的 workspace 连接生命周期拆分
- 前端 `direct-client` connect / handshake / bootstrap 拆分
- 前端 `connectPairingClient` 抽出到 protocol/pairing-client
- 前端 `useSessionFileLoaders` 的 file / git 只读 client 边界收窄
- 前端 `useSessionFilesPanelActions` 的文件面板导航 / 刷新 hook 拆分
- 前端 `useSessionGitDiffViewer` 的 git diff viewer hook 拆分
- 前端 `useSessionFileEditor` 的 file editor controller hook 拆分
- 前端 `direct-client` 的 HTTP E2EE / socket transport helper 拆分

所以准确口径应该是：

- 这次 **已经完成第一阶段边界抽离，并开始进入 relay 核心状态模块拆分**
- 但还没有完成原计划里那种 **全链路分层重构**
- 也没有完成原计划里的 **大范围文件拆分、状态机统一和全链路 E2E 验收矩阵**

## 当前执行进度

这一节记录继续按原计划推进后的可执行切片状态。只有实现、验证、两个 subagent
审查都通过后，才允许把条目标为完成。

- [x] `termd/src/net/protocol.rs` 抽出 `SessionTerminalFrameLog` 到
  `termd/src/net/protocol/terminal_frame_log.rs`
  - 验证：`cargo check -p termd`
  - 验证：`cargo test -p termd packet_terminal_ -- --nocapture`，28/28 通过
  - 规格复审：通过
  - 质量复审：通过
- [x] `termd/src/pty/supervisor.rs` 抽出 supervisor 终端 journal/mirror 到
  `termd/src/pty/supervisor/terminal_journal.rs`
  - 验证：`cargo check -p termd`
  - 验证：
    - `cargo test -p termd supervisor_ -- --nocapture`
    - `cargo test -p termd attach_sync_ -- --nocapture`
    - `cargo test -p termd daemon_terminal_mirror_ -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termd/src/net/protocol.rs` 抽出 startup/recovery 到
  `termd/src/net/protocol/recovery.rs`
  - 验证：`cargo check -p termd`
  - 验证：
    - `cargo test -p termd startup_ -- --nocapture`
    - `cargo test -p termd stale_restore_ -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termrelay/src/ws.rs` 补齐 route generation 生命周期与回归测试
  - 结果：
    - relay 边界强制 daemon control/data route 必须携带 `route_generation`
    - relay room 按 control 代际拒绝旧代 daemon data / idle daemon data 迟到接入
    - termd daemon control 派生的 data pipe / idle data pipe 统一继承同代 `route_generation`
  - 验证：`cargo fmt --all`
  - 验证：`cargo check -p termd -p termrelay`
  - 验证：
    - `cargo test -p termrelay daemon_routes_require_route_generation -- --nocapture`
    - `cargo test -p termrelay stale_idle_daemon_data_from_previous_route_generation_is_rejected -- --nocapture`
    - `cargo test -p termrelay stale_daemon_data_socket_from_previous_route_generation_is_rejected -- --nocapture`
    - `cargo test -p termrelay legacy_daemon_mux_route_is_rejected -- --nocapture`
    - `cargo test -p termrelay client_receives_retryable_error_when_daemon_control_is_offline -- --nocapture`
    - `cargo test -p termd relay_control_open_data_creates_raw_daemon_data_pipe -- --nocapture`
    - `cargo test -p termd relay_idle_data_pipe_accepts_assignment_and_sends_initial_hello -- --nocapture`
    - `cargo test -p termd relay_idle_data_pipe_closes_socket_after_client_disconnect -- --nocapture`
    - `cargo test -p termd relay_control_client_disconnect_aborts_pending_data_task -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termrelay/src/ws.rs` 抽出 writer/queue `pipe_pump` 到
  `termrelay/src/ws/pipe_pump.rs`
  - 结果：
    - `PipePump` 收口 writer 启动和 `FrameSender + control/data queue` 绑定关系
    - `PumpDataReceiver` 收口 data queue 出队后的 byte budget 回收
    - `http_tunnel` 与测试辅助不再手工持有或回收 data queue byte budget
  - 验证：`cargo fmt --all`
  - 验证：`cargo check -p termrelay`
  - 验证：
    - `cargo test -p termrelay relay_client_socket_receives_transport_idle_ping -- --nocapture`
    - `cargo test -p termrelay client_route_ready_does_not_wait_for_daemon_data_and_early_frames_are_piped -- --nocapture`
    - `cargo test -p termrelay idle_data_assignment_is_ordered_before_first_client_frame -- --nocapture`
    - `cargo test -p termrelay http_tunnel_request_body_waits_for_daemon_data_backpressure -- --nocapture`
    - `cargo test -p termrelay websocket_outbound_frame_pressure_distinguishes_slow_from_fast_large_frames -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termrelay/src/ws.rs` 抽出 `RelayRegistry` 到
  `termrelay/src/ws/registry.rs`
  - 结果：
    - room / register / unregister / pending pair / pre-pair flush / daemon data control /
      ping-pong enqueue 等 registry 生命周期集中收口到独立模块
    - `ws.rs` 主体回落到 websocket 握手、主循环和 `RelayState` facade，文件从超大状态机
      混合体收窄到 transport 主流程
    - `http_tunnel`、router tests 和 relay e2e 的 daemon route helper 全部补齐
      `route_generation`，不再依赖旧协议字段默认值
  - 验证：`cargo fmt --all`
  - 验证：`cargo check -p termd -p termrelay`
  - 验证：
    - `cargo test -p termrelay --bin termrelay -- --nocapture`
    - `cargo test -p termrelay --test relay_e2e -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termrelay/src/ws.rs` 抽出 `RouteBinder` 到
  `termrelay/src/ws/route_binder.rs`
  - 结果：
    - `bind_socket_route` 收口 `route_hello -> register -> route_ready` 建链前奏
    - `ws.rs` 的 `handle_socket` 现在主要保留 established websocket 的 writer 启动、
      pre-pair flush 和运行期读写主循环
    - 经复审后，`PipePump` 创建和 `EndpointCloseReceiver` 派生仍保留在 `ws.rs`，
      `RouteBinder` 只消费 `FrameSender`，不越界接管 writer/pump 职责
  - 验证：`cargo fmt --all`
  - 验证：`cargo check -p termd -p termrelay`
  - 验证：
    - `cargo test -p termrelay --bin termrelay -- --nocapture`
    - `cargo test -p termrelay --test relay_e2e -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termrelay/src/ws.rs` 抽出 `TransportPolicy` 到
  `termrelay/src/ws/policy.rs`
  - 结果：
    - websocket deadline / frame-size guard / idle ping / outbound pressure /
      receive-failure logging policy 统一收口到单独模块
    - `pipe_pump`、`route_prelude`、`router` 不再各自持有 transport 规则副本，
      改为消费 `policy.rs` 的单一来源
    - `ws.rs` 继续回落到连接编排和运行期主循环，transport rule 不再散落在主文件顶部
  - 验证：`cargo fmt --all`
  - 验证：`cargo check -p termd -p termrelay`
  - 验证：
    - `cargo test -p termrelay --bin termrelay -- --nocapture`
    - `cargo test -p termrelay --test relay_e2e -- --nocapture`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/App.tsx` 把连接生命周期逻辑收进 `useWorkspaceConnection`
  - 结果：
    - `useWorkspaceConnection` 不再只是 ref 容器，开始承接 workspace 建连、复用、
      关闭回收、session 权限补齐和独立 operation client 这组连接生命周期职责
    - `App.tsx` 不再内联 `closeWorkspaceClient`、`authenticatedClient`、
      `authenticatedWorkspaceClient`、`authenticatedSessionClient`、
      `resolveSessionScopedClient`、`openSessionOperationClient`
    - terminal attach / snapshot reset / receive loop 仍留在 `App.tsx` 和
      `useTerminalAttach`，没有把终端状态机误并入这次连接层切片
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/useWorkspaceConnection.test.tsx`
    - `npm test -- --run src/__tests__/app.test.tsx -t "可以创建 session 并自动 attach 到 terminal"`
    - `npm test -- --run src/__tests__/app.test.tsx -t "connection closed 后会静默按短延迟重试当前 session"`
    - `npm test -- --run src/__tests__/app.test.tsx -t "attach WebSocket 短断时保留终端并静默重连当前 session"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/protocol/direct-client.ts` 抽出 connect / handshake / bootstrap 到
  `termui/frontend/src/protocol/direct-handshake.ts`
  - 结果：
    - `DirectClient.connect()` 回落为薄入口，主要负责消费 handshake 结果、构造 client、
      恢复 transcript 和启动 receive pump
    - `performDirectHandshake()` 独立承接 route prelude、daemon 身份校验、E2EE bootstrap、
      客户端 `e2ee_key_exchange` 发送和失败清理
    - 新增 race 回归测试，覆盖 daemon 两帧到达后、客户端发送自己
      `e2ee_key_exchange` 前 socket 已关闭时必须失败而不是返回 dead client
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/direct-handshake.test.ts`
    - `npm test -- --run src/__tests__/direct-client.test.ts`
    - `npm test -- --run src/__tests__/app.test.tsx -t "可以创建 session 并自动 attach 到 terminal"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/App.tsx` 抽出 `connectPairingClient` 到
  `termui/frontend/src/protocol/pairing-client.ts`
  - 结果：
    - pairing 候选 URL 轮询、`server_id` 不匹配跳过、`pairing_payload_server_mismatch`
      错误归一化和空候选兜底都从页面组件移到了 protocol 层
    - `App.tsx` 的 pairing 入口只保留参数组装和 UI 状态切换，不再内联配对建连细节
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/app.test.tsx -t "配对候选 URL 会跳过 server_id 不匹配的 daemon|扫描 server_id 不匹配的邀请码时拒绝配对且不显示 token"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/hooks/useSessionFiles.ts` 把 `useSessionFileLoaders` 的
  `authenticatedSessionClient` 依赖收窄到
  `termui/frontend/src/protocol/session-sidecar-client.ts`
  - 结果：
    - 新增 `SessionFileLoadersClient`，只暴露 `listSessionFiles` /
      `getSessionGit` 两个只读能力
    - `useSessionFileLoaders` 不再在类型层直接依赖全量 `DirectClient`
    - 这刀只收窄 file / git 读路径边界，没有把 App.tsx 里的 upload / download /
      save / delete / git action sidecar handler 一起并进来
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/app.test.tsx -t "文件 panel 支持切换目录、上传、下载和删除"`
    - `npm test -- --run src/__tests__/app.test.tsx -t "文件 panel 可以切到 Git tab 查看未提交文件和提交图"`
    - `npm test -- --run src/__tests__/app.test.tsx -t "旧文件读取迟到后不会复活或覆盖当前编辑器"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/App.tsx` 把文件面板导航 / 刷新 handler 收进
  `termui/frontend/src/hooks/useSessionFiles.ts` 的 `useSessionFilesPanelActions`
  - 结果：
    - `handleOpenDirectory`、`handleGoToFilePath`、`handleRefreshSessionFiles`、
      `handleRefreshSessionGit`、`handleSessionFilesPanelTabChange` 从 `App.tsx`
      收口到独立 hook
    - hook 入参已经收窄成面板导航所需的最小字段集合，不再直接吃整个
      `SessionFilesController`
    - 这刀没有把 file editor、git diff、upload/download、attach/reconnect、
      terminal receive loop 混进来
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/app.test.tsx -t "文件 panel 支持切换目录、上传、下载和删除|文件 panel 可以切到 Git tab 查看未提交文件和提交图|文件 panel 默认每秒跟随终端 cwd，并可关闭跟随|文件 panel 在跟随模式下手动切目录后会退出跟随，避免被轮询打回|文件 panel 关闭跟随后忽略 daemon 后台 cwd 推送，仍可手动切目录"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/App.tsx` 把 git diff viewer 读路径收进
  `termui/frontend/src/hooks/useSessionFiles.ts` 的 `useSessionGitDiffViewer`
  - 结果：
    - `handleOpenGitDiff` / `handleCloseGitDiff`、diff request stale guard、
      close invalidation 和 session 切换失效逻辑从 `App.tsx` 收口到独立 hook
    - 新增 `termui/frontend/src/protocol/session-git-diff-client.ts`，只暴露
      git diff viewer 需要的最小 client 能力面：`getSessionGitDiff` + `close`
    - 这刀没有把 file editor、git mutation、upload/download、attach/reconnect 混进来
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/app.test.tsx -t "文件 panel 可以切到 Git tab 查看未提交文件和提交图|旧 Git diff 迟到后不会覆盖当前 diff 弹窗|旧文件读取迟到后不会复活或覆盖当前编辑器"`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/App.tsx` 把 file editor controller 收进
  `termui/frontend/src/hooks/useSessionFiles.ts` 的 `useSessionFileEditor`
  - 结果：
    - `handleOpenFile`、`handleSaveOpenFile`、`handleCloseFileEditor`、
      open/save request stale guard 和 session 切换失效逻辑从 `App.tsx`
      收口到独立 hook
    - 新增 `termui/frontend/src/protocol/session-file-editor-client.ts`，只暴露
      file editor 需要的最小能力面：`readSessionFile`、`writeSessionFile`、`close`
      以及 editor 读取 helper
    - 新增 `sessionFileWriteDelayMsByPath` 测试钩子，和“旧文件保存迟到后不会在切换
      session 后复活编辑器”的回归，真实覆盖 request 已收到后才切 session、
      ack 迟到返回的链路
    - 这刀没有把 git diff、git mutation、upload/download、attach/reconnect 混进来
  - 验证：`npm run typecheck`
  - 验证：`npm run build`
  - 验证：
    - `npm test -- --run src/__tests__/app.test.tsx -t "旧文件保存迟到后不会在切换 session 后复活编辑器|旧文件读取迟到后不会复活或覆盖当前编辑器|文件 panel 支持切换目录、上传、下载和删除|文件 panel 可以切到 Git tab 查看未提交文件和提交图"`
    - `npm test -- --run src/components/FileEditorDialog.test.tsx`
  - 规格复审：通过
  - 质量复审：通过
- [x] `termui/frontend/src/protocol/direct-client.ts` 继续拆 terminal/session/file/pairing protocol client
  - 结果：
    - `session-file-editor-client` / `session-git-diff-client` / `session-mutation-client` /
      `session-sidecar-client` 已把 file/git sidecar 边界从 `App.tsx` 和 `DirectClient`
      的直接耦合里收出来
    - `useSessionMutationActions` 让 delete / git action 的短 RPC 逻辑收口到文件 hook，
      不再散在页面组件里
    - `direct-client.ts` 继续保留 transport / terminal / session 的统一入口，但 helper
      面已经按职责拆开，不再把所有协议边界挤在一个文件里
  - 验证：
    - `npm run typecheck`
    - `npm test -- --run src/__tests__/app.test.tsx -t "文件 panel 支持切换目录、上传、下载和删除|文件 panel 可以切到 Git tab 查看未提交文件和提交图"`
    - `npm test -- --run src/__tests__/app.test.tsx -t "attach WebSocket 短断时保留终端并静默重连当前 session|attach WebSocket 短断恢复后还能继续渲染新的 terminal frame|connection closed 后会静默按短延迟重试当前 session|relay 恢复慢握手时重新 attach 使用长超时并静默恢复"`
  - 规格复审：通过
  - 质量复审：通过
- [x] 补齐原计划中的 relay/frontend 关键 E2E 回归覆盖
  - 结果：
    - 新增 attach 短断恢复后继续渲染 `terminal_frame` 的回归，覆盖“连接恢复了但终端不再刷新”的静默卡死路径
    - 断线恢复测试同时校验了 transient `Connection error` 不应闪现
  - 验证：
    - `npm run typecheck`
    - `npm test -- --run src/__tests__/app.test.tsx -t "attach WebSocket 短断时保留终端并静默重连当前 session|attach WebSocket 短断恢复后还能继续渲染新的 terminal frame|connection closed 后会静默按短延迟重试当前 session|relay 恢复慢握手时重新 attach 使用长超时并静默恢复"`
  - 规格复审：通过
  - 质量复审：通过

## 一、已经做了的

### 1. `termd` 启动恢复逻辑已经拆出来

这部分已经真实落地。

证据：

- [termd/src/net/server.rs](/usr/local/src/project/termd/termd/src/net/server.rs:6) 已经声明 `mod recovery;`
- [termd/src/net/server.rs](/usr/local/src/project/termd/termd/src/net/server.rs:55) 重新导出恢复函数
- [termd/src/net/server/recovery.rs](/usr/local/src/project/termd/termd/src/net/server/recovery.rs:31) 已经承载 `adopt_or_repair_runtime_sessions_from_supervisors`

这说明：

- 原计划里“startup / recovery 单独重构”的第一刀已经做了
- `server.rs` 不再独占这块恢复逻辑

### 2. `termrelay` 的 HTTP tunnel 和 route prelude 已经拆出来

这部分也已经真实落地。

证据：

- [termrelay/src/ws.rs](/usr/local/src/project/termd/termrelay/src/ws.rs:18) 已声明 `mod http_tunnel;`
- [termrelay/src/ws.rs](/usr/local/src/project/termd/termrelay/src/ws.rs:19) 已声明 `mod route_prelude;`
- [termrelay/src/ws.rs](/usr/local/src/project/termd/termrelay/src/ws.rs:22) 开始从 `http_tunnel` 导入
- [termrelay/src/ws.rs](/usr/local/src/project/termd/termrelay/src/ws.rs:26) 开始从 `route_prelude` 导入
- 新文件 [termrelay/src/ws/http_tunnel.rs](/usr/local/src/project/termd/termrelay/src/ws/http_tunnel.rs)
- 新文件 [termrelay/src/ws/route_prelude.rs](/usr/local/src/project/termd/termrelay/src/ws/route_prelude.rs)

这说明：

- 原计划里“relay 拆分 `ws.rs`”已经开始落地
- 但目前只拆了入口辅助部分，不是完整分层

### 3. 前端 `direct-client` 的 HTTP E2EE 与 socket transport helper 已经拆出来

这部分也已经真实落地。

证据：

- [termui/frontend/src/protocol/direct-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-client.ts:38) 已从 `./http-e2ee` 导入
- [termui/frontend/src/protocol/direct-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-client.ts:54) 已从 `./socket-transport` 导入
- 新文件 [termui/frontend/src/protocol/http-e2ee.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/http-e2ee.ts)
- 新文件 [termui/frontend/src/protocol/socket-transport.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/socket-transport.ts)

这说明：

- 原计划里“frontend 协议 helper 拆分”的第一刀已经做了
- `direct-client.ts` 不再把所有 helper 都塞在一个文件里

### 4. `App.tsx` 的 workspace 连接生命周期已经开始抽进 hook

这部分现在也已经真实落地，但只完成了连接层第一刀。

证据：

- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:229) 已经先创建
  `activeServer`，再把连接依赖注入 `useWorkspaceConnection`
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:123)
  现在已经承接 workspace 连接控制器，而不只是 refs
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:134)
  已收口 `closeWorkspaceClient`
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:162)
  已收口 `authenticatedClient`
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:232)
  已收口 `authenticatedWorkspaceClient`
- 新增测试 [termui/frontend/src/__tests__/useWorkspaceConnection.test.tsx](/usr/local/src/project/termd/termui/frontend/src/__tests__/useWorkspaceConnection.test.tsx:98)
  已覆盖并发复用、stale client 回收、关闭清理、权限幂等和 operation client 失败回收

这说明：

- 原计划里“App.tsx 拆出 connection hook”的第一刀已经落地
- 但它只收口了 workspace 连接生命周期，还没有继续拆 session / terminal /
  file / pairing 全套页面状态机

### 5. `direct-client` 的 connect / handshake / bootstrap 已经开始抽层

这部分现在也已经真实落地，但同样只完成了连接启动面的一刀。

证据：

- [termui/frontend/src/protocol/direct-handshake.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-handshake.ts:53)
  已独立承接 `performDirectHandshake`
- [termui/frontend/src/protocol/direct-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-client.ts:258)
  的 `DirectClient.connect()` 已改为消费 handshake 结果的薄入口
- [termui/frontend/src/__tests__/direct-handshake.test.ts](/usr/local/src/project/termd/termui/frontend/src/__tests__/direct-handshake.test.ts:44)
  已覆盖 dead-client race：daemon 两帧到达后、客户端发送自己的
  `e2ee_key_exchange` 前 socket 已关闭时，handshake 必须失败

这说明：

- 原计划里“frontend 协议客户端继续拆层”已经开始进入 connect/bootstrap 边界
- 但 terminal/session/file/pairing client 仍然还留在 `direct-client.ts`

### 6. `connectPairingClient` 已经从 `App.tsx` 抽到 protocol 层

这部分也已经真实落地，但它仍然只是 pairing protocol 面的一刀。

证据：

- [termui/frontend/src/protocol/pairing-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/pairing-client.ts:4)
  已独立承接 `connectPairingClient`
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:989)
  的 pairing 入口现在只消费 protocol 层 helper
- [termui/frontend/src/__tests__/app.test.tsx](/usr/local/src/project/termd/termui/frontend/src/__tests__/app.test.tsx:2779)
  仍覆盖“配对候选 URL 会跳过 `server_id` 不匹配 daemon”的关键语义

这说明：

- 原计划里“pairing protocol client 脱离页面组件”的第一刀已经落地
- 但 pairing 只是协议入口被抽出，页面上的 pairing 状态机并没有继续形成独立 hook

### 7. `useSessionFileLoaders` 已经开始从全量 `DirectClient` 收窄到最小 file / git 只读边界

这部分现在也已经真实落地，但它只完成了 file / git 读路径的类型边界第一刀。

证据：

- 新文件 [termui/frontend/src/protocol/session-sidecar-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/session-sidecar-client.ts:1)
  已定义 `SessionFileLoadersClient`
- [termui/frontend/src/protocol/session-sidecar-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/session-sidecar-client.ts:9)
  到 [termui/frontend/src/protocol/session-sidecar-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/session-sidecar-client.ts:11)
  只暴露 `listSessionFiles` / `getSessionGit`
- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:255)
  已把 `authenticatedSessionClient` 的类型从全量 `DirectClient` 收窄到
  `SessionFileLoadersClient`
- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:316)
  到 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:318)
  和 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:370)
  到 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:371)
  说明这个 hook 实际只消费这两个只读方法

这说明：

- 原计划里“frontend file/session protocol client 解耦”的第一刀已经落地
- 但它目前还只是 `useSessionFileLoaders` 的类型边界收窄
- `App.tsx` 里的 upload / download / save / delete / git action sidecar handler
  仍然直接面向更宽的 session 操作 client，没有继续拆成独立 file client / git client

### 8. `App.tsx` 的文件面板导航 / 刷新语义已经开始收口到独立 hook

这部分现在也已经真实落地，但它只完成了 file panel 导航 / 刷新这一小块页面状态边界。

证据：

- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:453)
  已新增 `useSessionFilesPanelActions`
- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:470)
  到 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:546)
  只承接 5 个文件面板动作：开目录、跳路径、刷新 files、刷新 git、切 tab
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1101)
  到 [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1125)
  已改为消费这个 hook，而不是在页面组件里内联 5 个 handler
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1)
  现在回落到 `4471` 行，比上个状态快照少了一截页面级样板逻辑

这说明：

- 原计划里“App.tsx 按 file/session/terminal 边界继续 hooks 化”的 file panel 第一刀已经落地
- 但它目前只收口了面板导航 / 刷新语义
- file editor、git diff viewer、upload/download transfer 和 file/git mutation 仍然留在 `App.tsx`

### 9. `App.tsx` 的 git diff viewer 读路径已经开始收口到独立 hook

这部分现在也已经真实落地，但它只完成了 git diff viewer 这一条只读弹窗路径。

证据：

- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:567)
  已新增 `useSessionGitDiffViewer`
- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:589)
  到 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:687)
  已独立承接 diff request stale guard、关闭失效、session 切换失效和
  `getSessionGitDiff` 读路径
- 新文件 [termui/frontend/src/protocol/session-git-diff-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/session-git-diff-client.ts:1)
  只暴露 git diff viewer 需要的最小能力面
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1115)
  到 [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1122)
  已改为消费 `useSessionGitDiffViewer`
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1)
  现在回落到 `4384` 行，继续把页面级协议状态机从大组件里往 hook 挪

这说明：

- 原计划里“App.tsx 按 feature/state machine 继续 hooks 化”的 git diff viewer 第一刀已经落地
- 它已经和 file editor 分开，不再强行合并成一个虚假的 read-dialog 抽象
- 但 file editor、upload/download transfer、file/git mutation 仍然留在 `App.tsx`

### 10. `App.tsx` 的 file editor controller 已经开始收口到独立 hook

这部分现在也已经真实落地，但它只完成了 file editor 这一条读/写弹窗路径。

证据：

- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:593)
  已新增 `useSessionFileEditor`
- [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:618)
  到 [termui/frontend/src/hooks/useSessionFiles.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useSessionFiles.ts:786)
  已独立承接 open/save request stale guard、close invalidation、session 切换失效、
  `readSessionFile` / `writeSessionFile` 路径，以及保存成功后的目录刷新
- 新文件 [termui/frontend/src/protocol/session-file-editor-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/session-file-editor-client.ts:1)
  只暴露 file editor 需要的最小能力面
- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1126)
  到 [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1137)
  已改为消费 `useSessionFileEditor`
- [termui/frontend/src/test/mock-daemon.ts](/usr/local/src/project/termd/termui/frontend/src/test/mock-daemon.ts:105)
  已新增 `sessionFileWriteDelayMsByPath`
- [termui/frontend/src/__tests__/app.test.tsx](/usr/local/src/project/termd/termui/frontend/src/__tests__/app.test.tsx:3606)
  到 [termui/frontend/src/__tests__/app.test.tsx](/usr/local/src/project/termd/termui/frontend/src/__tests__/app.test.tsx:3684)
  已新增“旧文件保存迟到后不会在切换 session 后复活编辑器”回归，且真实覆盖“request 已收到、session 已切换、ack 迟到返回”链路

这说明：

- 原计划里“App.tsx 按 feature/state machine 继续 hooks 化”的 file editor 第一刀已经落地
- file editor、git diff viewer、file panel loader 已经分开
- 但 upload/download transfer 和 file/git mutation 仍然留在 `App.tsx`

## 二、做了，但没有做到原计划那个力度

### 1. `termd / supervisor` 分层只做到了 recovery 拆分，没有做到四层架构真正拆开

原计划要求的是：

- `SessionSupervisor`
- `SupervisorClient / PtyBackend`
- `SessionRuntime`
- `DaemonProtocol`

边界都清清楚楚地收口。

但当前分支实际只动了启动恢复这一小块，没有继续拆 supervisor / runtime / protocol 主体。

证据：

- [termd/src/pty/supervisor.rs](/usr/local/src/project/termd/termd/src/pty/supervisor.rs:1) 仍是单文件，当前有 `3042` 行
- [termd/src/net/protocol.rs](/usr/local/src/project/termd/termd/src/net/protocol.rs:1) 仍是单文件，当前有 `20509` 行
- `git status --short` 里没有这两个文件的本轮改动

所以这部分不能叫“完成分层”，最多只能叫：

- 把 startup recovery 从 `server.rs` 里抽出来了

### 2. relay 已拆出 `http_tunnel` / `route_prelude` / `pipe_pump` / `registry` / `route_binder` / `policy`，但还没达到原计划的最终收口形态

原计划要求 relay `ws.rs` 进一步拆成职责明确的组件。

当前主要组件已经落地，但 `ws.rs` 仍然承担连接编排和一部分 shared type /
test helper，离原计划里那种更彻底的“薄 orchestrator”还有距离。

证据：

- [termrelay/src/ws.rs](/usr/local/src/project/termd/termrelay/src/ws.rs:1) 当前仍有 `2831` 行
- 当前已经新增了：
  - [termrelay/src/ws/http_tunnel.rs](/usr/local/src/project/termd/termrelay/src/ws/http_tunnel.rs)
  - [termrelay/src/ws/route_prelude.rs](/usr/local/src/project/termd/termrelay/src/ws/route_prelude.rs)
  - [termrelay/src/ws/pipe_pump.rs](/usr/local/src/project/termd/termrelay/src/ws/pipe_pump.rs)
  - [termrelay/src/ws/registry.rs](/usr/local/src/project/termd/termrelay/src/ws/registry.rs)
  - [termrelay/src/ws/route_binder.rs](/usr/local/src/project/termd/termrelay/src/ws/route_binder.rs)
  - [termrelay/src/ws/policy.rs](/usr/local/src/project/termd/termrelay/src/ws/policy.rs)

所以准确说法是：

- relay 核心 transport / registry 组件已经基本拆出
- 但 `ws.rs` 依然是核心大文件，离原计划目标还有明显距离

### 3. frontend 已拆 protocol helper、workspace 连接层、handshake/bootstrap 和 pairing protocol 第一刀，但还没有做到连接层 / 终端层 / 页面状态层的完整解耦

原计划要求拆出：

- `ConnectionSupervisor`
- transport adapters
- protocol clients
- terminal renderer 边界
- `App.tsx` hooks 化

当前只落地了其中一部分：

- `direct-client` 的 HTTP E2EE / socket transport helper 已抽出
- `App.tsx` 的 workspace 连接生命周期已开始抽入 `useWorkspaceConnection`
- `DirectClient.connect()` 的 handshake/bootstrap 已开始抽到独立模块
- `connectPairingClient` 已开始从页面组件挪到 protocol 层

但以下目标仍未真正落地：

- `direct-client.ts` 的 terminal / session / file / pairing protocol client 继续拆分
- `App.tsx` 的 session / terminal / file / pairing hooks 化
- 更完整的页面状态与协议边界解耦

证据：

- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1) 仍有 `4504` 行
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:123) 已经接管 workspace 连接层第一刀
- [termui/frontend/src/protocol/direct-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-client.ts:1) 仍有 `2045` 行
- [termui/frontend/src/protocol/direct-handshake.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-handshake.ts:53) 已经切出 connect/bootstrap 入口
- [termui/frontend/src/protocol/pairing-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/pairing-client.ts:4) 已经切出 pairing connect helper
- [termui/frontend/src/protocol/direct-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-client.ts:1964) 里 `SocketInbox` 仍然留在原文件

这说明：

- 前端已经跨过“只拆 helper”的阶段，开始把连接状态机从 `App.tsx` 往 hook 挪
- `direct-client` 也已经开始把 connect/bootstrap 从大文件里抽出来
- pairing 入口也已经开始从页面组件下沉到 protocol 层
- 但 `App.tsx` 的 attach 生命周期、terminal/UI 状态机，以及 `direct-client` 的 terminal /
  session / file / pairing 语义面，仍未完成原计划中的解耦

### 4. 文件拆分已经完成多刀，但仍不是原计划里的全量拆分矩阵

原计划列出了大范围拆分目标，包括：

- `termd/src/pty/supervisor.rs`
- `termd/src/net/protocol.rs`
- `termd/src/net/relay.rs`
- `termrelay/src/ws.rs`
- `App.tsx`
- `direct-client.ts`

当前分支至少已经产生了这些新增文件：

- [termd/src/net/server/recovery.rs](/usr/local/src/project/termd/termd/src/net/server/recovery.rs)
- [termd/src/net/protocol/terminal_frame_log.rs](/usr/local/src/project/termd/termd/src/net/protocol/terminal_frame_log.rs)
- [termd/src/pty/supervisor/terminal_journal.rs](/usr/local/src/project/termd/termd/src/pty/supervisor/terminal_journal.rs)
- [termrelay/src/ws/http_tunnel.rs](/usr/local/src/project/termd/termrelay/src/ws/http_tunnel.rs)
- [termrelay/src/ws/route_prelude.rs](/usr/local/src/project/termd/termrelay/src/ws/route_prelude.rs)
- [termrelay/src/ws/pipe_pump.rs](/usr/local/src/project/termd/termrelay/src/ws/pipe_pump.rs)
- [termrelay/src/ws/registry.rs](/usr/local/src/project/termd/termrelay/src/ws/registry.rs)
- [termrelay/src/ws/route_binder.rs](/usr/local/src/project/termd/termrelay/src/ws/route_binder.rs)
- [termrelay/src/ws/policy.rs](/usr/local/src/project/termd/termrelay/src/ws/policy.rs)
- [termui/frontend/src/protocol/direct-handshake.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/direct-handshake.ts)
- [termui/frontend/src/protocol/http-e2ee.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/http-e2ee.ts)
- [termui/frontend/src/protocol/pairing-client.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/pairing-client.ts)
- [termui/frontend/src/protocol/socket-transport.ts](/usr/local/src/project/termd/termui/frontend/src/protocol/socket-transport.ts)

所以准确说法只能是：

- 文件拆分已经形成一批真实落地切片
- 但远没有达到原计划列出的覆盖面

## 三、完全没做的

下面这些，在当前分支 diff 里没有体现为实际落地结果。

### 1. 没有完成 `protocol / E2EE / transport` 的统一边界重构

尤其没看到这些层面的实际代码落地：

- `route_ready` 生命周期重新收口
- `terminal_seq` / `ProtocolPacket.seq` / E2EE sequence 边界改造
- 更完整的 direct / relay transport 统一抽象

证据：

- [termd/src/net/protocol.rs](/usr/local/src/project/termd/termd/src/net/protocol.rs:1) 本轮未改
- [termd/src/net/relay.rs](/usr/local/src/project/termd/termd/src/net/relay.rs:1) 本轮未改，仍有 `8928` 行

### 2. 没有完成完整的 `App.tsx` 级别前端状态机重构

原计划要拆 connection/session/terminal/file/pairing hooks。

当前只完成了 connection hook 第一刀，没有完成整套状态机拆分。

证据：

- [termui/frontend/src/App.tsx](/usr/local/src/project/termd/termui/frontend/src/App.tsx:1) 仍有 `4504` 行
- [termui/frontend/src/hooks/useWorkspaceConnection.ts](/usr/local/src/project/termd/termui/frontend/src/hooks/useWorkspaceConnection.ts:123)
  已经只接住 workspace 连接生命周期，还没有继续覆盖 session / terminal / file / pairing hooks

### 3. 没有完成 `termd/src/net/relay.rs` 的拆分

原计划明确要拆：

- `relay/connector.rs`
- `relay/control.rs`
- `relay/data_pipe.rs`
- `relay/reconnect.rs`
- `relay/diagnostics.rs`

当前完全没动。

证据：

- [termd/src/net/relay.rs](/usr/local/src/project/termd/termd/src/net/relay.rs:1) 仍是单文件 `8928` 行

### 4. 没有完成 legacy 隔离和清理

原计划要求：

- `legacy pre-pair`
- `old route_ready`
- `terminal flow noop`

先隔离到 legacy 模块，再逐步删除。

当前没有看到对应模块化结果。

### 5. 没有完成原计划里的大验收矩阵

尤其下面这些没有在当前分支体现成对应的功能落地范围：

- relay web 无下行时的 route epoch / queue depth / close reason 诊断链路
- 满屏终端持续输出与底部光标稳定性的整套浏览器 E2E 收口
- 更系统的 supervisor / protocol / relay / frontend 全链路验收矩阵

这里不是说当时一点测试都没跑，而是说：

- 原计划承诺的那一整套“改造完成态验收面”并没有随本轮代码范围一起全部落地

## 四、当前最准确的项目口径

如果现在要对这次分支做一句话总结，最准确的说法是：

> `refactor/arch-boundary-split` 已经完成了一批边界拆分切片，但还没有完成 2026-06-03 那份“一步到位重构计划”的整体目标。

更具体一点：

- **做了的**：`termd` 多个边界切片、`termrelay` 的
  `route_generation / pipe_pump / registry / route_binder / transport policy` 拆分、`frontend protocol helper split`
- **没做到位的**：`supervisor/runtime/protocol` 真正分层、relay 的
  最终瘦身收口、frontend 连接状态机解耦
- **没做的**：`App.tsx` hooks 化、`net/relay.rs` 拆分、legacy 清理、原计划级别的全链路验收落地

## 五、后续建议

下一轮如果继续整理，建议不要再沿用“是否完成了大计划”这种模糊问法，而是拆成三个后续文档：

- 一份“已落地切片清单”
- 一份“剩余未做重构清单”
- 一份“下一轮执行计划”，只列能真正落地的小范围任务

这样后面追踪时，不会再把“原始大计划”和“实际实现范围”混在一起。
