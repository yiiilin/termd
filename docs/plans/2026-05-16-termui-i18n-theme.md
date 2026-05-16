# Termui i18n and Theme Preferences

## Goal

实现 Web 客户端本地偏好设置：

- 支持语言 `auto`、`zh-CN`、`en-US`
- 支持主题 `system`、`dark`、`light`
- 在工作台 `Clients`、`Daemons` 右侧提供设置入口，移动端菜单也可进入
- 偏好只保存在当前浏览器本地状态，不写入 daemon，不影响 session 和 supervisor
- xterm、Monaco、管理页、工作台、文件/Git panel 都随主题同步

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] Add typed browser preferences to local state with normalization and persistence tests.
- [x] Add a small typed i18n layer and wire it through the main app shell and existing panels.
- [x] Add Settings UI entry points and a modal for language/theme configuration.
- [x] Convert the UI color system to CSS variables with dark/light theme definitions.
- [x] Connect effective theme to xterm and Monaco without breaking resize/session behavior.
- [x] Add focused integration tests for settings persistence, language switching, and theme switching.
- [x] Run frontend typecheck/tests/build and browser visual checks before completion.
