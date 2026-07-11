# Termd Productivity Slices Implementation Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: keep this file as the source of truth. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施终端搜索、Git diff 查看、移动端输入增强和通知能力，并保留安全的文件编辑/删除能力。

**Architecture:** 先补协议类型和 daemon handlers，再接前端 DirectClient 与 React UI。终端搜索只读取 daemon 内存中的 screen snapshot，不把 PTY 明文写入 SQLite/state 文件。文件复制/移动和 Git commit/stash 暂不进入浏览器协议面，避免误操作入口超过当前 UI 的确认能力。

**Tech Stack:** Rust `termd_proto`/`termd`、React/Vite/TypeScript、xterm.js、lucide-react、Vitest。

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] 梳理现有终端、文件、Git、移动输入和通知边界
- [x] 新增协议类型与 daemon 处理：terminal search、git diff
- [x] 接入前端协议 client 与 TypeScript 类型
- [x] 实现终端搜索 UI 和结果跳转
- [x] 保留文件面板打开、编辑、下载、删除能力，移除复制/移动入口
- [x] 实现 Git diff 查看，移除 commit/stash 入口
- [x] 实现移动端快捷键配置和通知设置
- [x] 增加/更新测试并运行 Rust/前端验证
- [x] 使用安全脚本更新本地 termd
