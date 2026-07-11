# Terminal Poll Output Implementation Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不改变前端 terminal stream 逻辑的前提下，将 daemon 输出读取收敛成 `terminal.poll` 语义：daemon 从权威 terminal cache/tail 按 cursor 取 batch，cursor 丢失或 resize 跨越时返回 snapshot rebase。

**Architecture:** PTY/supervisor 仍持续输出，daemon 先更新 session 级 terminal mirror/cache/tail。direct 和 relay 进入 daemon 后统一走同一套 `ProtocolConnection`，server/relay 的输出 drain 只调用 daemon 内部 `terminal.poll` 取 tail/snapshot，再按现有 terminal stream frame 发给前端；relay 继续只转发 opaque bytes。

**Tech Stack:** Rust daemon/proto/relay，TypeScript React frontend，Vitest/Playwright/Cargo tests。

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Requirements Checklist

- [x] daemon 保存权威 terminal cache/tail，PTY 输出先更新 cache，再允许客户端读取。
- [x] daemon 内部新增 `terminal.poll` 语义：连续 cursor 返回 tail；首次 attach、cursor 丢失、tail 缺口或 resize epoch 不一致返回 snapshot；无新数据返回 empty。
- [x] resize 是 snapshot rebase 边界，poll 客户端不依赖 replay resize frame 恢复跨分辨率状态。
- [x] direct/relay 不再为 terminal output 使用 per-client output debt、credit 或 ACK；慢客户端不阻塞 daemon、PTY、relay 或其他客户端。
- [x] terminal input 仍走当前 session 的 terminal stream/connection，和 resize/input 保持有序。
- [x] relay 保持 dumb pipe，不新增业务解析。
- [x] 前端 attach/receive/xterm 写入逻辑保持不变，仍消费现有 terminal stream frame。
- [x] 二进制模式下 terminal stream 继续使用 typed terminal frame bytes，不退回大 JSON/base64。
- [x] 测试覆盖 direct、relay 相关协议路径、快速切换、大输出、cursor 丢失 snapshot、resize snapshot rebase。

### Task 1: Daemon Poll Semantics

**Files:**
- Modify: `termd/src/net/protocol.rs`
- Test: existing unit tests in `termd/src/net/protocol.rs`

- [x] **Step 1: Write failing daemon tests for poll tail, poll empty, cursor loss snapshot, and resize-crossing snapshot**
- [x] **Step 2: Implement internal `terminal.poll` collection from daemon session terminal log**
- [x] **Step 3: Verify targeted daemon protocol tests pass**

### Task 2: Keep Frontend Wire Stable

**Files:**
- Modify: `termd/src/net/protocol.rs`
- Test: existing unit tests in `termd/src/net/protocol.rs`

- [x] **Step 1: Add/adjust tests proving `terminal.attach` still returns existing stream frames to the frontend**
- [x] **Step 2: Route existing stream push collection through internal poll function without changing frontend packet shape**
- [x] **Step 3: Verify binary stream frames remain typed terminal frame payloads**

### Task 3: Reduce Terminal Push Queue Debt

**Files:**
- Modify: `termd/src/net/server.rs`
- Modify: `termd/src/net/relay.rs`
- Modify: `termd/src/net/protocol.rs`
- Test: existing server/relay protocol tests

- [x] **Step 1: Keep existing frontend stream output, but ensure each drain batch is produced by daemon poll instead of open-coded queue drain**
- [x] **Step 2: Remove or neutralize stale per-client terminal output debt where poll cursor/snapshot can replace it**
- [x] **Step 3: Verify relay still only routes opaque frames and direct/relay tests pass**

### Task 4: End-To-End Verification And Design Review

**Files:**
- Review all touched files

- [x] **Step 1: Run Rust tests for proto, daemon protocol/server/relay, and relay e2e**
- [x] **Step 2: Run frontend typecheck/build/unit tests**
- [x] **Step 3: Run browser smoke/relay tests that are available locally**
- [x] **Step 4: Re-read Requirements Checklist and mark every item pass/fail with evidence**

## Verification Evidence

- `cargo test -p termd`
- `cargo test -p termd-proto`
- `cargo test -p termrelay`
- `cargo test -p termctl`
- `cargo test -p termweb`
- `cargo test --workspace --no-fail-fast`
- `cargo fmt --check`
- `npm test -- --run`
- `npm run build`
- `npm run test:e2e -- --project=chromium tests/termui-web.smoke.spec.ts tests/termui-web.real-relay.spec.ts`
- `npm run test:e2e -- --project=mobile-chrome tests/termui-web.smoke.spec.ts tests/termui-web.real-relay.spec.ts`
