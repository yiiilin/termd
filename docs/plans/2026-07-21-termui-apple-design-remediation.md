# Termui Web Interaction and Responsive Remediation

## Goal

在不改变 daemon、relay、session 权限模型和终端数据协议的前提下，提升 termui Web 的用户掌控感、数据安全、响应式层级、可访问性和交互反馈。

本轮只实施 `/termui/frontend`。Flutter `/termui/native` 继续保持架构骨架，不与 Web 改造同步扩展。文件传输取消若暴露新的服务端或协议前置条件，必须先更新本计划并停止该工作包。

## Approved Contracts

- terminal 始终是工作台主表面；窄屏优先保留 session 名和终端可用宽度。
- 未保存文件不得因关闭、遮罩、Escape 或切换 session 静默丢失。
- 终止 session、删除文件或目录、Git discard、忘记 daemon 只有在明确确认后才能发送 mutation。
- 可恢复操作不增加确认；Stage、Unstage、Rename 等保持直接执行。
- 移动快捷键配置使用本地草稿；不完整输入不得在编辑过程中消失。
- coarse pointer 的主要命中区目标为 44px；用户缩放、safe-area 和辅助偏好必须得到支持。
- 不引入整套 UI 组件库或动画运行时依赖；优先使用现有 React、Pointer Events、CSS 和浏览器原语。

## Test Seams

- Component seam：通过可见控件、可访问角色、焦点和用户输入观察 Settings、Dialog、File Editor 行为。
- App integration seam：通过现有 `MockDaemon` 观察用户动作、请求数量、状态变化和取消路径。
- Browser seam：通过 Playwright 观察受影响 viewport 的 DOM 几何、可访问状态、终端输入和浏览器错误。
- Pure seam：解析、颜色对比和布局辅助函数仅在纯逻辑足以完整证明契约时使用单元测试。

## Responsive Contract

- `>= 1280px`：sidebar、terminal、files 三栏可完整显示；从受限桌面扩宽时保留用户当前 panel 状态，不自动挤压 terminal。
- `901px - 1279px`：sidebar 与 files 默认收起，terminal 保持主导，两个 panel 仍可按需展开。
- `761px - 900px`：sidebar 与 files 默认收起为 rail。
- `<= 760px`：Sessions 和 Files 使用覆盖面板，terminal 保持全宽。
- 移动 toolbar 优先级：session 名、连接异常、RTT、终端行列数。

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any current-scope task remains unchecked, the UI remediation is not complete
- Explicitly deferred protocol follow-ups remain unchecked and do not block this frontend-only remediation

## Tasks

- [x] 记录并验证当前 frontend 基线：typecheck、单测、build、桌面和移动 smoke
- [x] 建立共享可访问 Dialog 基础，并验证初始焦点、Tab 约束、Escape 和焦点恢复
- [x] 提升 File Editor dirty/close-request 状态，覆盖 Save、Discard、Stay 和 session 切换
- [x] 为 session close、文件删除、Git discard、忘记 daemon 增加对象明确的确认
- [x] 将移动快捷键设置改为草稿 + Apply/Cancel + 行内校验
- [x] 为文件列表局部错误保留缓存内容，并提供 dismiss/retry 语义
- [x] 验证上传/下载 abort 契约；若可靠，实现 Cancel/cancelling/cancelled 和晚到回调隔离
- [ ] Deferred protocol follow-up：另行评审并升级上传 abort 协议，包括客户端 transfer identity、幂等 abort 和 completed/aborted 明确结果；完成前不在 UI 声称 cancelled
- [x] 重构未配对首屏，隐藏无意义的空 summary、daemon manager 和重复 Workspace 入口
- [x] 实施 terminal-first 响应式 panel 与 toolbar 优先级
- [x] 修正移动命中区、用户缩放、safe-area、modal/menu 键盘路径
- [x] 修正 light/dark 语义颜色 token、terminal 光标/前景对比和高对比模式
- [x] 增加通用 pressed 反馈、可中断 pull 回弹，并补齐 reduced motion/transparency 偏好
- [x] 运行 frontend 全量验证与受影响 viewport 的 Chromium 桌面/移动视觉 QA
- [x] 确认未提交、未打 tag、未发版，并清理本轮临时测试产物

## Stop Conditions

- dirty 状态无法在 attach 前可靠协调时，先提升状态所有权，不允许只给关闭按钮加局部确认。
- 服务端 abort 不幂等或不能保证传输资源清理时，停止传输取消实现并单独评审协议范围。
- safe-area、缩放或命中区改造导致 IME、terminal fit、长按重复或滚动取消回归时，回滚该工作包。
- AA 对比无法在现有 Everforest 方向内达成时，停止 token 选型并提供替代方案。
- 任一改动需要新增运行时依赖时，先停止并说明必要性。

## Verification

每个工作包先运行对应的 focused Vitest；完成后至少运行：

```bash
cd termui/frontend
npm run typecheck
npm run test -- --run
npm run build
```

最终再运行选定 Playwright 项目和仓库 `bash scripts/qa.sh`。测试环境仅使用本地 `MockDaemon`、Playwright webServer 和任务创建的临时资源。
