# Supervisor Opaque Terminal Refactor Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: Use subagent-driven execution where available. Every functional task must pass two review stages before it can be marked complete: spec compliance review first, code quality review second.

**Goal:** 彻底重构终端链路。attach 成功后，`supervisor` 成为唯一终端事实与业务心跳所有者；`termd` 与 `relay` 只负责 pairing、auth、route、workspace/file API 和 opaque frame 转发。此次改造不保留旧终端协议兼容层，不保留 daemon-side terminal replay/heartbeat 语义，不保留 tmux 生产路径。

**Architecture:** `client` 保持一条 workspace 协议面向 `termd`，另有一条 terminal attach 语义面向 `supervisor`。物理链路仍可能经过 `relay -> termd`，但 attach 后终端消息统一编码为 opaque attach frame，由 `supervisor` 直接定义、发送、裁决和关闭。`termd` 只在 attach bootstrap 阶段理解 session/device 权限，之后不再解析终端业务字段。

**Tech Stack:** Rust workspace (`termd`, `termrelay`, `proto`), TypeScript/React (`termui/frontend`), Ghostty Web renderer, existing supervisor PTY backend.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Invariants

- `supervisor` 是 attach 后终端事实唯一所有者
- `supervisor` 负责 terminal heartbeat；`termd`/`relay` 的 ping/pong 只属于 transport 层
- `termd`/`relay` attach 后只搬运 opaque attach frame，不解析 snapshot/output/resize/cwd/cursor/selection/heartbeat
- attach 超时只关闭当前 attach，不关闭 session，不影响其他 attach
- file tree / file / git / session catalog 继续由 `termd` 负责
- Ghostty 是唯一 Web terminal renderer

## Tasks

- [x] Task 1: 新建 attach-scoped terminal protocol 文档与共享类型，定义 opaque attach frame、supervisor terminal message、heartbeat ping/pong、attach close reason，以及 packet stream 中的 attach frame payload 编解码。
- [x] Task 2: 扩展 `PtyAttachment` / `SessionRuntime` / `SupervisorPtyBackend`，让 watched attachment 从 no-op handle 升级为真实的 `supervisor` attach proxy，支持 output signal、opaque frame read/write 和 detach。
- [x] Task 3: 在 `supervisor` 中补齐 attach-scoped terminal message 集合与 heartbeat 任务，至少覆盖 attach sync、snapshot/output/resize/exit、heartbeat ping、heartbeat timeout close。
- [x] Task 4: 重写 `termd` packet terminal stream 路径，删除 daemon-side terminal replay/snapshot/tail 组装逻辑，改为对 watched attachment 做 opaque frame drain/forward，并将 client stream_chunk 输入直接转发给 attach proxy。
- [x] Task 5: 清理 `DaemonProtocol` 中与 packet terminal stream 绑定的旧 terminal state：`SessionTerminalFrameLog`、`pending_outputs`、`terminal_frame_next_seq`、`terminal_frame_snapshot_required`、`deferred_output_wakeups`、terminal sidecar timeout 依赖。
- [x] Task 6: 改造前端 `DirectClient` / terminal attach hook，引入 attach frame 队列与 `SupervisorTerminalClient`，由前端直接编解码 supervisor terminal message，并将 heartbeat pong 回给 attach stream。
- [x] Task 7: 改造 `App` / `TerminalPane` / 相关 hooks，切断旧 `session_data` / `terminal_frame` attach 路径，只保留 workspace RPC 与 supervisor terminal channel 双客户端模型。
- [x] Task 8: 清理 relay / termd 的终端业务耦合与日志假设，确保 relay 只统计 transport，termd 只统计 attach channel，不再记录 terminal frame/session_data 业务计数。
- [x] Task 9: 删除 tmux 生产代码、废弃兼容分支、死测试和无用 helper，并按新边界重组目录与命名。
- [x] Task 10: 补齐验证：Rust 单测、前端单测、direct/relay 集成链路、Ghostty 基本交互验证。
- [x] Task 11: 每个功能任务完成后做两轮 subagent 审核；修复阻断问题后再打勾，最后做一次全局集成复审。

## Verification Notes

- `cargo check -p termd`
  - 结果：通过。用于确认 `#[cfg(test)]` 收口后生产构建仍然可编译。
- `cargo test -p termd --lib --quiet`
  - 结果：通过，`412 passed; 0 failed; 20 ignored`。
- `cargo test -p termd-proto --lib binary_protocol_packet_attach_frame_carries_raw_bytes_without_base64 -- --nocapture`
  - 结果：通过，`1 passed; 0 failed`。
- `pnpm --dir termui/frontend exec vitest run`
  - 结果：通过，`22 files, 372 tests`。
- `npm run build`（`termui/frontend`）
  - 结果：通过，Vite 生产构建完成。
- `pnpm --dir termui/frontend exec playwright test tests/termui-web.smoke.spec.ts tests/termui-web.real-relay.spec.ts -g "pair、list、attach 的浏览器 smoke|浏览器通过真实 relay 连接 daemon 完成 pairing 和 session list|真实 relay 下 clear 之后上滚不会再看到 pre-clear 历史" --project=chromium`
  - 结果：通过，`3 passed`。
- Subagent 复审
  - 规格复审：`Huygens` 最终结论为“无阻断问题”。
  - 代码质量复审：`Copernicus` 最终结论为“无阻断问题”。

补充说明：
- `termd` 生产构建下已不再持有/消费 daemon-side terminal mirror；该类 legacy/search 辅助逻辑仅保留在 `#[cfg(test)]` 测试路径，用于覆盖旧单测和回归夹具。
- `session.attach` 已收敛为 workspace 权限附着；`terminal.attach` / `terminal.create` 才负责 watched attachment 与 opaque attach bootstrap。
- 后续收口清理见 [2026-06-10-supervisor-final-cleanup.md](2026-06-10-supervisor-final-cleanup.md)；其中会移除剩余 tmux backend 代码树与过期口径。
