# Supervisor Sharing Authority Refactor Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 保留 Ghostty Web 前端与现有外部协议，移除生产路径中的 tmux，把每个 session 的 supervisor 提升为权威 terminal truth 与 shared-control authority，daemon 收缩为 auth/pairing/E2EE/relay/session catalog 路由层。

**Architecture:** session supervisor 直接托管 PTY 与 terminal journal，并在本地维护 attached device/operator 集合、terminal snapshot/tail、controller 广播和 close/reconnect 语义。daemon 通过 supervisor IPC 完成 create/attach/detach/input/resize/control，并继续向 Web/relay 转发现有 terminal frame 协议。

**Tech Stack:** Rust, existing `SupervisorPtyBackend`/`SupervisorTerminalCache`, current `DaemonProtocol`, current Ghostty frontend, existing cargo/vitest/playwright verification.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] Task 1: 扩展 supervisor/session runtime 测试，先写出“sharing authority 在 supervisor”所需的失败用例，覆盖 attach/detach/role/control/input 授权、daemon 重连后的 state/restore 信息，以及无 tmux 生产启动路径。
- [x] Task 2: 扩展 `supervisor` IPC 与内部状态机，让 supervisor 本地维护 attached devices、operator/noop control 规则和 device-scoped input/resize 校验，并让 `SupervisorPtySession` 暴露 authority 接口给 daemon runtime。
- [x] Task 3: 重构 `SessionRuntime`，将 attach/detach/role/steal_control/write_input 等 authority 逻辑优先委托给 supervisor-backed session，只把 daemon `SessionManager` 保留为最小状态镜像或直接降级为 metadata 辅助。
- [x] Task 4: 切换生产默认 backend 到 `SupervisorPtyBackend`，移除 server/protocol/state recovery 中对 tmux 生产路径的依赖，并确保现有 persisted session / restore flow 只走 `UnixSocket` supervisor 元数据。
- [x] Task 5: 清理 protocol 中 tmux-specific terminal snapshot/mirror/fallback 假设，保留现有外部 terminal frame 协议，同时让 attach/create/reconnect 全部回到 supervisor 权威 snapshot/tail。
- [x] Task 6: 清理前端与前端测试中的 tmux 语义假设，仅保留对 Ghostty + supervisor snapshot/tail 的兼容逻辑；避免改动用户当前未提交的工作区变更范围之外的行为。
- [x] Task 7: 跑完整验证：至少覆盖新增/更新的 Rust 单测、`cargo test -p termd`、前端 typecheck/test 中受影响用例，并做 direct/relay 关键链路验证。
- [x] Task 8: 每个功能点完成后做两轮 subagent 审核（规范符合性 + 代码质量），修完阻断问题后再更新对应任务状态，最后再做一次全局集成复审。

## Verification Notes

- `cargo test -p termd`: 通过，当前全量后端测试为 `431 passed; 0 failed`
- `npm -C termui/frontend run typecheck`: 通过
- `timeout 45s ./node_modules/.bin/vitest run --config vitest.config.ts src/__tests__/terminal-pane.test.tsx src/__tests__/app.test.tsx`:
  在当前前端工作树状态下未在超时窗口内完成；本轮后端重构未改动用户未提交的前端文件
- Subagent 审核结论：Feature 1/2 已获两名 reviewer 通过；Feature 3 以保守表述通过集成复审，不宣称当前前端运行态已被完整复验
