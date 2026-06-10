# Desktop IME Input Sink Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端中文输入法回到 ghostty-web 的原生输入 sink，保证空格和标点在 composition 场景下正常提交。

**Architecture:** 保持 `ghostty-web` 作为唯一终端渲染器，不引入额外输入层。桌面端让 terminal 的真实输入落到隐藏 `textarea`，同时继续通过 `host` 的 focus/resize 监听维护 session 状态。

**Tech Stack:** React + TypeScript, `ghostty-web`, Vitest, Playwright.

---

### Task 1: Restore desktop input sink behavior

**Files:**
- Modify: `termui/frontend/src/components/TerminalPane.tsx`
- Modify: `termui/frontend/src/__tests__/terminal-pane.test.tsx`
- Modify: `termui/frontend/tests/termui-web.smoke.spec.ts`

- [ ] **Step 1: Update desktop focus flow**

```ts
const focusTerminalInputSink = (terminal: TerminalRendererTerminal | null = terminalRef.current) => {
  const input = resolveTerminalInputElement();
  if (!input) {
    terminal?.focus();
    return;
  }
  terminal?.focus();
  try {
    input.focus({ preventScroll: true });
  } catch {
    input.focus();
  }
};
```

- [ ] **Step 2: Bridge host focus to the real input on desktop too**

```ts
const handleHostFocusBridge = (event: FocusEvent) => {
  const target = event.target;
  if (!(target instanceof HTMLElement)) {
    return;
  }
  if (target !== host && target !== terminal.element) {
    return;
  }
  if (terminalSelectionDragRef.current?.active || terminalSelectionFocusPendingRef.current) {
    return;
  }
  const helperTextarea = resolveTerminalInputElement(host);
  if (!helperTextarea || document.activeElement === helperTextarea) {
    return;
  }
  try {
    helperTextarea.focus({ preventScroll: true });
  } catch {
    helperTextarea.focus();
  }
};
```

- [ ] **Step 3: Update desktop focus assertions**

```ts
await waitFor(() => expect(document.activeElement).toBe(textarea));
```

- [ ] **Step 4: Update browser smoke regression**

```ts
await expect(terminalTextarea).toBeFocused();
await expect(terminalHost).not.toBeFocused();
```

- [ ] **Step 5: Verify**

Run:
```bash
pnpm --dir termui/frontend vitest run src/__tests__/terminal-pane.test.tsx src/__tests__/app.test.tsx
pnpm --dir termui/frontend playwright test tests/termui-web.smoke.spec.ts --project=chromium
```
Expected: exit 0.

**Self-review checklist**
- `ghostty-web` 仍是唯一终端实现。
- 桌面端输入 sink 不再停留在 host-only 焦点。
- IME 相关回归测试覆盖 desktop click/focus path。
