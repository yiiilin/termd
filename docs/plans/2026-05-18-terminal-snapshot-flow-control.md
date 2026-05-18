# Terminal Snapshot Flow Control Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 session 切换不再被大量历史 replay 阻塞，并以 supervisor 为权威恢复最近 1000 行热历史、当前屏幕和后续 tail。

**Architecture:** supervisor 维护 session 级 `terminal_seq`、raw journal 和 `TerminalScreen`；daemon 只负责权限、E2EE、WebSocket 路由和按 packet credit 转发 terminal frame；浏览器在 xterm write callback 后补 credit。现有旧 `session_data` 路径保留为兼容层，packet terminal stream 使用新的 snapshot/output/resize frame。

**Tech Stack:** Rust `termd`/`proto`、Unix socket supervisor IPC、serde JSON packet、React/TypeScript、xterm.js、Vitest、Cargo tests。

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Constraints

- 不在当前 `termd.service` 上测试；端到端验证必须启动独立 state dir、独立端口的新 `termd`。
- 不主动 kill 当前 supervisor 或当前 session。
- `ProtocolPacket.seq` 仍是连接内 stream seq；新增 `terminal_seq` 是 session 级终端事件 seq，二者不能混用。
- 注释使用中文，关键状态机和流控边界必须写清楚。

## Tasks

### Task 1: Proto Terminal Frame Types

**Files:**
- Modify: `proto/src/lib.rs`
- Modify: `termui/frontend/src/protocol/types.ts`

- [x] 增加 `TerminalFrameKind`、`TerminalFramePayload`、`TerminalSnapshotFramePayload`、`TerminalOutputFramePayload`、`TerminalResizeFramePayload`。
- [x] `terminal.attach` payload 支持可选 `last_terminal_seq`。
- [x] Rust/TypeScript 协议类型测试覆盖 snapshot/output/resize JSON 形状。
- [x] 运行 `cargo test -p termd-proto --locked` 和 `npm run test -- --run src/__tests__/protocol-types.test.ts`。

### Task 2: Supervisor Terminal Cache

**Files:**
- Modify: `termd/src/pty/mod.rs`
- Modify: `termd/src/pty/supervisor.rs`
- Modify: `termd/src/net/screen.rs`
- Modify: `termd/tests/session_supervisor.rs`

- [x] 扩展 PTY trait，提供 terminal snapshot/tail frame 能力；非 supervisor backend 可降级为现有 snapshot/read。
- [x] 在 supervisor 内新增 `SupervisorTerminalCache` 和 `TerminalEvent`，维护 `next_terminal_seq`、journal、`TerminalScreen`、size。
- [x] PTY output/resize/exit 统一分配 session 级 `terminal_seq`，并写入 journal。
- [x] snapshot 返回 `base_seq=current_seq`、1000 行热历史 + 当前 viewport、size、process id。
- [x] tail 按 `last_terminal_seq` 返回 journal 窗口内事件；过旧时要求 snapshot resync。
- [x] 测试覆盖：输出后 snapshot、resize 进入 journal、journal 窗口外返回 snapshot、daemon 重连后 supervisor snapshot 仍可用。
- [x] 运行 `cargo test -p termd --test session_supervisor --locked`。

### Task 3: Daemon Packet Terminal Stream

**Files:**
- Modify: `termd/src/runtime/mod.rs`
- Modify: `termd/src/net/protocol.rs`
- Modify: `termd/src/net/server.rs`

- [x] `attach_session` 移除 attach 前同步大 drain；attach 响应立即返回。
- [x] packet terminal stream open 后按 credit 推送 terminal snapshot/tail frame，而不是伪装成 `session_data`。
- [x] output watcher 不再依赖 daemon 作为唯一终端缓存权威；优先读取 supervisor terminal frame。
- [x] resize 推送也进入 terminal frame，保留旧 `session_resized` 元数据推送。
- [x] flow credit 消耗以 terminal frame 为单位，`ProtocolPacket.seq` 只表示连接内 frame 序号。
- [x] 测试覆盖：attach response 先于 snapshot、大量输出不会阻塞输入、credit 为 0 不推送、flow 后继续推送、terminal_seq 连续。
- [x] 运行 `cargo test -p termd --locked`。

### Task 4: Frontend Render-Complete Flow Control

**Files:**
- Modify: `termui/frontend/src/protocol/direct-client.ts`
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/__tests__/direct-client.test.ts`
- Modify: `termui/frontend/src/__tests__/terminal-pane.test.tsx`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`

- [x] `DirectClient` 不再收到 stream chunk 就 ack；改为把 transport seq 暴露给渲染层。
- [x] `DirectClient` 提供 render ack API，支持每 8 个 frame 批量补 credit。
- [x] `TerminalPane` 支持 snapshot reset 语义，并把输出按最大 64KB 写入 xterm。
- [x] `TerminalPane` 在每个 xterm write callback 后通知 frame rendered。
- [x] 前端校验 `terminal_seq` 连续；发现缺口时触发 resync attach。
- [x] 测试覆盖：snapshot reset 不重复、慢 write 不提前 ack、切片写入、terminal_seq 缺口触发重同步。
- [x] 运行 `npm run typecheck` 和相关 Vitest。

### Task 5: Independent End-to-End Verification

**Files:**
- Modify or create only if needed: `scripts/qa.sh` or test helper script

- [x] 构建 release 二进制和前端 bundle。
- [x] 启动独立 `termd`：使用临时 `TERMD_STATE_DIR`/state DB 和非 8765 端口，不使用 `systemctl restart termd.service`。
- [x] 通过 WebSocket/termctl 或浏览器自动化创建 session，输出约 10MB 后重新 attach，确认输入立即可发送。
- [x] 验证 snapshot 可向上滚到最近 1000 行热历史。
- [x] 验证两个浏览器/连接 attach 后最终画面一致。
- [x] 验证前端慢渲染时后端 credit 不无限灌输出。
- [x] 验证独立 daemon 重启、supervisor 保持存活后仍能恢复最近 1000 行热历史和当前屏幕。
- [x] 确认当前系统 `termd.service` 的 supervisor/session 未被测试流程影响。
