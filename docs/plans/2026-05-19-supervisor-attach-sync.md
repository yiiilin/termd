# Supervisor AttachSync Implementation Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 supervisor/daemon 边界重构为事务化 `AttachSync(last_terminal_seq)` 模型，保证 snapshot/tail/live terminal frame 不丢不重。

**Architecture:** supervisor 继续作为 PTY、`TerminalScreen` 和 terminal event journal 的唯一权威源；daemon 只作为 supervisor consumer 和 Web/E2EE fanout 层。`AttachSync` 在同一个 supervisor state 锁视角下注册/替换 daemon controller、生成 snapshot 或 tail、确定 live 起点。

**Tech Stack:** Rust `termd` supervisor IPC、serde JSON Unix socket frame、`PtyTerminalFrame`、daemon packet terminal stream、React/TypeScript xterm.js render seq 检查。

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Invariants

- supervisor 只服务 daemon/backend controller，不直接管理 Web session。
- `TerminalEvent` journal 只存 raw terminal events，snapshot 不进入 journal。
- `Output`、`Resize`、`Exit` 共用同一个 session 级 `terminal_seq`。
- `AttachSync(last_terminal_seq)` 返回的 frames 覆盖到 `base_seq`；live stream 只能发送 `seq > base_seq`。
- 新 controller 替换旧 controller 后，旧 controller 不能继续 input/resize，也不能继续输出到 daemon。
- journal 过期必须降级返回自包含 snapshot。
- Web flow/ack 只影响 daemon 到 Web，不回流影响 supervisor event log。
- 不发版；实现完成后只允许 git commit。

## Tasks

### Task 1: Supervisor AttachSync API

**Files:**
- Modify: `termd/src/pty/supervisor.rs`
- Modify: `termd/src/pty/mod.rs`

- [x] 写红测：`AttachSync(None)` 在同一同步点返回 snapshot，且之后 live 只发 `seq > base_seq`。
- [x] 写红测：`AttachSync(Some(last_seq))` 在 journal 窗口内返回 tail，不返回 snapshot。
- [x] 写红测：`AttachSync(Some(old_seq))` 在 journal 过期时返回 snapshot。
- [x] 写红测：新 controller 替换旧 controller 后，旧连接 input/resize 被拒绝。
- [x] 实现 `SupervisorRequest::AttachSync { session_id, last_terminal_seq }` 和对应 response。
- [x] 让 `AttachSync` 在一个 state 锁内注册 controller、生成 snapshot/tail、设置 live 起点。
- [x] 运行 `cargo test -p termd pty::supervisor::tests -- --nocapture`。

### Task 2: Daemon Runtime Uses AttachSync

**Files:**
- Modify: `termd/src/pty/supervisor.rs`
- Modify: `termd/src/runtime/mod.rs`
- Modify: `termd/src/net/protocol.rs`

- [x] 写红测：daemon reconnect supervisor 后第一次 terminal snapshot/tail 不重复 pending live frame。
- [x] 写红测：packet terminal attach 携带 `last_terminal_seq` 时优先 tail，过旧时 snapshot。
- [x] 将 `drop_pending_terminal_frames_through(base_seq)` 降级为 `AttachSync` 响应后的 daemon 本地接收队列裁剪，不再作为同步事务来源。
- [x] 保留兼容 `snapshot()`/legacy `read()` 行为，不改变旧 `session_data` 路径。
- [x] 运行 `cargo test -p termd net::protocol::tests::packet_terminal_stream_open_and_output_use_stream_sequence -- --exact`。
- [x] 运行 `cargo test -p termd --test session_supervisor`。

### Task 3: Web Resync Alignment

**Files:**
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Modify: `termui/frontend/src/__tests__/direct-client.test.ts`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`

- [x] 确认 Web attach/resync 已把 `last_terminal_seq` 传给 daemon；缺口时触发重新 attach。
- [x] 若已有逻辑满足，只补测试证明；若不满足，补齐 DirectClient/App 参数传递。
- [x] 写/更新 Vitest：snapshot reset 后推进 base seq，连续 output 正常写入，不连续 output 触发 resync。
- [x] 运行 `npm run typecheck`。
- [x] 运行 `npm run test -- --run src/__tests__/direct-client.test.ts src/__tests__/app.test.tsx`。

### Task 4: End-to-End Regression Coverage

**Files:**
- Modify: `termd/tests/session_supervisor.rs`
- Modify: `termctl/tests/direct_daemon_e2e.rs` if needed
- Modify: `termui/frontend/tests/termui-web.real-relay.spec.ts` if needed

- [x] 写 Rust 集成测试：supervisor 保持存活、daemon reconnect 后通过 AttachSync 恢复 snapshot/tail。
- [x] 写 Rust 集成测试：输出发生在 sync 边界附近时不丢不重。
- [x] 评估 Rust/termctl E2E：termctl 无 xterm screen/seq 状态，保持每次 attach 获取 snapshot 的兼容模型，并运行既有创建/attach/resize E2E。
- [x] 运行 `cargo test -p termd --test session_supervisor`。
- [x] 运行 `cargo test -p termctl --test direct_daemon_e2e`。

### Task 5: Full Verification And Commit

**Files:**
- Update this plan file after each verified task.

- [x] 运行 `cargo fmt --all -- --check`。
- [x] 运行 `cargo test --workspace`。
- [x] 运行 `npm run typecheck`、`npm run test -- --run`、`npm run build` in `termui/frontend`。
- [x] 运行 `npm run test:e2e` in `termui/frontend` if Playwright/browser deps are available.
- [x] 记录任何已知非本次引入的验证问题。
- [x] `git diff` 自审，确认不发版、不改 release/tag。
- [x] 提交代码。
