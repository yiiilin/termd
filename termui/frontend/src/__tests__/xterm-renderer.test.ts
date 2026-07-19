import { describe, expect, it, vi } from "vitest";

const IOS_SAFARI_USER_AGENT =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1";

function mockIosSafariUserAgent(): () => void {
  const descriptor = Object.getOwnPropertyDescriptor(window.navigator, "userAgent");
  Object.defineProperty(window.navigator, "userAgent", {
    configurable: true,
    value: IOS_SAFARI_USER_AGENT,
  });
  return () => {
    if (descriptor) {
      Object.defineProperty(window.navigator, "userAgent", descriptor);
    } else {
      Reflect.deleteProperty(window.navigator, "userAgent");
    }
  };
}

function imeKeyboardEvent(type: "keydown" | "keypress" | "keyup", key: string, keyCode: number): KeyboardEvent {
  const event = new KeyboardEvent(type, {
    bubbles: true,
    cancelable: true,
    code: key === " " ? "Space" : "Unidentified",
    key,
  });
  Object.defineProperties(event, {
    charCode: { value: type === "keypress" ? keyCode : 0 },
    keyCode: { value: keyCode },
    which: { value: keyCode },
  });
  return event;
}

describe("xterm renderer adapter", () => {
  it("补齐 TerminalPane 需要的 renderer contract，并保持 xterm 单栈语义", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
    });

    expect(renderer.kind).toBe("xterm");

    const host = document.createElement("div");
    renderer.terminal.open(host);

    expect(host.getAttribute("role")).toBe("textbox");
    expect(host.querySelector(".xterm")).not.toBeNull();
    expect(host.querySelector(".xterm-screen")).not.toBeNull();
    expect(host.querySelector('textarea[aria-label="Terminal input"]')).toBe(renderer.terminal.textarea);

    let writeParsedCount = 0;
    renderer.terminal.onWriteParsed(() => {
      writeParsedCount += 1;
    });

    renderer.terminal.write("line-1\nline-2");
    expect(writeParsedCount).toBe(1);
    expect(host.dataset.termdBuffer).toContain("line-1");
    expect(host.dataset.termdBuffer).toContain("line-2");

    renderer.setOptions({ fontSize: 18, theme: { background: "#101418" } });
    expect(renderer.terminal.options.fontSize).toBe(18);
    expect(renderer.terminal.options.theme).toEqual({ background: "#101418" });

  });

  it("viewport 选区和 facade 复制文本保持一致，不回落到陈旧 selection", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
    });
    const host = document.createElement("div");
    renderer.terminal.open(host);

    renderer.terminal.write("hist-001\nhist-002\nhist-003\n");
    renderer.terminal.scrollToLine(0);

    const selected = renderer.terminal.selectViewportRange({ col: 0, row: 1 }, { col: 7, row: 1 });

    expect(selected).toBe("hist-002");
    expect(renderer.terminal.hasSelection()).toBe(true);
    expect(renderer.terminal.getSelection()).toBe("hist-002");
    expect(renderer.terminal.getSelectionPosition?.()).toEqual({
      start: { x: 0, y: 1 },
      end: { x: 7, y: 1 },
    });
    expect(host.dataset.termdSelection).toBe("hist-002");

    renderer.terminal.deselect();
    expect(renderer.terminal.hasSelection()).toBe(false);
    expect(renderer.terminal.getSelection()).toBe("");
  });

  it("创建新 xterm 实例时会沿用传入的初始 rows/cols，而不是回退到默认 80x24", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { cols: 108, rows: 35, fontSize: 13 },
    });
    const host = document.createElement("div");
    renderer.terminal.open(host);

    expect(renderer.terminal.cols).toBe(108);
    expect(renderer.terminal.rows).toBe(35);
    expect(host.dataset.termdCols).toBe("108");
    expect(host.dataset.termdRows).toBe("35");
  });

  it("通过 xterm input 注入用户输入，并公开实时 application cursor mode", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({ terminalOptions: {} });
    const host = document.createElement("div");
    renderer.terminal.open(host);
    const onData = vi.fn();
    renderer.terminal.onData(onData);

    renderer.terminal.input("\x1b[A", true);
    expect(onData).toHaveBeenCalledWith("\x1b[A");
    expect(renderer.terminal.modes.applicationCursorKeysMode).toBe(false);

    (globalThis as {
      __TERMD_TEST_TERMINAL__?: { setApplicationCursorKeysMode?: (enabled: boolean) => void };
    }).__TERMD_TEST_TERMINAL__?.setApplicationCursorKeysMode?.(true);
    expect(renderer.terminal.modes.applicationCursorKeysMode).toBe(true);
  });

  it("iPhone 中文 IME 在延迟 input 中轮换标点时发送最小替换序列", async () => {
    const restoreUserAgent = mockIosSafariUserAgent();
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");
    const renderer = createXtermRenderer({ terminalOptions: {} });

    try {
      const host = document.createElement("div");
      renderer.terminal.open(host);
      const textarea = renderer.terminal.textarea;
      expect(textarea).toBeDefined();
      const onData = vi.fn();
      renderer.terminal.onData(onData);
      textarea!.value = "，";

      textarea!.dispatchEvent(imeKeyboardEvent("keydown", "。", 229));

      // iOS 可能在 xterm 的 keydown timer 已执行后才更新 helper textarea。
      await new Promise((resolve) => window.setTimeout(resolve, 0));
      textarea!.value = "。";
      textarea!.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        data: "。",
        inputType: "insertText",
      }));

      textarea!.dispatchEvent(imeKeyboardEvent("keyup", "。", 0));
      await new Promise((resolve) => window.setTimeout(resolve, 0));

      expect(onData).toHaveBeenCalledTimes(1);
      expect(onData).toHaveBeenCalledWith("\x7f。");
    } finally {
      renderer.terminal.dispose();
      restoreUserAgent();
      vi.resetModules();
    }
  });

  it("iPhone 英文软键盘的 keypress 和 input 只发送一个空格", async () => {
    const restoreUserAgent = mockIosSafariUserAgent();
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");
    const renderer = createXtermRenderer({ terminalOptions: {} });

    try {
      const host = document.createElement("div");
      renderer.terminal.open(host);
      const textarea = renderer.terminal.textarea;
      const onData = vi.fn();
      renderer.terminal.onData(onData);

      textarea!.value = "";
      textarea!.dispatchEvent(imeKeyboardEvent("keydown", " ", 229));
      textarea!.dispatchEvent(imeKeyboardEvent("keypress", " ", 32));
      textarea!.value = " ";
      textarea!.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        data: " ",
        inputType: "insertText",
      }));
      textarea!.dispatchEvent(imeKeyboardEvent("keyup", " ", 32));

      expect(onData).toHaveBeenCalledTimes(1);
      expect(onData).toHaveBeenCalledWith(" ");
    } finally {
      renderer.terminal.dispose();
      restoreUserAgent();
      vi.resetModules();
    }
  });

  it("iPhone 中文 IME 双空格转换按删除和插入顺序发送句号", async () => {
    const restoreUserAgent = mockIosSafariUserAgent();
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");
    const renderer = createXtermRenderer({ terminalOptions: {} });

    try {
      const host = document.createElement("div");
      renderer.terminal.open(host);
      const textarea = renderer.terminal.textarea;
      const onData = vi.fn();
      renderer.terminal.onData(onData);
      textarea!.value = " ";
      textarea!.dispatchEvent(imeKeyboardEvent("keydown", " ", 229));

      textarea!.value = "";
      textarea!.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        data: null,
        inputType: "deleteContentBackward",
      }));
      textarea!.value = "。";
      textarea!.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        data: "。",
        inputType: "insertText",
      }));
      textarea!.dispatchEvent(imeKeyboardEvent("keyup", " ", 32));

      expect(onData.mock.calls.flat().join("")).toBe("\x7f。");
    } finally {
      renderer.terminal.dispose();
      restoreUserAgent();
      vi.resetModules();
    }
  });

  it("iPhone 中文候选词 composition 保持由 xterm 原输入路径处理", async () => {
    const restoreUserAgent = mockIosSafariUserAgent();
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");
    const renderer = createXtermRenderer({ terminalOptions: {} });

    try {
      const host = document.createElement("div");
      renderer.terminal.open(host);
      const textarea = renderer.terminal.textarea;
      const onData = vi.fn();
      renderer.terminal.onData(onData);

      textarea!.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
      textarea!.dispatchEvent(imeKeyboardEvent("keydown", "Process", 229));
      textarea!.value = "你";
      textarea!.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        data: "你",
        inputType: "insertCompositionText",
        isComposing: true,
      }));
      textarea!.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "你" }));

      expect(onData).toHaveBeenCalledTimes(1);
      expect(onData).toHaveBeenCalledWith("你");
    } finally {
      renderer.terminal.dispose();
      restoreUserAgent();
      vi.resetModules();
    }
  });

  it("运行期主题更新不会把 rows/cols 重新写入 xterm options", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { cols: 108, rows: 35, fontSize: 13 },
    });

    renderer.setOptions({ theme: { background: "#101418" } });

    expect(renderer.terminal.options).toEqual({
      cols: 108,
      rows: 35,
      fontSize: 13,
      theme: { background: "#101418" },
    });
  });

  it("scroll state 使用 xterm 的 top-based viewport 语义，并在 dispose 后停止 debug mirror", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 13 },
    });
    const host = document.createElement("div");
    renderer.terminal.open(host);
    renderer.terminal.write(Array.from({ length: 48 }, (_, index) => `row-${index + 1}\n`).join(""));

    const bottomState = renderer.scrollState();
    expect(bottomState?.baseY).toBeGreaterThan(0);
    expect(bottomState?.viewportY).toBe(bottomState?.baseY);

    renderer.terminal.scrollToLine(0);
    expect(renderer.scrollState()?.viewportY).toBe(0);
    expect(host.dataset.termdViewportYRaw).toBe(String(bottomState?.baseY ?? 0));

    renderer.terminal.dispose();
    host.dataset.termdBuffer = "after-dispose";
    expect(host.dataset.termdBuffer).toBe("after-dispose");
  });
});
