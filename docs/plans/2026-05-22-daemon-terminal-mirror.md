# Daemon Terminal Mirror Implementation Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 daemon 为每个 supervisor session 维护可 attach 的终端镜像缓存，浏览器加入时优先从 daemon cache 发送 snapshot/tail，live raw bytes 先更新 mirror 再进入 room fanout。

**Architecture:** supervisor 仍是权威状态源；daemon mirror 是 read replica。supervisor IPC 重连时用权威 snapshot/tail 重置 daemon mirror；live output/resize/exit 进入 daemon 后，先更新 mirror 和 seq，再进入 session 级 live log。relay 继续只做 dumb pipe。

**Tech Stack:** Rust, existing `TerminalScreen`, `PtyTerminalFrame`, daemon `DaemonProtocol` terminal stream path, existing cargo/vitest/playwright verification.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] Task 1: 扩展 `TerminalScreen` 测试，覆盖普通屏/替代屏双缓存、当前屏模式、wrap/origin/insert/application/bracketed paste/mouse 等 VT mode 的 snapshot 恢复。
- [x] Task 2: 为 supervisor daemon IPC 客户端增加 mirror 测试，证明 attach/reconnect snapshot 会重置 mirror，live output/resize 在入队前更新 mirror，并能从本地 mirror 返回 snapshot/tail。
- [x] Task 3: 实现 `TerminalScreen` 的缺失状态字段、模式解析、snapshot 模式恢复，以及必要的测试可见 accessor。
- [x] Task 4: 实现 daemon-side `SupervisorTerminalMirror`，接入 `SupervisorPtySession`、IPC reader、snapshot seeding、frame queue pruning。
- [x] Task 5: 让 protocol attach 优先使用 daemon mirror snapshot/tail，避免每个 browser attach 都向 supervisor 请求 snapshot；同时保持普通 backend 兼容路径。
- [x] Task 6: 补协议层回归测试，覆盖多客户端 attach 共享 daemon cache、live log cursor、snapshot/tail 顺序。
- [x] Task 7: 修复审查发现的缓存一致性缺口：snapshot/空 tail 必须推进连接 cursor，protocol mirror 必须忽略旧帧，supervisor attach seed 必须避免 snapshot 回退 live frame，alt 切换不得丢 VT modes。
- [x] Task 8: 补一个真实 relay 回归测试，覆盖多个 session 的大输出、快速切换、回切后继续输入仍能秒级恢复。
- [x] Task 9: 运行完整验证：`cargo fmt --check`、聚焦测试、`cargo test -p termd`、`cargo test --workspace --locked`、前端 typecheck/test，以及 direct/relay 交互验证。
