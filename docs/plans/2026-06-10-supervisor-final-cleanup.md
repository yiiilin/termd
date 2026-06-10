# Supervisor Final Cleanup Plan

**Goal:** 清理 supervisor-only 重构后仍残留的 tmux / xterm / 旧终端语义口径，让当前代码库的生产路径、测试命名和对外技术文档重新对齐到 “supervisor 是唯一终端事实源，Ghostty 是唯一 Web terminal renderer，relay/termd 只做 workspace 和 opaque forwarding”。

**Scope boundary:**

- 允许删除已不再被生产路径使用的 tmux backend 代码与对应集成测试。
- 允许保留 `PtyRestoreInfo::Tmux` 这类“读取旧状态并降级关闭”的兼容元数据，只要它不再恢复或驱动生产运行。
- 允许重命名/改注释/改文档，前提是不改变当前已验证通过的 direct/relay/supervisor/ghostty 行为。
- 不改写旧计划文档里的历史事实；历史计划允许保留当时语境，当前技术文档和当前测试命名必须收敛。

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Audit Summary

- 生产默认 backend 已是 `SupervisorPtyBackend`，但仓库里仍暴露 `termd/src/pty/tmux.rs` 与 `termd/tests/tmux_backend.rs`。
- `TECH.md` 仍然把当前生产实现描述成 tmux-backed runtime。
- 前端和部分 Rust 注释/测试命名仍把“full-screen redraw / attach lifecycle / current renderer”写成 tmux 或 xterm 语义。
- 旧 tmux restore metadata 仍需要被读取并在恢复阶段显式判废，不能把这部分兼容误删。

## Tasks

- [x] Task 1: 删除已脱离生产路径的 tmux backend 暴露面和专属集成测试，只保留旧 tmux restore metadata 的判废兼容路径。
- [x] Task 2: 清理生产代码与当前前端测试里的过期 tmux/xterm 架构命名、注释和测试标题，统一到 supervisor/ghostty 口径。
- [x] Task 3: 重写 `TECH.md` 当前实现描述，确保生产路径、职责边界、验证入口与现状一致。
- [x] Task 4: 跑受影响验证，并完成两轮 subagent 复审后再打勾。

## Verification Targets

- `cargo test --workspace --locked --quiet`
- `cd termui/frontend && npm run typecheck`
- `cd termui/frontend && npm run test -- --run`
- 如前端或真实链路行为受影响：`cd termui/frontend && npm run test:e2e`
