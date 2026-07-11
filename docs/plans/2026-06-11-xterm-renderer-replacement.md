# Xterm Renderer Replacement Plan

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

> **For agentic workers:** REQUIRED: Use subagent-driven execution where available. Every functional task must pass two review stages before it can be marked complete: spec compliance review first, code quality review second.

**Goal:** 彻底将 `termui/frontend` 的 Web 终端 renderer 从 `ghostty-web` 重构为 `xterm.js`，删除生产路径中的 Ghostty 专有依赖、补丁与测试假设，并把终端交互恢复到稳定、可维护的 xterm.js 语义。

**Architecture:** 保留现有 `TerminalRendererInstance` 抽象和 `TerminalPane` / `App` / attach data-plane，不改 supervisor / termd / relay 协议面；在前端内部把 `ghostty-renderer.ts` 替换成 `xterm-renderer.ts`，将当前依赖 Ghostty 私有对象的选区、scrollback、fit、theme、input-anchor、debug bridge 改写为基于 xterm.js 官方 API 的实现。所有 Ghostty 专有 CSS、测试夹具、注释、debug 全局符号和 package 依赖必须一并清理，不保留双 renderer fallback。

**Tech Stack:** TypeScript, React 19, Vite 7, `@xterm/xterm`, `@xterm/addon-fit`, `@xterm/addon-search`, Vitest, Playwright.

---

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Root-Cause Audit

- `ghostty-web` 不是简单的 renderer 依赖，而是把 selection、scrollback、fit、theme、refresh、debug 能力带进了 `ghostty-renderer.ts` 私有补丁层。
- `TerminalPane.tsx`、`App.tsx`、`styles.css`、Vitest mock、Playwright smoke/real-relay 都已经携带 Ghostty 假设，直接替换 npm 包会让焦点、粘贴、选区和主题同步一起失效。
- 当前 Ghostty 路径中最重的私有补丁包括：`selectionManager` 选区桥接、fractional viewport render 修正、canvas filler、font metrics 稳定化、theme resync by full snapshot。换成 xterm.js 后，这些补丁大部分应被删除，而不是机械平移。

## Invariants

- Web 客户端生产路径只保留一个 renderer：`xterm.js`
- 不保留 Ghostty fallback、兼容分支、运行期开关或双栈测试矩阵
- `TerminalRendererInstance` 对 `TerminalPane` 暴露的抽象保持稳定，避免把 renderer 私有 API 再次泄漏到 UI 层
- 终端输入、选区、滚动、搜索、theme 更新必须走 xterm.js 官方公开 API，不依赖私有字段
- `App` 侧 theme 切换不再通过 full snapshot 重建终端实例修补 renderer 缺陷
- 现有 attach / output / resize / scrollback / copy-paste 行为不回退

## Planned File Changes

- Create: `termui/frontend/src/components/terminal/xterm-renderer.ts`
- Create: `termui/frontend/src/__tests__/xterm-renderer.test.ts`
- Modify: `termui/frontend/package.json`
- Modify: `termui/frontend/package-lock.json`
- Modify: `termui/frontend/src/components/terminal/renderer.ts`
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Modify: `termui/frontend/src/App.tsx`
- Modify: `termui/frontend/src/styles.css`
- Modify: `termui/frontend/src/test/vitest.setup.ts`
- Modify: `termui/frontend/src/__tests__/terminal-renderer-factory.test.ts`
- Modify: `termui/frontend/src/__tests__/terminal-renderer-race.test.tsx`
- Modify: `termui/frontend/src/__tests__/terminal-pane.test.tsx`
- Modify: `termui/frontend/src/__tests__/app.test.tsx`
- Modify: `termui/frontend/src/__tests__/mobile-layout-regression.test.ts`
- Modify: `termui/frontend/tests/termui-web.smoke.spec.ts`
- Modify: `termui/frontend/tests/termui-web.real-relay.spec.ts`
- Delete: `termui/frontend/src/components/terminal/ghostty-renderer.ts`
- Delete: `termui/frontend/src/__tests__/ghostty-renderer.test.ts`

## Tasks

- [x] Task 1: 安装并接入 `xterm.js` 官方依赖，新增 `xterm-renderer.ts`，完整实现 `TerminalRendererInstance` / `TerminalRendererTerminal` / fit / search / scroll state / input anchor 适配层。
- [x] Task 2: 用 xterm.js 官方 buffer/selection API 重写当前 Ghostty 私有选区链路，提供 renderer-neutral 的 viewport range text、selection position、clear selection 和 debug bridge。
- [x] Task 3: 改造 `renderer.ts` 和 `TerminalPane.tsx`，删除 Ghostty renderer kind 分支、Ghostty 输入说明、Ghostty dispose/theme 假设，收口为 xterm.js 单一路径。
- [x] Task 4: 改造 `App.tsx` theme 行为，删除 Ghostty 专属 full snapshot resync 逻辑，改成 xterm.js 原地 theme 更新并验证 attach 会话不抖动。
- [x] Task 5: 清理样式层 Ghostty 专有 DOM/CSS 假设，删除 `.terminal-host-grid-filler` 和相关 canvas/layout hack，保留 xterm.js textarea/input 可访问性与移动端 IME 定位需求。
- [x] Task 6: 重写 Vitest mock、renderer 单测、TerminalPane/App 测试和 Playwright smoke/real-relay 断言，去掉 `__TERMD_DEBUG_GHOSTTY__`、`selectionManager`、Ghostty wrapper/filler 等假设，补齐 xterm.js 原生选区、theme 热更新、scrollback、输入粘贴回归。
- [x] Task 7: 删除 `ghostty-web` 依赖、旧 renderer 文件、旧测试文件和遗留注释，确保生产代码、测试代码、文档注释里不再宣称 Ghostty 是当前 Web renderer。
- [x] Task 8: 执行完整验证：前端 typecheck、build、目标 Vitest、全量 Vitest、关键 Playwright smoke；必要时补 direct/relay 浏览器链路验证。
- [x] Task 9: 对每个功能任务完成结果做两轮 subagent 审核，所有阻断问题修复后再打勾，最后做一次全局复审。

## Verification Targets

- `pnpm --dir termui/frontend exec tsc --noEmit -p tsconfig.json --pretty false`
- `pnpm --dir termui/frontend exec vite build`
- `pnpm --dir termui/frontend exec vitest run src/__tests__/xterm-renderer.test.ts src/__tests__/terminal-pane.test.tsx src/__tests__/terminal-renderer-factory.test.ts src/__tests__/terminal-renderer-race.test.tsx`
- `pnpm --dir termui/frontend exec vitest run`
- `pnpm --dir termui/frontend exec playwright test tests/termui-web.smoke.spec.ts --project=chromium`
- 如 real relay 用例受影响：`pnpm --dir termui/frontend exec playwright test tests/termui-web.real-relay.spec.ts --project=chromium`

## Cleanup Standard

- `ghostty-web` 不再出现在 `package.json`、生产代码 import、测试 mock、Playwright 断言和 UI 注释中
- 不保留 `__TERMD_DEBUG_GHOSTTY__`、`selectionManager`、Ghostty metrics/canvas filler/requestRender 私有补丁
- 不新增“以后再切”的过渡 TODO；本次直接收敛成 xterm.js 单栈
