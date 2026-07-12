import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TerminalPane, type TerminalOutputItem } from "../components/TerminalPane";
import type { SessionSearchResultPayload, TerminalSize } from "../protocol/types";

const animationFrameMs = 16;
const DEFAULT_TERMINAL_SIZE = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
let mockedDocumentVisibilityState: DocumentVisibilityState = "visible";
let mockedDocumentHasFocus = true;

function setDocumentVisibility(state: DocumentVisibilityState): void {
  mockedDocumentVisibilityState = state;
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => mockedDocumentVisibilityState,
  });
  Object.defineProperty(document, "hidden", {
    configurable: true,
    get: () => mockedDocumentVisibilityState === "hidden",
  });
  document.dispatchEvent(new Event("visibilitychange"));
}

function setDocumentHasFocus(focused: boolean): void {
  mockedDocumentHasFocus = focused;
  Object.defineProperty(document, "hasFocus", {
    configurable: true,
    value: () => mockedDocumentHasFocus,
  });
}

function restoreDocumentVisibility(): void {
  mockedDocumentVisibilityState = "visible";
  mockedDocumentHasFocus = true;
  Reflect.deleteProperty(document, "visibilityState");
  Reflect.deleteProperty(document, "hidden");
  Reflect.deleteProperty(document, "hasFocus");
}

function fireTouchPointer(
  target: HTMLElement,
  type: "pointerdown" | "pointermove" | "pointerup" | "pointercancel",
  options: { pointerId: number; clientX: number; clientY: number },
) {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: options.pointerId },
    pointerType: { value: "touch" },
    button: { value: 0 },
    clientX: { value: options.clientX },
    clientY: { value: options.clientY },
  });
  fireEvent(target, event);
}

function installMutableVisualViewport(layoutHeight: number, visualHeight: number, offsetTop = 0) {
  let metrics = { layoutHeight, visualHeight, offsetTop };
  const target = new EventTarget();
  const originalInnerHeight = Object.getOwnPropertyDescriptor(window, "innerHeight");
  const originalVisualViewport = Object.getOwnPropertyDescriptor(window, "visualViewport");
  const viewport = {
    get height() {
      return metrics.visualHeight;
    },
    get width() {
      return window.innerWidth;
    },
    get offsetTop() {
      return metrics.offsetTop;
    },
    get offsetLeft() {
      return 0;
    },
    get pageTop() {
      return metrics.offsetTop;
    },
    get pageLeft() {
      return 0;
    },
    get scale() {
      return 1;
    },
    addEventListener: target.addEventListener.bind(target),
    removeEventListener: target.removeEventListener.bind(target),
    dispatchEvent: target.dispatchEvent.bind(target),
  } as unknown as VisualViewport;
  Object.defineProperty(window, "innerHeight", {
    configurable: true,
    get: () => metrics.layoutHeight,
  });
  Object.defineProperty(window, "visualViewport", {
    configurable: true,
    value: viewport,
    writable: true,
  });
  return {
    setMetrics(nextLayoutHeight: number, nextVisualHeight: number, nextOffsetTop = 0) {
      metrics = { layoutHeight: nextLayoutHeight, visualHeight: nextVisualHeight, offsetTop: nextOffsetTop };
      target.dispatchEvent(new Event("resize"));
    },
    restore() {
      if (originalInnerHeight) {
        Object.defineProperty(window, "innerHeight", originalInnerHeight);
      } else {
        Reflect.deleteProperty(window, "innerHeight");
      }
      if (originalVisualViewport) {
        Object.defineProperty(window, "visualViewport", originalVisualViewport);
      } else {
        Reflect.deleteProperty(window, "visualViewport");
      }
    },
  };
}

function renderMobileTerminalPane(onInput?: (data: string) => void): {
  frame: HTMLElement;
  host: HTMLElement;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
};
function renderMobileTerminalPane(options?: {
  onInput?: (data: string) => void;
  onResize?: (size: TerminalSize) => void;
}): {
  frame: HTMLElement;
  host: HTMLElement;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
};
function renderMobileTerminalPane(
  inputOrOptions:
    | ((data: string) => void)
    | {
      onInput?: (data: string) => void;
      onResize?: (size: TerminalSize) => void;
    }
    = {},
) {
  const takeOutput = vi.fn(() => []);
  const registerOutputDrain = vi.fn(() => () => undefined);
  const options =
    typeof inputOrOptions === "function"
      ? { onInput: inputOrOptions }
      : inputOrOptions;
  const onInput = options.onInput ?? vi.fn();
  const onResize = options.onResize ?? vi.fn();
  render(
    <TerminalPane
      attached
      sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
      mobileInputMode
      outputResetVersion={0}
      takeOutput={takeOutput}
      registerOutputDrain={registerOutputDrain}
      onInput={onInput}
      onResize={onResize}
      onCursorChange={vi.fn()}
    />,
  );
  const pane = screen.getByTestId("terminal-pane");
  return {
    frame: pane.querySelector<HTMLElement>(".terminal-frame")!,
    host: pane.querySelector<HTMLElement>(".terminal-host")!,
    onInput,
    onResize,
  };
}

function mockTerminalLayout(input: { viewportWidth: number; viewportHeight: number }) {
  let viewportHeight = input.viewportHeight;
  const clientWidthSpy = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? input.viewportWidth : 0;
  });
  const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? viewportHeight : 0;
  });
  const offsetWidthSpy = vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-frame") ? Number.parseFloat(this.style.width || "0") : 0;
  });
  const offsetHeightSpy = vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-frame") ? Number.parseFloat(this.style.height || "0") : 0;
  });
  const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport")
      ? Math.max(viewportHeight, Number.parseFloat(this.querySelector<HTMLElement>(".terminal-frame")?.style.height || "0"))
      : 0;
  });

  return {
    setViewportHeight(nextHeight: number) {
      viewportHeight = nextHeight;
    },
    restore() {
      clientWidthSpy.mockRestore();
      clientHeightSpy.mockRestore();
      offsetWidthSpy.mockRestore();
      offsetHeightSpy.mockRestore();
      scrollHeightSpy.mockRestore();
    },
  };
}

function terminalHostScale(): number {
  const host = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-host");
  expect(host).not.toBeNull();
  const match = /scale\(([^)]+)\)/.exec(host!.style.transform);
  return match ? Number.parseFloat(match[1]) : 1;
}

function activateMobileDirectionGesture(frame: HTMLElement, pointerId: number, startX = 160, startY = 240) {
  fireTouchPointer(frame, "pointerdown", {
    pointerId,
    clientX: startX,
    clientY: startY,
  });
  act(() => {
    vi.advanceTimersByTime(1000);
  });
  expect(screen.queryByLabelText("mobile direction gesture")).toBeNull();
}

function renderTerminalPaneWithOutput(items: TerminalOutputItem[], options: {
  onTerminalResync?: (lastTerminalSeq?: number) => void;
  onTerminalSeqRendered?: (terminalSeq: number) => void;
} = {}) {
  const queue = [...items];
  const takeOutput = vi.fn(() => queue.splice(0));
  const registerOutputDrain = vi.fn((drain: () => void) => {
    drain();
    return () => undefined;
  });
  render(
    <TerminalPane
      attached
      sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
      outputResetVersion={0}
      takeOutput={takeOutput}
      registerOutputDrain={registerOutputDrain}
      onTerminalResync={options.onTerminalResync}
      onTerminalSeqRendered={options.onTerminalSeqRendered}
      onInput={vi.fn()}
      onResize={vi.fn()}
      onCursorChange={vi.fn()}
    />,
  );
}

function terminalHost(): HTMLElement {
  const host = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-host");
  expect(host).not.toBeNull();
  return host!;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((innerResolve, innerReject) => {
    resolve = innerResolve;
    reject = innerReject;
  });
  return { promise, resolve, reject };
}

async function flushPasteShortcutFallbackTick() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

function searchResult(query: string, matchCount: number): SessionSearchResultPayload {
  return {
    session_id: "00000000-0000-0000-0000-000000000401",
    query,
    line_count: 4,
    matches: Array.from({ length: matchCount }, (_, index) => ({
      line_index: index,
      column_index: 0,
      line_text: `${query}-${index}`,
    })),
    truncated: false,
  };
}

describe("TerminalPane terminal sequence rendering", () => {
  it("TerminalPane 不直接绑定 xterm 私有 DOM 或具体 renderer import", () => {
    const source = readFileSync(resolve(process.cwd(), "src/components/TerminalPane.tsx"), "utf8");

    expect(source).not.toContain("@xterm/");
    const legacyRemovedWrapperSelector = [".", "ghost", "ty", "-", "terminal"].join("");
    expect(source).not.toContain(legacyRemovedWrapperSelector);
    expect(source).not.toContain("_syncTextArea");
  });

  it("detach cleanup 会清理 E2E debug buffer 镜像，避免旧明文残留在 host dataset", async () => {
    let queue: TerminalOutputItem[] = [
      { kind: "snapshot", bytes: new TextEncoder().encode("debug-old-session\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ];
    const takeOutput = vi.fn(() => queue.splice(0));
    const registerOutputDrain = vi.fn((drain: () => void) => {
      drain();
      return () => undefined;
    });
    const props = {
      sessionSize: DEFAULT_TERMINAL_SIZE,
      outputResetVersion: 0,
      takeOutput,
      registerOutputDrain,
      onInput: vi.fn(),
      onResize: vi.fn(),
      onCursorChange: vi.fn(),
    };
    const { rerender } = render(<TerminalPane attached {...props} />);

    await waitFor(() => expect(terminalHost().dataset.termdBuffer).toContain("debug-old-session"));

    rerender(<TerminalPane attached={false} {...props} />);

    await waitFor(() => expect(terminalHost().dataset.termdBuffer).toBeUndefined());
    expect(terminalHost().dataset.buffer).toBeUndefined();
  });

  it("隐藏 xterm textarea 不重复暴露 Terminal input 可访问名称", async () => {
    renderTerminalPaneWithOutput([]);

    const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
    expect(terminalInput).toHaveClass("terminal-host");
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();
    expect(textarea).toHaveAttribute("aria-hidden", "true");
  });

  it("可见 Terminal input 获得焦点时会桥接到真实输入 textarea", async () => {
    renderTerminalPaneWithOutput([]);

    const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();

    act(() => {
      terminalInput.focus();
    });

    // 中文注释：桌面端的可见终端仍由 host 承载 role/布局，
    // 但真实键盘与中文 IME 输入要落到 xterm.js 的 textarea 输入 sink。
    await waitFor(() => expect(document.activeElement).toBe(textarea));
    expect(document.activeElement).not.toBe(terminalInput);
  });

  it("移动端 pointerdown 不会立即聚焦 helper textarea，避免触摸滚动时误弹键盘", () => {
    const onResize = vi.fn();
    const { host } = renderMobileTerminalPane({ onResize });
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();
    const focusSpy = vi.spyOn(textarea!, "focus");

    fireTouchPointer(host, "pointerdown", {
      pointerId: 1,
      clientX: 24,
      clientY: 24,
    });

    // 中文注释：用户此时可能只是准备拖动查看 scrollback。触摸落下本身不应立即
    // 再次拉起 helper textarea，否则软键盘会抢占滚动手势，表现成“点一下就没法滚”。
    expect(focusSpy).not.toHaveBeenCalled();
    expect(onResize).not.toHaveBeenCalled();
    focusSpy.mockRestore();
  });

  it("移动端明确 tap/click 终端后，经由 host focus bridge 仍会把焦点转交到 helper textarea", async () => {
    const { host } = renderMobileTerminalPane();
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();

    fireTouchPointer(host, "pointerdown", {
      pointerId: 7,
      clientX: 24,
      clientY: 24,
    });
    fireTouchPointer(host, "pointerup", {
      pointerId: 7,
      clientX: 24,
      clientY: 24,
    });

    fireEvent.click(host, { clientX: 24, clientY: 24, button: 0 });

    // 中文注释：真实浏览器里常见时序是 click 先让 host 拿到 focus，
    // 再由 focus bridge 同步转交给 helper textarea。这里仍要验证整条链路，
    // 但触发边界应该是 tap/click，而不是 pointerdown。
    await waitFor(() => expect(document.activeElement).toBe(textarea));
  });

  it("移动端 tap/click 终端后会恢复 helper textarea 焦点，并继续获得 active focus 的 resize 权限", async () => {
    const onResize = vi.fn();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 31,
      cols: 101,
    };
    const { host } = renderMobileTerminalPane({ onResize });
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();

    fireTouchPointer(host, "pointerdown", {
      pointerId: 11,
      clientX: 24,
      clientY: 24,
    });
    fireTouchPointer(host, "pointerup", {
      pointerId: 11,
      clientX: 24,
      clientY: 24,
    });
    fireEvent.click(host, { clientX: 24, clientY: 24, button: 0 });
    await waitFor(() => expect(document.activeElement).toBe(textarea));
    expect(onResize).not.toHaveBeenCalled();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 32,
      cols: 102,
    };
    fireEvent(window, new Event("resize"));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 800));
    });
    fireEvent(window, new Event("resize"));
    await waitFor(() =>
      expect(onResize).toHaveBeenCalledWith({
        rows: 32,
        cols: 102,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      }),
    );
  });

  it("移动端触摸拖动滚动终端时不会聚焦 helper textarea", async () => {
    vi.useFakeTimers();
    try {
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={vi.fn<() => TerminalOutputItem[]>(() => [
            { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
            { kind: "output", bytes: new TextEncoder().encode(Array.from({ length: 80 }, (_, index) => `line-${index}\n`).join("")), terminalSeq: 1 },
          ])}
          registerOutputDrain={vi.fn((drain: () => void) => {
            drain();
            return () => undefined;
          })}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const host = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-host");
      const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(host).not.toBeNull();
      expect(textarea).not.toBeNull();
      const focusSpy = vi.spyOn(textarea!, "focus");

      fireTouchPointer(host!, "pointerdown", {
        pointerId: 88,
        clientX: 160,
        clientY: 196,
      });
      expect(focusSpy).not.toHaveBeenCalled();

      fireTouchPointer(host!, "pointermove", {
        pointerId: 88,
        clientX: 160,
        clientY: 260,
      });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：明确进入纵向 scroll 手势后，helper textarea 仍不应被聚焦。
      expect(focusSpy).not.toHaveBeenCalled();
      focusSpy.mockRestore();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端纵向滚动被 capture 消费后也会收回临时 focus 放行", async () => {
    vi.useFakeTimers();
    try {
      const snapshot = Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("");
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={vi.fn<() => TerminalOutputItem[]>(() => [
            {
              kind: "snapshot",
              bytes: new TextEncoder().encode(snapshot),
              baseSeq: 30,
              size: DEFAULT_TERMINAL_SIZE,
            },
          ])}
          registerOutputDrain={vi.fn((drain: () => void) => {
            drain();
            return () => undefined;
          })}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });
      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(frame).not.toBeNull();
      expect(textarea).not.toBeNull();

      act(() => {
        textarea!.focus();
      });
      expect(document.activeElement).toBe(textarea);
      act(() => {
        textarea!.blur();
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(160);
      });

      fireTouchPointer(frame!, "pointerdown", {
        pointerId: 14,
        clientX: 160,
        clientY: 196,
      });
      fireTouchPointer(frame!, "pointermove", {
        pointerId: 14,
        clientX: 160,
        clientY: 260,
      });
      fireTouchPointer(frame!, "pointerup", {
        pointerId: 14,
        clientX: 160,
        clientY: 260,
      });

      // 中文注释：滚动手势在 capture 阶段会 stopPropagation，bubble 阶段的
      // direction pointerup 清理不会执行；TerminalPane 必须自己收回这次临时 focus
      // 许可，但不能主动 blur helper textarea，避免真实软键盘被滚动误判关闭。
      textarea!.focus();
      await act(async () => {
        await Promise.resolve();
      });
      expect(document.activeElement).toBe(textarea);
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端 pointerup 后挂起的 bypass timer 会在 unmount 时清理", async () => {
    vi.useFakeTimers();
    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      const renderResult = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();

      const timerCountBeforePointerEnd = vi.getTimerCount();
      fireTouchPointer(frame!, "pointerdown", {
        pointerId: 9,
        clientX: 24,
        clientY: 24,
      });
      fireTouchPointer(frame!, "pointerup", {
        pointerId: 9,
        clientX: 24,
        clientY: 24,
      });
      const timerCountAfterPointerEnd = vi.getTimerCount();
      expect(timerCountAfterPointerEnd).toBeGreaterThan(timerCountBeforePointerEnd);

      renderResult.unmount();
      expect(vi.getTimerCount()).toBeLessThan(timerCountAfterPointerEnd);
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端早到 helper focus 仍可输入，但不会提前上报 resize", async () => {
    const onInput = vi.fn();
    const onResize = vi.fn();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 31,
      cols: 101,
    };
    const { frame } = renderMobileTerminalPane({ onInput, onResize });
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();

    textarea!.focus();
    await waitFor(() => expect(document.activeElement).toBe(textarea));
    textarea!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });
    onInput.mockClear();
    onResize.mockClear();

    fireTouchPointer(frame, "pointerdown", {
      pointerId: 5,
      clientX: 24,
      clientY: 24,
    });
    textarea!.focus();
    await waitFor(() => expect(document.activeElement).toBe(textarea));

    const beforeInputEvent = new InputEvent("beforeinput", {
      bubbles: true,
      cancelable: true,
      inputType: "insertText",
      data: "x",
    });
    textarea!.dispatchEvent(beforeInputEvent);

    expect(onInput).toHaveBeenCalledWith("x");
    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端 focusRequest 会恢复 active focus 与后续 layout resize authority", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    const renderResult = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    onResize.mockClear();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 31,
      cols: 101,
    };
    renderResult.rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        focusRequest={1}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 8));
    });

    // 中文注释：仅靠一轮 armed focus resize 还不够；focusRequest 必须把终端
    // 从 passive helper focus 提升成真正 active focus，这样后续 layout resize
    // 才还能继续上报给 daemon。
    onResize.mockClear();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 32,
      cols: 102,
    };
    fireEvent(window, new Event("resize"));
    await waitFor(() =>
      expect(onResize).toHaveBeenCalledWith({
        rows: 32,
        cols: 102,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      }),
    );
  });

  it("移动端早到 helper focus 状态下 paste 和组合输入仍可用，且不会提前上报 resize", async () => {
    const onInput = vi.fn();
    const onResize = vi.fn();
    const { frame } = renderMobileTerminalPane({ onInput, onResize });
    const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(textarea).not.toBeNull();

    textarea!.focus();
    await waitFor(() => expect(document.activeElement).toBe(textarea));
    textarea!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });
    onInput.mockClear();
    onResize.mockClear();

    fireTouchPointer(frame, "pointerdown", {
      pointerId: 12,
      clientX: 24,
      clientY: 24,
    });
    textarea!.focus();
    await waitFor(() => expect(document.activeElement).toBe(textarea));

    const pasteEvent = new Event("paste", { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(pasteEvent, "clipboardData", {
      configurable: true,
      value: {
        getData: (type: string) => (type === "text" ? "passive-paste" : ""),
      },
    });
    textarea!.dispatchEvent(pasteEvent);
    await waitFor(() => expect(onInput).toHaveBeenCalledWith("passive-paste"));
    expect(onResize).not.toHaveBeenCalled();

    onInput.mockClear();
    textarea!.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    const candidateSpaceEvent = new InputEvent("beforeinput", {
      bubbles: true,
      cancelable: true,
      inputType: "insertText",
      data: " ",
    });
    Object.defineProperty(candidateSpaceEvent, "isComposing", {
      configurable: true,
      value: true,
    });
    textarea!.dispatchEvent(candidateSpaceEvent);
    textarea!.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "你" }));
    textarea!.value = "你";
    fireEvent.input(textarea!);

    expect(candidateSpaceEvent.defaultPrevented).toBe(false);
    await waitFor(() => expect(onInput).toHaveBeenCalledWith("你"));
    expect(onResize).not.toHaveBeenCalled();
  });

  it("snapshot 后推进 base seq，连续 output 正常写入并推进 terminal_seq", async () => {
    const onTerminalSeqRendered = vi.fn();

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: new TextEncoder().encode("tail\n"), terminalSeq: 11 },
      ],
      { onTerminalSeqRendered },
    );

    await screen.findByText("snapshot", { exact: false });
    const host = terminalHost();
    await waitFor(() => expect(host.dataset.buffer).toContain("tail"));
    expect(onTerminalSeqRendered.mock.calls).toEqual([[10], [11]]);
  });

  it("snapshot 按自身尺寸重绘，并在 tail resize 后再写后续 output", async () => {
    const encoder = new TextEncoder();
    const onTerminalSeqRendered = vi.fn();
    const snapshotSize = { rows: 32, cols: 120, pixel_width: 0, pixel_height: 0 };
    const tailSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: encoder.encode("wide-snapshot\n"), baseSeq: 0, size: snapshotSize },
        { kind: "output", bytes: encoder.encode("before-resize\n"), terminalSeq: 1 },
        { kind: "resize", terminalSeq: 2, size: tailSize },
        { kind: "output", bytes: encoder.encode("after-resize\n"), terminalSeq: 3 },
      ],
      { onTerminalSeqRendered },
    );

    const host = terminalHost();
    await waitFor(() => expect(host.dataset.buffer).toContain("after-resize"));
    const operations = (globalThis as {
      __TERMD_TEST_TERMINAL_STATS__?: {
        operations: Array<{ op: string; cols?: number; rows?: number }>;
      };
    }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
    const firstWriteIndex = operations.findIndex((operation) => operation.op === "write");
    const snapshotResizeIndex = operations.findIndex(
      (operation) => operation.op === "resize" && operation.cols === 120 && operation.rows === 32,
    );
    const tailResizeIndex = operations.findIndex(
      (operation) => operation.op === "resize" && operation.cols === 80 && operation.rows === 24,
    );
    const writeAfterTailResizeIndex = operations.findIndex(
      (operation, index) => operation.op === "write" && index > tailResizeIndex,
    );

    expect(snapshotResizeIndex).toBeGreaterThanOrEqual(0);
    expect(snapshotResizeIndex).toBeLessThan(firstWriteIndex);
    expect(tailResizeIndex).toBeGreaterThan(firstWriteIndex);
    expect(writeAfterTailResizeIndex).toBeGreaterThan(tailResizeIndex);
    expect(onTerminalSeqRendered.mock.calls).toEqual([[0], [1], [2], [3]]);
  });

  it("output terminal_seq 不连续时触发 resync，且不推进到跳号 frame", async () => {
    const onTerminalResync = vi.fn();
    const onTerminalSeqRendered = vi.fn();

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: new TextEncoder().encode("gap\n"), terminalSeq: 13 },
      ],
      { onTerminalResync, onTerminalSeqRendered },
    );

    await screen.findByText("snapshot", { exact: false });
    await waitFor(() => expect(onTerminalResync).toHaveBeenCalledWith(10));
    expect(onTerminalSeqRendered.mock.calls).toEqual([[10]]);
  });

  it("single huge output frame is chunked instead of triggering high-water resync loop", async () => {
    const onTerminalResync = vi.fn();
    const onTerminalSeqRendered = vi.fn();
    const huge = new Uint8Array(4 * 1024 * 1024 + 1);

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: huge, terminalSeq: 11 },
      ],
      { onTerminalResync, onTerminalSeqRendered },
    );

    await waitFor(
      () => {
        const stats = (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writtenBytes: number } }).__TERMD_TEST_TERMINAL_STATS__;
        expect(stats?.writtenBytes ?? 0).toBeGreaterThan(4 * 1024 * 1024);
      },
      { timeout: 5000 },
    );
    expect(onTerminalResync).not.toHaveBeenCalled();
    await waitFor(() => expect(onTerminalSeqRendered).toHaveBeenCalledWith(11), { timeout: 5000 });
  });

  it("accumulated relay burst backlog below high-water keeps draining instead of reconnecting", async () => {
    const onTerminalResync = vi.fn();
    const onTerminalSeqRendered = vi.fn();
    const backlog = Array.from({ length: 16 }, (_, index) => ({
      kind: "output" as const,
      bytes: new Uint8Array(1024 * 1024),
      terminalSeq: index + 11,
    }));

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
        ...backlog,
      ],
      { onTerminalResync, onTerminalSeqRendered },
    );

    await waitFor(() => expect(onTerminalSeqRendered).toHaveBeenCalledWith(26), { timeout: 10_000 });
    expect(onTerminalResync).not.toHaveBeenCalled();
  });

  it("extreme accumulated pending output bytes above high-water still triggers resync", async () => {
    const onTerminalResync = vi.fn();
    const backlog = Array.from({ length: 17 }, (_, index) => ({
      kind: "output" as const,
      bytes: new Uint8Array(4 * 1024 * 1024),
      terminalSeq: index + 11,
    }));

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
        ...backlog,
      ],
      { onTerminalResync },
    );

    await waitFor(() => expect(onTerminalResync).toHaveBeenCalledWith(undefined));
  });

  it("连续 output frame 合并成批量 xterm write，但仍逐帧确认 terminal_seq", async () => {
    const onTerminalSeqRendered = vi.fn();
    const items: TerminalOutputItem[] = [
      { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ...Array.from({ length: 32 }, (_, index) => ({
        kind: "output" as const,
        bytes: new TextEncoder().encode(`line-${index + 1}\n`),
        terminalSeq: index + 1,
      })),
    ];

    renderTerminalPaneWithOutput(items, { onTerminalSeqRendered });

    const host = terminalHost();
    await waitFor(() => expect(host.dataset.buffer).toContain("line-32"));
    const stats = (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number } })
      .__TERMD_TEST_TERMINAL_STATS__;
    expect(stats?.writes ?? 0).toBeLessThan(10);
    expect(onTerminalSeqRendered.mock.calls.at(-1)).toEqual([32]);
    expect(onTerminalSeqRendered).toHaveBeenCalledTimes(33);
  });

  it("live output 停止后也刷新最后一笔写入，不需要等待下一次输入", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      const encoder = new TextEncoder();
      const onTerminalSeqRendered = vi.fn();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: encoder.encode("snapshot\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onTerminalSeqRendered={onTerminalSeqRendered}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const host = terminalHost();
      expect(host.dataset.buffer).toContain("snapshot");

      queue = [
        { kind: "output", bytes: encoder.encode("final-tail\n"), terminalSeq: 1 },
      ];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      // 中文注释：真实 xterm 某些渲染时序下，最后一笔 write 已解析但尚未 repaint。
      // 如果 TerminalPane 只在下一次输入/resize 时 refresh，就会表现为“按一下键才出现尾巴”。
      expect(host.dataset.buffer).toContain("final-tail");
      expect(onTerminalSeqRendered.mock.calls).toEqual([[0], [1]]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("进入 session 的异步 snapshot 写入完成后会贴到底部", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
        .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__ = true;
      const snapshot = Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      // 中文注释：刚 attach 时早期 resize/stabilize 可能已经在 snapshot 写完前执行过。
      // write callback 后仍必须再次贴底，否则用户会停在 snapshot 顶部附近。
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("点击终端空白处不会把已上滚的 scrollback 强制贴底", async () => {
    vi.useFakeTimers();
    try {
      const snapshot = Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      act(() => {
        xterm?.scrollToLine(0);
      });
      expect(xterm?.viewportY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!, { clientX: 24, clientY: 24, button: 0 });
      fireEvent.click(frame!, { clientX: 24, clientY: 24, button: 0 });
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：点击空白处只应该聚焦终端；如果用户正在看历史，不能强行滚回最新输出。
      expect(xterm?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("拖拽选区会驱动 xterm selection 并触发复制", async () => {
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode("line-001\nline-002\nline-003\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      await waitFor(() => expect(terminalHost().dataset.buffer).toContain("line-003"));
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      expect(canvas).not.toBeNull();

      fireEvent.mouseDown(canvas!, { clientX: 20, clientY: 20, button: 0 });
      fireEvent.mouseMove(window, { clientX: 180, clientY: 20 });
      fireEvent.mouseUp(window, { clientX: 180, clientY: 20 });

      await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));
      expect(terminalHost().dataset.termdSelection).not.toBe("");
      await waitFor(() => expect(screen.getByRole("status")).toBeVisible());
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("拖拽命中 canvas 包装层时仍会启动自定义选区复制", async () => {
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode("line-001\nline-002\nline-003\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      await waitFor(() => expect(terminalHost().dataset.buffer).toContain("line-003"));
      fireEvent.mouseDown(terminalHost(), { clientX: 20, clientY: 20, button: 0 });
      fireEvent.mouseMove(window, { clientX: 180, clientY: 20 });
      fireEvent.mouseUp(window, { clientX: 180, clientY: 20 });

      await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));
      expect(terminalHost().dataset.termdSelection).not.toBe("");
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("移动端长按拖动会选择局部字符范围并支持跨行", async () => {
    vi.useFakeTimers();
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      const queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new TextEncoder().encode("abcdefghi\nABCDEFGHI\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      expect(terminalHost().dataset.buffer).toContain("ABCDEFGHI");
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      expect(canvas).not.toBeNull();

      fireTouchPointer(canvas!, "pointerdown", {
        pointerId: 31,
        clientX: 20,
        clientY: 5,
      });
      act(() => {
        vi.advanceTimersByTime(700);
      });
      fireTouchPointer(canvas!, "pointermove", {
        pointerId: 31,
        clientX: 65,
        clientY: 15,
      });
      fireTouchPointer(canvas!, "pointerup", {
        pointerId: 31,
        clientX: 65,
        clientY: 15,
      });

      expect(terminalHost().dataset.termdHasSelection).toBe("true");
      // 中文注释：移动端长按只落下选区起点，后续拖动按 cell 扩展；不能再退回“选整行”。
      expect(terminalHost().dataset.termdSelection).toBe("cdefghi\nABCDEFG");
      expect(terminalHost().dataset.termdSelection).not.toContain("abcdefghi\nABCDEFGHI");
    } finally {
      rectSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("移动端长按选择不会弹出键盘，后续单独点击才聚焦输入", async () => {
    vi.useFakeTimers();
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      const queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new TextEncoder().encode("mobile-focus-select\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(canvas).not.toBeNull();
      expect(terminalInput).not.toBeNull();
      const focusSpy = vi.spyOn(terminalInput!, "focus");

      try {
        terminalInput!.blur();
        focusSpy.mockClear();
        fireTouchPointer(canvas!, "pointerdown", {
          pointerId: 32,
          clientX: 20,
          clientY: 5,
        });
        act(() => {
          vi.advanceTimersByTime(700);
        });
        fireTouchPointer(canvas!, "pointermove", {
          pointerId: 32,
          clientX: 55,
          clientY: 5,
        });
        fireTouchPointer(canvas!, "pointerup", {
          pointerId: 32,
          clientX: 55,
          clientY: 5,
        });
        // 中文注释：移动浏览器会在 touch 后补发 mouse/click；这仍属于同一轮长按选择，
        // 不能被解释成“用户点击输入”而弹出软键盘。
        fireEvent.mouseDown(canvas!, { clientX: 55, clientY: 5, button: 0 });
        fireEvent.click(canvas!, { clientX: 55, clientY: 5, button: 0 });

        expect(focusSpy).not.toHaveBeenCalled();
        expect(document.activeElement).not.toBe(terminalInput);

        act(() => {
          vi.advanceTimersByTime(900);
        });
        fireTouchPointer(canvas!, "pointerdown", {
          pointerId: 33,
          clientX: 20,
          clientY: 5,
        });
        fireTouchPointer(canvas!, "pointerup", {
          pointerId: 33,
          clientX: 20,
          clientY: 5,
        });
        fireEvent.click(canvas!, { clientX: 20, clientY: 5, button: 0 });

        expect(focusSpy).toHaveBeenCalled();
        expect(document.activeElement).toBe(terminalInput);
      } finally {
        focusSpy.mockRestore();
      }
    } finally {
      rectSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("移动端选区提供显式复制按钮且不会弹出键盘", async () => {
    vi.useFakeTimers();
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      const queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new TextEncoder().encode("abcdefghi\nABCDEFGHI\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(canvas).not.toBeNull();
      expect(terminalInput).not.toBeNull();
      const focusSpy = vi.spyOn(terminalInput!, "focus");
      const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;

      try {
        terminalInput!.blur();
        focusSpy.mockClear();
        clipboardWriteTextMock.mockClear();
        fireTouchPointer(canvas!, "pointerdown", {
          pointerId: 34,
          clientX: 20,
          clientY: 5,
        });
        act(() => {
          vi.advanceTimersByTime(700);
        });
        fireTouchPointer(canvas!, "pointermove", {
          pointerId: 34,
          clientX: 65,
          clientY: 15,
        });
        fireTouchPointer(canvas!, "pointerup", {
          pointerId: 34,
          clientX: 65,
          clientY: 15,
        });

        expect(terminalHost().dataset.termdSelection).toBe("cdefghi\nABCDEFG");
        const copySelectionButton = screen.getByRole("button", { name: "Copy selection" });
        clipboardWriteTextMock.mockClear();

        fireEvent.pointerDown(copySelectionButton);
        fireEvent.click(copySelectionButton);
        await act(async () => {
          await Promise.resolve();
        });

        expect(clipboardWriteTextMock).toHaveBeenCalledWith("cdefghi\nABCDEFG");
        expect(focusSpy).not.toHaveBeenCalled();
        expect(document.activeElement).not.toBe(terminalInput);
      } finally {
        focusSpy.mockRestore();
      }
    } finally {
      rectSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("没有 canvas 时点击 host 仍会命中 xterm 可见表面并聚焦隐藏 textarea", async () => {
    const surfaceRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("line-001\nline-002\nline-003\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("line-003"));
    const xtermScreen = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm-screen");
    const canvas = screen.getByTestId("terminal-pane").querySelector("canvas");
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(xtermScreen).not.toBeNull();
    expect(canvas).not.toBeNull();
    expect(terminalInput).not.toBeNull();
    canvas?.remove();
    const rectSpy = vi.spyOn(xtermScreen!, "getBoundingClientRect").mockReturnValue(surfaceRect);
    try {
      terminalInput!.blur();
      fireEvent.mouseDown(terminalHost(), { clientX: 20, clientY: 20, button: 0 });
      fireEvent.click(terminalHost(), { clientX: 20, clientY: 20, button: 0 });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("拖拽选区结束后的 trailing click 不会把焦点抢回隐藏 textarea", async () => {
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode("line-001\nline-002\nline-003\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      await waitFor(() => expect(terminalHost().dataset.buffer).toContain("line-003"));
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(canvas).not.toBeNull();
      expect(terminalInput).not.toBeNull();
      terminalInput!.blur();

      fireEvent.mouseDown(canvas!, { clientX: 20, clientY: 20, button: 0 });
      fireEvent.mouseMove(window, { clientX: 180, clientY: 20 });
      fireEvent.mouseUp(window, { clientX: 180, clientY: 20 });
      fireEvent.click(canvas!, { clientX: 180, clientY: 20 });

      await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));
      expect(document.activeElement).not.toBe(terminalInput);
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("xterm 画布右边缘点击会继续聚焦隐藏 textarea", async () => {
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      const snapshot = Array.from({ length: 80 }, (_, index) => `scrollbar-line-${index}\n`).join("");
      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      await waitFor(() => expect(Number(terminalHost().dataset.termdScrollbackLength ?? "0")).toBeGreaterThan(0));
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(canvas).not.toBeNull();
      expect(terminalInput).not.toBeNull();
      terminalInput!.blur();

      fireEvent.mouseDown(canvas!, { clientX: canvasRect.right - 2, clientY: 20, button: 0 });
      fireEvent.click(canvas!, { clientX: canvasRect.right - 2, clientY: 20 });

      expect(document.activeElement).toBe(terminalInput);
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("自定义拖拽复制不依赖 xterm 返回的 selectionPosition", async () => {
    const canvasRect = {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 800,
      bottom: 240,
      width: 800,
      height: 240,
      toJSON() {
        return this;
      },
    } as DOMRect;
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue(canvasRect);
    try {
      (globalThis as {
        __TERMD_TEST_FORCE_SELECTION_POSITION__?: { start: { x: number; y: number }; end: { x: number; y: number } };
      }).__TERMD_TEST_FORCE_SELECTION_POSITION__ = {
        start: { x: 0, y: 0 },
        end: { x: 10, y: 0 },
      };
      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode("first-line\nsecond-line\nthird-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      await waitFor(() => expect(terminalHost().dataset.buffer).toContain("third-line"));
      const canvas = screen.getByTestId("terminal-pane").querySelector<HTMLCanvasElement>("canvas");
      expect(canvas).not.toBeNull();
      const rowHeight = canvasRect.height / DEFAULT_TERMINAL_SIZE.rows;
      const y = canvasRect.top + rowHeight * 1.5;

      fireEvent.mouseDown(canvas!, { clientX: 2, clientY: y, button: 0 });
      fireEvent.mouseMove(window, { clientX: 180, clientY: y });
      fireEvent.mouseUp(window, { clientX: 180, clientY: y });

      await waitFor(() => expect(terminalHost().dataset.termdSelectionCopy).toContain("second-line"));
      expect(terminalHost().dataset.termdSelectionCopy).not.toContain("first-line");
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("终端有 xterm 选区时 Ctrl+C 会优先走浏览器原生 copy 事务", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-shortcut-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-shortcut-line"));
    const xterm = (globalThis as {
      __TERMD_TEST_TERMINAL__?: { select: (text: string) => void };
    }).__TERMD_TEST_TERMINAL__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const setData = vi.fn();
    const originalExecCommand = document.execCommand;
    const execCommandMock = vi.fn((command: string) => {
      if (command !== "copy") {
        return false;
      }
      const copyEvent = new Event("copy", { bubbles: true, cancelable: true }) as ClipboardEvent;
      Object.defineProperty(copyEvent, "clipboardData", {
        configurable: true,
        value: {
          setData,
        },
      });
      document.dispatchEvent(copyEvent);
      return true;
    });
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: execCommandMock,
    });

    try {
      xterm?.select("copy-shortcut-line");
      await waitFor(() => expect(terminalHost().dataset.termdSelection).toContain("copy-shortcut-line"));
      clipboardWriteTextMock.mockClear();

      const copyShortcut = new KeyboardEvent("keydown", {
        key: "c",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(copyShortcut);

      expect(copyShortcut.defaultPrevented).toBe(true);
      expect(execCommandMock).toHaveBeenCalledWith("copy");
      expect(setData).toHaveBeenCalledWith("text/plain", "copy-shortcut-line");
      expect(clipboardWriteTextMock).not.toHaveBeenCalled();
      expect(terminalHost().dataset.termdSelectionCopy).toBe("copy-shortcut-line");
    } finally {
      Object.defineProperty(document, "execCommand", {
        configurable: true,
        value: originalExecCommand,
      });
    }
  });

  it("终端原生 copy 事务不可用时 Ctrl+C 会回退到程序化复制", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-shortcut-fallback-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-shortcut-fallback-line"));
    const xterm = (globalThis as {
      __TERMD_TEST_TERMINAL__?: { select: (text: string) => void };
    }).__TERMD_TEST_TERMINAL__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const originalExecCommand = document.execCommand;
    const execCommandMock = vi.fn(() => false);
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: execCommandMock,
    });

    try {
      xterm?.select("copy-shortcut-fallback-line");
      await waitFor(() => expect(terminalHost().dataset.termdSelection).toContain("copy-shortcut-fallback-line"));
      clipboardWriteTextMock.mockClear();

      const copyShortcut = new KeyboardEvent("keydown", {
        key: "c",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(copyShortcut);

      expect(copyShortcut.defaultPrevented).toBe(true);
      expect(execCommandMock).toHaveBeenCalledWith("copy");
      await waitFor(() => expect(clipboardWriteTextMock).toHaveBeenCalledWith("copy-shortcut-fallback-line"));
      expect(terminalHost().dataset.termdSelectionCopy).toBe("copy-shortcut-fallback-line");
    } finally {
      Object.defineProperty(document, "execCommand", {
        configurable: true,
        value: originalExecCommand,
      });
    }
  });

  it("浏览器 copy 事件会把 xterm 选区写入 clipboardData", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-event-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-event-line"));
    const xterm = (globalThis as {
      __TERMD_TEST_TERMINAL__?: { select: (text: string) => void };
    }).__TERMD_TEST_TERMINAL__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const setData = vi.fn();

    xterm?.select("copy-event-line");
    await waitFor(() => expect(terminalHost().dataset.termdSelection).toContain("copy-event-line"));
    clipboardWriteTextMock.mockClear();

    const copyEvent = new Event("copy", { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(copyEvent, "clipboardData", {
      configurable: true,
      value: {
        setData,
      },
    });
    document.dispatchEvent(copyEvent);

    expect(copyEvent.defaultPrevented).toBe(true);
    expect(setData).toHaveBeenCalledWith("text/plain", "copy-event-line");
    expect(clipboardWriteTextMock).not.toHaveBeenCalled();
    expect(terminalHost().dataset.termdSelectionCopy).toBe("copy-event-line");
  });

  it("点击终端外后，旧的 xterm 选区不会继续劫持 Ctrl+C", async () => {
    const outputItems: TerminalOutputItem[] = [
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-outside-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ];
    const takeOutput = vi.fn(() => outputItems);
    const registerOutputDrain = vi.fn((drain: () => void) => {
      drain();
      return () => undefined;
    });
    render(
      <div>
        <div data-testid="outside-copy-target">
          outside
        </div>
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />
      </div>,
    );

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-outside-line"));
    const xterm = (globalThis as {
      __TERMD_TEST_TERMINAL__?: { select: (text: string) => void };
    }).__TERMD_TEST_TERMINAL__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;

    xterm?.select("copy-outside-line");
    await waitFor(() => expect(terminalHost().dataset.termdSelection).toContain("copy-outside-line"));
    clipboardWriteTextMock.mockClear();

    screen.getByRole("textbox", { name: "Terminal input" }).focus();
    fireEvent.mouseDown(screen.getByTestId("outside-copy-target"));
    const copyShortcut = new KeyboardEvent("keydown", {
      key: "c",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    document.dispatchEvent(copyShortcut);

    expect(copyShortcut.defaultPrevented).toBe(false);
    expect(clipboardWriteTextMock).not.toHaveBeenCalled();
  });

  it("已有 xterm 选区时，点击终端内其他位置会取消选区", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("selection-clear-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("selection-clear-line"));
    const xterm = (globalThis as {
      __TERMD_TEST_TERMINAL__?: { select: (text: string) => void };
    }).__TERMD_TEST_TERMINAL__;
    const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    expect(frame).not.toBeNull();

    xterm?.select("selection-clear-line");
    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));

    fireEvent.mouseDown(frame!, { clientX: 20, clientY: 20, button: 0 });
    fireEvent.click(frame!, { clientX: 20, clientY: 20, button: 0 });

    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("false"));
    expect(terminalHost().dataset.termdSelection).toBe("");
  });

  it("debug bridge 持有的 xterm 选区也会在终端内点击后清掉", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("selection-manager-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("selection-manager-line"));
    const debugTerminal = (window as typeof window & {
      __TERMD_DEBUG_TERMINAL__?: {
        selectViewportRange: (
          start: { col: number; row: number },
          end: { col: number; row: number },
        ) => string | undefined;
      };
    }).__TERMD_DEBUG_TERMINAL__;
    expect(debugTerminal).toBeDefined();
    const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    expect(frame).not.toBeNull();

    const selection = debugTerminal?.selectViewportRange({ col: 0, row: 0 }, { col: 21, row: 0 });
    expect(selection).toContain("selection-manager-line");
    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));

    fireEvent.mouseDown(frame!, { clientX: 20, clientY: 20, button: 0 });
    fireEvent.click(frame!, { clientX: 20, clientY: 20, button: 0 });

    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("false"));
    expect(terminalHost().dataset.termdSelection).toBe("");
  });

  it("终端聚焦时 Shift+Insert 会从剪贴板读取并发送粘贴文本", async () => {
    const onInput = vi.fn();
    const clipboardDescriptor = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const readTextSpy = vi.fn<() => Promise<string>>(() => Promise.resolve("shift-insert-paste"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        ...navigator.clipboard,
        writeText: clipboardWriteTextMock,
        readText: readTextSpy,
      },
    });

    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={onInput}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
      });

      const pasteShortcut = new KeyboardEvent("keydown", {
        key: "Insert",
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(pasteShortcut);

      expect(readTextSpy).not.toHaveBeenCalled();
      await flushPasteShortcutFallbackTick();
      await waitFor(() => expect(onInput).toHaveBeenCalledWith("shift-insert-paste"));
      expect(readTextSpy).toHaveBeenCalledTimes(1);
      expect(pasteShortcut.defaultPrevented).toBe(false);
    } finally {
      if (clipboardDescriptor) {
        Object.defineProperty(navigator, "clipboard", clipboardDescriptor);
      }
    }
  });

  it("Shift+Insert 读剪贴板失败时仍保留原生 paste 路径", async () => {
    const onInput = vi.fn();
    const clipboardDescriptor = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const readTextSpy = vi.fn<() => Promise<string>>(() => Promise.reject(new Error("denied")));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        ...navigator.clipboard,
        writeText: clipboardWriteTextMock,
        readText: readTextSpy,
      },
    });

    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={onInput}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      act(() => {
        screen.getByRole("textbox", { name: "Terminal input" }).focus();
      });

      const pasteShortcut = new KeyboardEvent("keydown", {
        key: "Insert",
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(pasteShortcut);

      expect(readTextSpy).not.toHaveBeenCalled();
      expect(pasteShortcut.defaultPrevented).toBe(false);

      const pasteEvent = new Event("paste", { bubbles: true, cancelable: true }) as ClipboardEvent;
      Object.defineProperty(pasteEvent, "clipboardData", {
        configurable: true,
        value: {
          getData: (type: string) => (type === "text" ? "native-fallback-paste" : ""),
        },
      });
      terminalInput!.dispatchEvent(pasteEvent);

      expect(pasteEvent.defaultPrevented).toBe(true);
      await waitFor(() => expect(onInput).toHaveBeenCalledWith("native-fallback-paste"));
      await flushPasteShortcutFallbackTick();
      expect(readTextSpy).not.toHaveBeenCalled();
    } finally {
      if (clipboardDescriptor) {
        Object.defineProperty(navigator, "clipboard", clipboardDescriptor);
      }
    }
  });

  it("Shift+Insert 只在原生链路未消费后才启动 readText 兜底，并避免重复发送", async () => {
    const onInput = vi.fn();
    const clipboardDescriptor = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const readTextDeferred = deferred<string>();
    const readTextSpy = vi.fn<() => Promise<string>>(() => readTextDeferred.promise);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        ...navigator.clipboard,
        writeText: clipboardWriteTextMock,
        readText: readTextSpy,
      },
    });

    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={onInput}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      act(() => {
        screen.getByRole("textbox", { name: "Terminal input" }).focus();
      });

      const pasteShortcut = new KeyboardEvent("keydown", {
        key: "Insert",
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(pasteShortcut);
      expect(readTextSpy).not.toHaveBeenCalled();
      await flushPasteShortcutFallbackTick();
      expect(readTextSpy).toHaveBeenCalledTimes(1);

      const pasteEvent = new Event("paste", { bubbles: true, cancelable: true }) as ClipboardEvent;
      Object.defineProperty(pasteEvent, "clipboardData", {
        configurable: true,
        value: {
          getData: (type: string) => (type === "text" ? "dedup-paste" : ""),
        },
      });
      terminalInput!.dispatchEvent(pasteEvent);

      await waitFor(() => expect(onInput).toHaveBeenCalledWith("dedup-paste"));
      expect(onInput).toHaveBeenCalledTimes(1);

      await act(async () => {
        readTextDeferred.resolve("dedup-paste");
        await Promise.resolve();
      });

      expect(onInput).toHaveBeenCalledTimes(1);
    } finally {
      if (clipboardDescriptor) {
        Object.defineProperty(navigator, "clipboard", clipboardDescriptor);
      }
    }
  });

  it("Shift+Insert 不会在终端失焦后截走其他输入控件的粘贴", async () => {
    const onInput = vi.fn();
    const clipboardDescriptor = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const readTextSpy = vi.fn<() => Promise<string>>(() => Promise.resolve("should-not-paste"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        ...navigator.clipboard,
        writeText: clipboardWriteTextMock,
        readText: readTextSpy,
      },
    });

    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      render(
        <div>
          <input data-testid="outside-input" aria-label="outside input" />
          <TerminalPane
            attached
            sessionSize={DEFAULT_TERMINAL_SIZE}
            outputResetVersion={0}
            takeOutput={takeOutput}
            registerOutputDrain={registerOutputDrain}
            onInput={onInput}
            onResize={vi.fn()}
            onCursorChange={vi.fn()}
          />
        </div>,
      );

      act(() => {
        screen.getByRole("textbox", { name: "Terminal input" }).focus();
      });
      act(() => {
        screen.getByTestId("outside-input").focus();
      });

      const pasteShortcut = new KeyboardEvent("keydown", {
        key: "Insert",
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(pasteShortcut);

      await flushPasteShortcutFallbackTick();
      expect(readTextSpy).not.toHaveBeenCalled();
      expect(onInput).not.toHaveBeenCalled();
      expect(pasteShortcut.defaultPrevented).toBe(false);
    } finally {
      if (clipboardDescriptor) {
        Object.defineProperty(navigator, "clipboard", clipboardDescriptor);
      }
    }
  });

  it("远端 sessionSize 变化不会让已聚焦客户端回写本地尺寸", async () => {
    vi.useFakeTimers();
    try {
      const localSize = { rows: 37, cols: 89, pixel_width: 716, pixel_height: 668 };
      const remoteSize = { rows: 33, cols: 53, pixel_width: 434, pixel_height: 600 };
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
        .__TERMD_TEST_FIT_DIMENSIONS__ = { rows: localSize.rows, cols: localSize.cols };

      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={localSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!, { clientX: 24, clientY: 24, button: 0 });
      fireEvent.click(frame!, { clientX: 24, clientY: 24, button: 0 });
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      expect(onResize).not.toHaveBeenCalled();

      rerender(
        <TerminalPane
          attached
          sessionSize={remoteSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：另一客户端 resize 后会把权威 sessionSize 推给本端。
      // 当前客户端不能立刻把自己的本地尺寸写回 daemon；同时也不应在失焦/回焦链路里
      // 强制把本地 xterm 缩回远端尺寸，避免肉眼看到一次远端网格闪回。
      expect(onResize).not.toHaveBeenCalled();
      const operations = (globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
      expect(operations).not.toContainEqual({ op: "resize", cols: remoteSize.cols, rows: remoteSize.rows });
    } finally {
      restoreDocumentVisibility();
      vi.useRealTimers();
    }
  });

  it("未聚焦客户端在 sessionSize 变化后会跟随远端权威 grid", async () => {
    vi.useFakeTimers();
    try {
      const initialSize = { rows: 31, cols: 101, pixel_width: 808, pixel_height: 496 };
      const remoteSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
        .__TERMD_TEST_FIT_DIMENSIONS__ = { rows: initialSize.rows, cols: initialSize.cols };

      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={initialSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      act(() => {
        window.dispatchEvent(new Event("blur"));
        vi.advanceTimersByTime(240);
      });

      rerender(
        <TerminalPane
          attached
          sessionSize={remoteSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      const operations = (globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
      // 中文注释：这里是 daemon/supervisor 已确认的 sessionSize 变化，不是本地 blur/layout 抖动。
      // 未聚焦客户端必须被动切回权威 grid，避免后续输出按旧列宽继续错位。
      expect(operations).toContainEqual({ op: "resize", cols: remoteSize.cols, rows: remoteSize.rows });
    } finally {
      restoreDocumentVisibility();
      vi.useRealTimers();
    }
  });

  it("snapshot 渲染完成后已聚焦客户端会回写本地尺寸", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      const localSize = { rows: 37, cols: 89, pixel_width: 716, pixel_height: 668 };
      const snapshotSize = { rows: 33, cols: 53, pixel_width: 434, pixel_height: 600 };
      const onResize = vi.fn();
      const snapshot = new TextEncoder().encode("remote-snapshot\n");
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
        .__TERMD_TEST_FIT_DIMENSIONS__ = { rows: localSize.rows, cols: localSize.cols };

      render(
        <TerminalPane
          attached
          sessionSize={localSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      expect(onResize).not.toHaveBeenCalled();

      queue = [{ kind: "snapshot", bytes: snapshot, baseSeq: 10, size: snapshotSize }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 32);
      });

      expect(terminalHost().dataset.buffer)
        .toContain("remote-snapshot");
      // 中文注释：reload/重连 snapshot 可能携带旧尺寸；用户已经重新聚焦终端时，
      // snapshot 完成后必须把当前浏览器布局尺寸写回 daemon/supervisor。
      expect(onResize).toHaveBeenCalledWith({
        rows: localSize.rows,
        cols: localSize.cols,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("位于底部时新输出继续跟随 PTY 底部滚动", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 40 }, (_, index) => `snapshot-line-${index}\n`).join("")),
          baseSeq: 0,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      queue = [{ kind: "output", bytes: encoder.encode("tail\n"), terminalSeq: 1 }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("用户上滚后，已经排队的延迟贴底帧不会再把视口拉回底部", async () => {
    vi.useFakeTimers();
    try {
      const snapshot = Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      act(() => {
        xterm?.scrollToLine(0);
      });
      expect(xterm?.viewportY()).toBe(0);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 10);
      });

      // 中文注释：用户已经开始查看历史后，后续排队的贴底帧只能在“当前仍在底部”时生效。
      expect(xterm?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("位于底部时纯 resize frame 也会重新贴到新的 PTY 底部", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
        .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__ = true;
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("")),
          baseSeq: 0,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
      const baseYBeforeResize = xterm?.baseY() ?? 0;

      queue = [{ kind: "resize", terminalSeq: 1, size: { rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 } }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：resize frame 没有 output bytes；即使没有后续 write callback，也必须完成贴底。
      expect(xterm?.baseY()).toBeGreaterThan(baseYBeforeResize);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("不在底部时新输出不打断用户查看历史", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 40 }, (_, index) => `snapshot-line-${index}\n`).join("")),
          baseSeq: 0,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);

      act(() => {
        xterm?.scrollToLine(0);
      });
      expect(xterm?.viewportY()).toBe(0);

      queue = [{ kind: "output", bytes: encoder.encode("tail\n"), terminalSeq: 1 }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：用户已经上滚时，PTY 继续输出只能更新 buffer，不能把视口抢回底部。
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("桌面终端有 scrollback 时不再渲染 termd 自定义拖动滚动条", async () => {
    vi.useFakeTimers();
    try {
      const snapshot = Array.from({ length: 80 }, (_, index) => `scrollbar-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      // 中文注释：scrollback 仍由 xterm 自己管理；termd 不再额外渲染 DOM 拖动条。
      expect(document.querySelector(".terminal-scroll-track")).toBeNull();
      expect(document.querySelector(".terminal-scroll-thumb")).toBeNull();
      expect(screen.queryByRole("button", { name: "Terminal scroll" })).toBeNull();

      act(() => {
        xterm?.scrollToLine(0);
      });
      expect(xterm?.viewportY()).toBe(0);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 2);
      });
      expect(document.querySelector(".terminal-scroll-track")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("全屏程序重绘导致 xterm 无 scrollback 时会在稳定窗口后主动请求一次 snapshot resync", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const largeFullscreenRedraw = "x".repeat(9 * 1024);

      renderTerminalPaneWithOutput(
        [
          { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
          { kind: "output", bytes: new TextEncoder().encode(largeFullscreenRedraw), terminalSeq: 1 },
        ],
        { onTerminalResync },
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBe(0);
      // 中文注释：自动补历史只在输出稳定一小段时间后触发，避免 relay 恢复期的
      // 暂态输出被误判成全屏程序重绘。
      expect(onTerminalResync).toHaveBeenCalledTimes(0);

      act(() => {
        vi.advanceTimersByTime(1000);
        vi.advanceTimersByTime(animationFrameMs * 2);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenCalledWith(undefined);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 30);
      });
      // 中文注释：resync 请求发出后，在 snapshot 回来之前不能每帧重复重连。
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("自动 scrollback resync 等待稳定窗口期间，用户上滚仍会立即请求历史 snapshot", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const largeFullscreenRedraw = "x".repeat(9 * 1024);

      renderTerminalPaneWithOutput(
        [
          { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
          { kind: "output", bytes: new TextEncoder().encode(largeFullscreenRedraw), terminalSeq: 1 },
        ],
        { onTerminalResync },
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -900 });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：稳定窗口只约束自动路径。用户明确向上滚时，仍要立刻回源拉历史。
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenCalledWith(undefined, { revealHistory: true });
    } finally {
      vi.useRealTimers();
    }
  });

  it("用户向上滚动但 xterm 无本地 scrollback 时会请求 supervisor snapshot history", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const fullscreenRedraw = "x".repeat(4 * 1024);

      renderTerminalPaneWithOutput(
        [
          { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
          { kind: "output", bytes: new TextEncoder().encode(fullscreenRedraw), terminalSeq: 1 },
        ],
        { onTerminalResync },
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(0);

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      const wheelEvent = new WheelEvent("wheel", { deltaY: -900, bubbles: true, cancelable: true });
      frame!.dispatchEvent(wheelEvent);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：真实 supervisor attach 输出可能不让 xterm 本地形成 scrollback；
      // 用户一旦向上滚，就应主动拉 daemon/supervisor snapshot，而不是等输出超过 8KiB。
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenCalledWith(undefined, { revealHistory: true });
      expect(wheelEvent.defaultPrevented).toBe(true);

      fireEvent.wheel(frame!, { deltaY: -900 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("用户滚轮会把已在路上的自动 scrollback snapshot 升级成 reveal history", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const fullscreenRedraw = "x".repeat(9 * 1024);

      renderTerminalPaneWithOutput(
        [
          { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
          { kind: "output", bytes: new TextEncoder().encode(fullscreenRedraw), terminalSeq: 1 },
        ],
        { onTerminalResync },
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(0);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1000 + animationFrameMs * 2);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenNthCalledWith(1, undefined);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -900 });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：自动预取可能已经启动 full snapshot；用户随后上滚时必须把
      // 已在路上的 snapshot 升级成 reveal，否则真实浏览器会拿到历史但仍停在底部。
      expect(onTerminalResync).toHaveBeenCalledTimes(2);
      expect(onTerminalResync).toHaveBeenNthCalledWith(2, undefined, { revealHistory: true });

      fireEvent.wheel(frame!, { deltaY: -900 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("用户向上滚动且 xterm 已有本地 scrollback 时会直接移动 viewport", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const snapshot = Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("");

      renderTerminalPaneWithOutput(
        [
          {
            kind: "snapshot",
            bytes: new TextEncoder().encode(snapshot),
            baseSeq: 30,
            size: DEFAULT_TERMINAL_SIZE,
          },
        ],
        { onTerminalResync },
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -320 });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：真实 xterm 有时不会自己消费 React wheel；termd 必须把
      // wheel 明确转换成 renderer-neutral scrollToLine，避免有 scrollback 也滚不动。
      expect(xterm?.viewportY()).toBeLessThan(xterm?.baseY() ?? 0);
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("高精度小步长滚轮会累积成实际的 scrollback 移动", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const snapshot = Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("");

      renderTerminalPaneWithOutput(
        [
          {
            kind: "snapshot",
            bytes: new TextEncoder().encode(snapshot),
            baseSeq: 30,
            size: DEFAULT_TERMINAL_SIZE,
          },
        ],
        { onTerminalResync },
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      for (let index = 0; index < 4; index += 1) {
        frame!.dispatchEvent(new WheelEvent("wheel", { deltaY: -4, bubbles: true, cancelable: true }));
      }

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：像素级小 delta 以前会被逐次 trunc 掉；累积后至少应该滚动一行。
      expect(xterm?.viewportY()).toBeLessThan(xterm?.baseY() ?? 0);
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端向下拖动且已有本地 scrollback 时会直接移动 viewport，向上拖动会回到更新内容", async () => {
    vi.useFakeTimers();
    try {
      const snapshot = Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("");

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={vi.fn<() => TerminalOutputItem[]>(() => [
            {
              kind: "snapshot",
              bytes: new TextEncoder().encode(snapshot),
              baseSeq: 30,
              size: DEFAULT_TERMINAL_SIZE,
            },
          ])}
          registerOutputDrain={vi.fn((drain: () => void) => {
            drain();
            return () => undefined;
          })}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();

      fireTouchPointer(frame!, "pointerdown", {
        pointerId: 41,
        clientX: 160,
        clientY: 196,
      });
      fireTouchPointer(frame!, "pointermove", {
        pointerId: 41,
        clientX: 160,
        clientY: 260,
      });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      expect(xterm?.viewportY()).toBeLessThan(xterm?.baseY() ?? 0);

      fireTouchPointer(frame!, "pointermove", {
        pointerId: 41,
        clientX: 160,
        clientY: 196,
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      expect(xterm?.viewportY()).toBeGreaterThanOrEqual(xterm?.baseY() ?? 0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端向下拖动且 xterm 无本地 scrollback 时会请求 supervisor snapshot history", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const fullscreenRedraw = "x".repeat(4 * 1024);

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          outputResetVersion={0}
          takeOutput={vi.fn<() => TerminalOutputItem[]>(() => [
            { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
            { kind: "output", bytes: new TextEncoder().encode(fullscreenRedraw), terminalSeq: 1 },
          ])}
          registerOutputDrain={vi.fn((drain: () => void) => {
            drain();
            return () => undefined;
          })}
          onTerminalResync={onTerminalResync}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();

      fireTouchPointer(frame!, "pointerdown", {
        pointerId: 42,
        clientX: 160,
        clientY: 196,
      });
      fireTouchPointer(frame!, "pointermove", {
        pointerId: 42,
        clientX: 160,
        clientY: 260,
      });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenCalledWith(undefined, { revealHistory: true });
    } finally {
      vi.useRealTimers();
    }
  });

  it("用户滚轮触发的 scrollback snapshot 回来后会自动停在历史区域", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: encoder.encode("x".repeat(4 * 1024)), terminalSeq: 1 },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        takeOutput,
        registerOutputDrain,
        onTerminalResync,
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };
      const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -900 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);
      expect(onTerminalResync).toHaveBeenCalledWith(undefined, { revealHistory: true });

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 30,
          size: DEFAULT_TERMINAL_SIZE,
          revealHistory: true,
        },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 20);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      // 中文注释：这次 snapshot 是用户向上滚触发的历史拉取；回来后应该直接看到历史，
      // 不能像普通 attach 一样又贴回底部，导致用户必须再滚第二次。
      expect(xterm?.viewportY()).toBeLessThan(xterm?.baseY() ?? 0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("带 revealHistory 的 snapshot 即使跨 outputResetVersion 重建也不会贴回底部", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 30,
          size: DEFAULT_TERMINAL_SIZE,
          revealHistory: true,
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        takeOutput,
        registerOutputDrain,
        onTerminalResync: vi.fn(),
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };

      const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 8);
      });

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 31,
          size: DEFAULT_TERMINAL_SIZE,
          revealHistory: true,
        },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 24);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      // 中文注释：App 的 full snapshot 重连会重建 xterm；reveal intent 必须跟着
      // snapshot item 进入新实例，否则新实例会按普通 attach 自动贴底。
      expect(xterm?.viewportY()).toBeLessThan(xterm?.baseY() ?? 0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("同 session 的 outputResetVersion 重建会沿用已确认 sessionSize，不回退到默认 80x24", async () => {
    const confirmedSessionSize = { rows: 35, cols: 108, pixel_width: 716, pixel_height: 668 };
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn((drain: () => void) => {
      drain();
      return () => undefined;
    });
    const props = {
      attached: true,
      sessionSize: confirmedSessionSize,
      takeOutput,
      registerOutputDrain,
      onInput: vi.fn(),
      onResize: vi.fn(),
      onCursorChange: vi.fn(),
    };

    const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);

    await waitFor(() => {
      const host = document.querySelector<HTMLElement>(".terminal-host");
      expect(host?.dataset.termdCols).toBe(String(confirmedSessionSize.cols));
      expect(host?.dataset.termdRows).toBe(String(confirmedSessionSize.rows));
    });

    rerender(<TerminalPane {...props} outputResetVersion={1} />);

    await waitFor(() => {
      const host = document.querySelector<HTMLElement>(".terminal-host");
      expect(host?.dataset.termdCols).toBe(String(confirmedSessionSize.cols));
      expect(host?.dataset.termdRows).toBe(String(confirmedSessionSize.rows));
    });
  });

  it("同 session 的 outputResetVersion 变化不会替换桌面输入框或丢失焦点", async () => {
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    const props = {
      attached: true,
      sessionSize: DEFAULT_TERMINAL_SIZE,
      takeOutput,
      registerOutputDrain,
      onTerminalResync: vi.fn(),
      onInput: vi.fn(),
      onResize: vi.fn(),
      onCursorChange: vi.fn(),
    };

    const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);
    let terminalTextarea: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalTextarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalTextarea).not.toBeNull();
    });
    const stableTextarea = terminalTextarea!;
    const blurSpy = vi.spyOn(stableTextarea, "blur");

    try {
      stableTextarea.focus();
      await waitFor(() => expect(document.activeElement).toBe(stableTextarea));

      rerender(<TerminalPane {...props} outputResetVersion={1} />);

      await waitFor(() => {
        expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).toBe(stableTextarea);
        expect(stableTextarea.isConnected).toBe(true);
        expect(document.activeElement).toBe(stableTextarea);
      });
      expect(blurSpy).not.toHaveBeenCalled();
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("同 session reset 期间浏览器短暂丢焦后会恢复桌面输入焦点", async () => {
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    const props = {
      attached: true,
      sessionSize: DEFAULT_TERMINAL_SIZE,
      takeOutput,
      registerOutputDrain,
      onTerminalResync: vi.fn(),
      onInput: vi.fn(),
      onResize: vi.fn(),
      onCursorChange: vi.fn(),
    };

    const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);
    let terminalTextarea: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalTextarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalTextarea).not.toBeNull();
    });
    const stableTextarea = terminalTextarea!;

    try {
      stableTextarea.focus();
      await waitFor(() => expect(document.activeElement).toBe(stableTextarea));
      (globalThis as { __TERMD_TEST_TERMINAL_BLUR_ON_RESET__?: boolean }).__TERMD_TEST_TERMINAL_BLUR_ON_RESET__ = true;

      rerender(<TerminalPane {...props} outputResetVersion={1} />);

      await waitFor(() => {
        expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).toBe(stableTextarea);
        expect(document.activeElement).toBe(stableTextarea);
      });
    } finally {
      delete (globalThis as { __TERMD_TEST_TERMINAL_BLUR_ON_RESET__?: boolean }).__TERMD_TEST_TERMINAL_BLUR_ON_RESET__;
    }
  });

  it("桌面窗口恢复时浏览器自动回到 helper textarea 不会被当成被动焦点打掉", async () => {
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    render(
      <TerminalPane
        attached
        sessionSize={DEFAULT_TERMINAL_SIZE}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={vi.fn()}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalTextarea = await waitFor(() => {
      const element = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(element).not.toBeNull();
      return element!;
    });
    const blurSpy = vi.spyOn(terminalTextarea, "blur");

    try {
      terminalTextarea.focus();
      await waitFor(() => expect(document.activeElement).toBe(terminalTextarea));

      window.dispatchEvent(new Event("blur"));
      await waitFor(() => expect(document.activeElement).not.toBe(terminalTextarea));
      blurSpy.mockClear();

      window.dispatchEvent(new Event("focus"));
      terminalTextarea.focus();

      await waitFor(() => expect(document.activeElement).toBe(terminalTextarea));
      expect(blurSpy).not.toHaveBeenCalled();
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("首个 snapshot reset 短暂丢焦后会恢复桌面输入焦点", async () => {
    const encoder = new TextEncoder();
    const queue: TerminalOutputItem[] = [];
    const takeOutput = vi.fn(() => queue.splice(0));
    let drainOutput: (() => void) | undefined;
    const registerOutputDrain = vi.fn((drain: () => void) => {
      drainOutput = drain;
      drain();
      return () => undefined;
    });
    const onInput = vi.fn();

    render(
      <TerminalPane
        attached
        sessionSize={DEFAULT_TERMINAL_SIZE}
        focusRequest={1}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={onInput}
        onResize={vi.fn()}
        onCursorChange={vi.fn()}
      />,
    );
    let terminalTextarea: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalTextarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalTextarea).not.toBeNull();
      expect(document.activeElement).toBe(terminalTextarea);
    });
    const stableTextarea = terminalTextarea!;

    try {
      (globalThis as { __TERMD_TEST_TERMINAL_BLUR_ON_RESET__?: boolean }).__TERMD_TEST_TERMINAL_BLUR_ON_RESET__ = true;
      queue.push({
        kind: "snapshot",
        bytes: encoder.encode("snapshot-ready\n"),
        baseSeq: 1,
        size: DEFAULT_TERMINAL_SIZE,
      });

      act(() => {
        drainOutput?.();
      });

      await waitFor(() => {
        const operations = (globalThis as {
          __TERMD_TEST_TERMINAL_STATS__?: {
            operations: Array<{ op: string; text?: string }>;
          };
        }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
        expect(operations.some((operation) => operation.op === "reset")).toBe(true);
        expect(operations.some((operation) => operation.op === "write" && operation.text === "snapshot-ready\n")).toBe(true);
        expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).toBe(stableTextarea);
        expect(document.activeElement).toBe(stableTextarea);
      });

      stableTextarea.value = "after-snapshot-reset";
      fireEvent.input(stableTextarea);
      expect(onInput).toHaveBeenCalledWith("after-snapshot-reset");
    } finally {
      delete (globalThis as { __TERMD_TEST_TERMINAL_BLUR_ON_RESET__?: boolean }).__TERMD_TEST_TERMINAL_BLUR_ON_RESET__;
    }
  });

  it("revealHistory snapshot 渲染后不会污染下一次普通 snapshot 贴底", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 30,
          size: DEFAULT_TERMINAL_SIZE,
          revealHistory: true,
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        takeOutput,
        registerOutputDrain,
        onTerminalResync: vi.fn(),
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };

      const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 24);
      });

      const firstxterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(firstxterm?.baseY()).toBeGreaterThan(0);
      expect(firstxterm?.viewportY()).toBeLessThan(firstxterm?.baseY() ?? 0);

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 31,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 24);
      });

      const secondxterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      // 中文注释：revealHistory 只属于当前 snapshot。下一次普通 attach/theme/full snapshot
      // 必须恢复默认贴底，不能被上一轮用户上滚意图污染。
      expect(secondxterm?.viewportY()).toBe(secondxterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("未消费的 reveal-history 请求在 outputResetVersion 重建时不会污染普通 snapshot", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      const onTerminalResync = vi.fn();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: encoder.encode("x".repeat(4 * 1024)), terminalSeq: 1 },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        takeOutput,
        registerOutputDrain,
        onTerminalResync,
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };

      const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -900 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledWith(undefined, { revealHistory: true });

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `${index + 1}\n`).join("")),
          baseSeq: 31,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);
      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 24);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      // 中文注释：用户上滚意图如果还没等到 snapshot 就遇到 reset，必须随旧 xterm buffer 失效。
      // 新 buffer 收到普通 snapshot 时仍要贴底，不能继承旧 ref 的“停在历史区”行为。
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("revealHistory 的延迟收尾不会作用到新的普通 snapshot", async () => {
    vi.useFakeTimers();
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `reveal-${index + 1}\n`).join("")),
          baseSeq: 30,
          size: DEFAULT_TERMINAL_SIZE,
          revealHistory: true,
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        takeOutput,
        registerOutputDrain,
        onTerminalResync: vi.fn(),
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };
      const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 40);
      });
      expect(terminalHost().dataset.buffer).toContain("reveal-220");

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 220 }, (_, index) => `normal-${index + 1}\n`).join("")),
          baseSeq: 31,
          size: DEFAULT_TERMINAL_SIZE,
        },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 80);
      });

      expect(terminalHost().dataset.buffer).toContain("normal-220");
      const xterm = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_TERMINAL__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("可见时排队的 xterm write 在 hidden 后会切到 timer fallback", async () => {
    vi.useFakeTimers();
    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
      });
      setDocumentHasFocus(true);
      rafQueue.clear();
      queue = [{ kind: "data", bytes: encoder.encode("writer-hidden-race\n") }];

      act(() => {
        drainOutput?.();
      });

      const terminalStats = () => (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number } }).__TERMD_TEST_TERMINAL_STATS__;
      expect(terminalStats()?.writes ?? 0).toBe(0);
      expect(rafQueue.size).toBeGreaterThan(0);

      act(() => {
        setDocumentVisibility("hidden");
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });

      expect(screen.getByText(/writer-hidden-race/)).toBeInTheDocument();
      expect(terminalStats()?.writes ?? 0).toBe(1);
    } finally {
      restoreDocumentVisibility();
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("可见时排队的 xterm write 在 blur 后会切到 timer fallback", async () => {
    vi.useFakeTimers();
    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
      });
      setDocumentHasFocus(true);
      rafQueue.clear();
      queue = [{ kind: "data", bytes: encoder.encode("writer-blur-race\n") }];

      act(() => {
        drainOutput?.();
      });
      expect(rafQueue.size).toBeGreaterThan(0);

      setDocumentHasFocus(false);
      act(() => {
        window.dispatchEvent(new Event("blur"));
      });
      await act(async () => {
        await vi.runOnlyPendingTimersAsync();
        await vi.runOnlyPendingTimersAsync();
      });

      expect(screen.getByText(/writer-blur-race/)).toBeInTheDocument();
    } finally {
      restoreDocumentVisibility();
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("窗口 blur 后新到的 xterm write 直接走 timer fallback", async () => {
    vi.useFakeTimers();
    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);
    try {
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
      });
      setDocumentHasFocus(false);
      window.dispatchEvent(new Event("blur"));
      rafQueue.clear();
      queue = [{ kind: "data", bytes: encoder.encode("writer-blur-direct\n") }];

      act(() => {
        drainOutput?.();
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });

      expect(screen.getByText(/writer-blur-direct/)).toBeInTheDocument();
    } finally {
      restoreDocumentVisibility();
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("xterm write callback 在 blur 后被 rescue 时，后续 stdout 不会继续卡在 writeInFlight", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__ = true;
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
      });
      setDocumentHasFocus(true);
      queue = [{ kind: "data", bytes: encoder.encode("stalled-write-1\n") }];

      act(() => {
        drainOutput?.();
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 2);
      });

      const terminalStats = () => (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number } }).__TERMD_TEST_TERMINAL_STATS__;
      expect(terminalStats()?.writes ?? 0).toBe(1);
      expect(screen.getByText(/stalled-write-1/)).toBeInTheDocument();

      queue = [{ kind: "data", bytes: encoder.encode("stalled-write-2\n") }];
      act(() => {
        drainOutput?.();
      });
      await act(async () => {
        await Promise.resolve();
      });
      expect(terminalStats()?.writes ?? 0).toBe(1);

      setDocumentHasFocus(false);
      act(() => {
        window.dispatchEvent(new Event("blur"));
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
      });

      // 中文注释：这里模拟的是真实 bug：上一个 xterm write 的 completion callback
      // 因失焦被冻结。blur rescue 必须解开 in-flight 锁，并继续排空后续 stdout。
      expect(terminalStats()?.writes ?? 0).toBeGreaterThanOrEqual(2);
      expect(screen.getByText(/stalled-write-2/)).toBeInTheDocument();
    } finally {
      restoreDocumentVisibility();
      vi.useRealTimers();
    }
  });

  it("全屏程序 resync 等待 snapshot 期间不会按 cooldown 周期重复请求", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: encoder.encode("x".repeat(9 * 1024)), terminalSeq: 1 },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });

      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onTerminalResync={onTerminalResync}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
        vi.advanceTimersByTime(1000 + animationFrameMs * 2);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);

      queue = [{ kind: "output", bytes: encoder.encode("y".repeat(9 * 1024)), terminalSeq: 2 }];
      act(() => {
        // 中文注释：超过 cooldown 后如果 snapshot 还没回来，pending 标记必须继续挡住重复全量拉取。
        vi.advanceTimersByTime(6_000);
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 24);
      });

      expect(onTerminalResync).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("切换 session 后 scrollback resync 冷却会重置", async () => {
    vi.useFakeTimers();
    try {
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: encoder.encode("x".repeat(9 * 1024)), terminalSeq: 1 },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      const props = {
        attached: true,
        sessionSize: DEFAULT_TERMINAL_SIZE,
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onTerminalResync,
        onInput: vi.fn(),
        onResize: vi.fn(),
        onCursorChange: vi.fn(),
      };

      const { rerender } = render(<TerminalPane {...props} />);

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
        await vi.advanceTimersByTimeAsync(1000 + animationFrameMs * 2);
      });
      expect(onTerminalResync).toHaveBeenCalledTimes(1);

      queue = [
        { kind: "snapshot", bytes: new Uint8Array(), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
        { kind: "output", bytes: encoder.encode("y".repeat(9 * 1024)), terminalSeq: 1 },
      ];
      rerender(<TerminalPane {...props} outputResetVersion={1} />);

      await act(async () => {
        drainOutput?.();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 12);
        await vi.advanceTimersByTimeAsync(1000 + animationFrameMs * 2);
      });

      // 中文注释：新 session 重新挂载后，第一次无 scrollback burst 必须能再次触发 resync；
      // 否则旧 session 的 cooldown 会把历史拉取挡住，用户上滚看到的就不是历史内容。
      expect(onTerminalResync).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("切换 session 时旧的异步 write 回调不能阻塞或确认新 session", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_SERIALIZE_TERMINAL_WRITES__?: boolean })
        .__TERMD_TEST_SERIALIZE_TERMINAL_WRITES__ = true;
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: encoder.encode("old-session\n"), baseSeq: 10, size: DEFAULT_TERMINAL_SIZE },
      ];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      const onTerminalSeqRendered = vi.fn();
      const onOutputResetApplied = vi.fn();

      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onOutputResetApplied={onOutputResetApplied}
          onTerminalSeqRendered={onTerminalSeqRendered}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await Promise.resolve();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 2);
      });
      expect((globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number } }).__TERMD_TEST_TERMINAL_STATS__?.writes).toBe(1);
      expect(onTerminalSeqRendered).not.toHaveBeenCalled();

      queue = [
        { kind: "snapshot", bytes: encoder.encode("new-session\n"), baseSeq: 30, size: DEFAULT_TERMINAL_SIZE },
      ];
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={1}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onOutputResetApplied={onOutputResetApplied}
          onTerminalSeqRendered={onTerminalSeqRendered}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        drainOutput?.();
        await vi.advanceTimersByTimeAsync(animationFrameMs * 2);
      });
      expect(onOutputResetApplied).toHaveBeenCalledWith(1);

      // 中文注释：旧 session 的 write 回调尚未返回时，新 session 的 snapshot 也必须能开始写入；
      // 否则用户快速切 session 会被旧的大量输出拖住，表现为整个 Web 延迟数秒。
      expect((globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number } }).__TERMD_TEST_TERMINAL_STATS__?.writes).toBe(2);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 6);
      });

      expect(onTerminalSeqRendered.mock.calls).toEqual([[30]]);
      const host = terminalHost();
      act(() => {
        // 中文注释：TerminalPane 在 write callback 之后再排一帧 refresh；这里单独推进，确认新实例完成绘制。
        vi.advanceTimersByTime(animationFrameMs);
      });
      expect(host.dataset.buffer).toContain("new-session");
      expect(host.dataset.buffer).not.toContain("old-session");
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("TerminalPane terminal search", () => {
  it("忽略旧搜索请求的迟到结果", async () => {
    const alphaSearch = deferred<SessionSearchResultPayload>();
    const betaSearch = deferred<SessionSearchResultPayload>();
    const onSearch = vi.fn((query: string) => (query === "alpha" ? alphaSearch.promise : betaSearch.promise));

    render(
      <TerminalPane
        attached
        sessionSize={DEFAULT_TERMINAL_SIZE}
        outputResetVersion={0}
        takeOutput={() => []}
        registerOutputDrain={() => () => undefined}
        onSearch={onSearch}
        onInput={vi.fn()}
        onResize={vi.fn()}
        onCursorChange={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Search terminal" }));
    const input = screen.getByPlaceholderText("Search scrollback");
    const form = input.closest("form");
    expect(form).not.toBeNull();

    fireEvent.change(input, { target: { value: "alpha" } });
    fireEvent.submit(form!);
    fireEvent.change(input, { target: { value: "beta" } });
    fireEvent.submit(form!);
    expect(onSearch).toHaveBeenNthCalledWith(1, "alpha");
    expect(onSearch).toHaveBeenNthCalledWith(2, "beta");

    await act(async () => {
      betaSearch.resolve(searchResult("beta", 2));
      await betaSearch.promise;
    });
    expect(await screen.findByText("1/2")).toBeInTheDocument();
    expect(screen.getByTestId("terminal-search-highlight")).toHaveTextContent("beta");

    await act(async () => {
      alphaSearch.resolve(searchResult("alpha", 1));
      await alphaSearch.promise;
    });

    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByTestId("terminal-search-highlight")).toHaveTextContent("beta");
  });
});

describe("TerminalPane mobile direction gesture", () => {
  it("静止长按不抢系统长按菜单，contextmenu 也不会被阻止", () => {
    vi.useFakeTimers();
    try {
      const onInput = vi.fn();
      const { frame } = renderMobileTerminalPane(onInput);
      fireTouchPointer(frame, "pointerdown", {
        pointerId: 20,
        clientX: 160,
        clientY: 240,
      });
      act(() => {
        vi.advanceTimersByTime(1000);
      });

      expect(screen.queryByLabelText("mobile direction gesture")).toBeNull();
      const contextMenuEvent = new Event("contextmenu", { bubbles: true, cancelable: true });
      fireEvent(frame, contextMenuEvent);
      expect(contextMenuEvent.defaultPrevented).toBe(false);

      fireTouchPointer(frame, "pointermove", {
        pointerId: 20,
        clientX: 160,
        clientY: 196,
      });
      expect(screen.queryByLabelText("mobile direction gesture")).toBeNull();
      expect(onInput).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("一档每半秒发送一个方向键，并在松手后停止", () => {
    vi.useFakeTimers();
    try {
      const onInput = vi.fn();
      const { frame } = renderMobileTerminalPane(onInput);
      activateMobileDirectionGesture(frame, 21);

      fireTouchPointer(frame, "pointermove", {
        pointerId: 21,
        clientX: 160,
        clientY: 208,
      });

      expect(onInput).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(499);
      });
      expect(onInput).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(1);
      });
      expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A"]);
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A", "\x1b[A"]);

      fireTouchPointer(frame, "pointerup", {
        pointerId: 21,
        clientX: 160,
        clientY: 208,
      });
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A", "\x1b[A"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("二档每半秒发送两个方向键", () => {
    vi.useFakeTimers();
    try {
      const onInput = vi.fn();
      const { frame } = renderMobileTerminalPane(onInput);
      activateMobileDirectionGesture(frame, 22);

      fireTouchPointer(frame, "pointermove", {
        pointerId: 22,
        clientX: 160,
        clientY: 170,
      });

      expect(onInput).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A", "\x1b[A"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("三档保持拖动即时发送", () => {
    vi.useFakeTimers();
    try {
      const onInput = vi.fn();
      const { frame } = renderMobileTerminalPane(onInput);
      activateMobileDirectionGesture(frame, 23);

      fireTouchPointer(frame, "pointermove", {
        pointerId: 23,
        clientX: 160,
        clientY: 130,
      });
      fireTouchPointer(frame, "pointermove", {
        pointerId: 23,
        clientX: 160,
        clientY: 85,
      });

      expect(onInput.mock.calls.map(([data]) => data)).toEqual(["\x1b[A", "\x1b[A"]);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("TerminalPane terminal sizing", () => {
  it("刷新和聚焦期间同一行列尺寸只上报一次", () => {
    vi.useFakeTimers();
    let hostWidth = 800;
    let hostHeight = 500;
    const clientWidthSpy = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
      return this.classList.contains("terminal-host") ? hostWidth : 0;
    });
    const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
      return this.classList.contains("terminal-host") ? hostHeight : 0;
    });
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          focusRequest={1}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      hostWidth = 801;
      hostHeight = 501;
      act(() => {
        window.dispatchEvent(new Event("resize"));
        vi.advanceTimersByTime(animationFrameMs * 4);
      });

      // 中文注释：刷新后 focusRequest、focusin、ResizeObserver/window resize
      // 都可能撞在同一轮布局里；相同 rows/cols 不应该因为像素抖动被重复写回 PTY。
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);
    } finally {
      clientWidthSpy.mockRestore();
      clientHeightSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("focusRequest 首次 attach 时即使原生 focus 延迟也会上报稳定尺寸", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__?: boolean }).__TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__ = true;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          focusRequest={1}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 16);
      });

      // 中文注释：真实浏览器里 terminal.focus() 和 focusin 之间可能隔着一轮以上布局；
      // 即使 DOM focus 还没稳定，也要让首个 focusRequest 把真实 rows/cols 写回 PTY。
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("新 session 的 focusRequest 首轮成功后若下一帧暂态掉到 body，会在稳定帧恢复输入焦点", () => {
    vi.useFakeTimers();
    const previousBodyTabIndex = document.body.getAttribute("tabindex");
    try {
      render(
        <TerminalPane
          attached
          sessionSize={DEFAULT_TERMINAL_SIZE}
          focusRequest={1}
          outputResetVersion={0}
          takeOutput={vi.fn(() => [])}
          registerOutputDrain={vi.fn(() => () => undefined)}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();

      act(() => vi.advanceTimersByTime(animationFrameMs));
      expect(document.activeElement).toBe(terminalInput);

      document.body.tabIndex = -1;
      act(() => document.body.focus());
      expect(document.activeElement).toBe(document.body);

      act(() => vi.advanceTimersByTime(animationFrameMs * 3));
      expect(document.activeElement).toBe(terminalInput);
    } finally {
      if (previousBodyTabIndex === null) {
        document.body.removeAttribute("tabindex");
      } else {
        document.body.setAttribute("tabindex", previousBodyTabIndex);
      }
      vi.useRealTimers();
    }
  });

  it.each(["toolbar", "input", "select", "contenteditable"] as const)(
    "focusRequest 稳定帧不会从外部 %s 控件抢回焦点",
    (controlKind) => {
      vi.useFakeTimers();
      try {
        const externalControl = controlKind === "toolbar"
          ? <div className="toolbar"><button type="button" data-testid="external-focus-control">tool</button></div>
          : controlKind === "input"
            ? <input data-testid="external-focus-control" />
            : controlKind === "select"
              ? <select data-testid="external-focus-control"><option>choice</option></select>
              : <div contentEditable suppressContentEditableWarning tabIndex={0} data-testid="external-focus-control">editor</div>;
        render(
          <>
            <TerminalPane
              attached
              sessionSize={DEFAULT_TERMINAL_SIZE}
              focusRequest={1}
              outputResetVersion={0}
              takeOutput={vi.fn(() => [])}
              registerOutputDrain={vi.fn(() => () => undefined)}
              onInput={vi.fn()}
              onResize={vi.fn()}
              onCursorChange={vi.fn()}
            />
            {externalControl}
          </>,
        );
        const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        const external = screen.getByTestId("external-focus-control") as HTMLElement;
        act(() => vi.advanceTimersByTime(animationFrameMs));
        expect(document.activeElement).toBe(terminalInput);

        act(() => external.focus());
        expect(document.activeElement).toBe(external);
        act(() => vi.advanceTimersByTime(animationFrameMs * 4));

        expect(document.activeElement).toBe(external);
      } finally {
        vi.useRealTimers();
      }
    },
  );

  it("刷新和聚焦期间经过临时行列尺寸时只上报最终尺寸", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 29,
        cols: 96,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          focusRequest={1}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs);
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      act(() => {
        window.dispatchEvent(new Event("resize"));
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      // 中文注释：浏览器 reload/focus 时布局可能先给出临时行列，再稳定到最终行列。
      // 对 shared PTY 只能上报最终行列，避免 supervisor/daemon 被中间尺寸来回 resize。
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("刷新和聚焦期间的临时行列变化会先遮住 canvas", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 29,
        cols: 96,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
      });

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      act(() => {
        window.dispatchEvent(new Event("resize"));
      });
      const host = screen.getByRole("textbox", { name: "Terminal input" });
      expect(host).toHaveAttribute("data-termd-resize-stabilizing", "true");
      act(() => {
        vi.advanceTimersByTime(240);
      });
      expect(host).not.toHaveAttribute("data-termd-resize-stabilizing");
      expect(onResize).toHaveBeenCalledWith({
        rows: 31,
        cols: 101,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("聚焦终端上报 resize 时先按本地可用高度撑开 xterm", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 31,
      cols: 101,
    };
    render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();

    await waitFor(() =>
      expect(onResize).toHaveBeenCalledWith({
        rows: 31,
        cols: 101,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      }),
    );
    const operations = (globalThis as {
      __TERMD_TEST_TERMINAL_STATS__?: {
        operations: Array<{ op: string; cols?: number; rows?: number }>;
      };
    }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
    // 中文注释：daemon 确认可能因为持续输出延迟；聚焦客户端仍要先撑开本地视口，
    // 否则 xterm 会长期停在默认 24 行，外层面板下方只剩大片空白。
    expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
  });

  it("窗口 blur 不会把本地 xterm 强制缩回远端尺寸，回焦时不经历远端网格闪回", async () => {
    vi.useFakeTimers();
    try {
      const remoteSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          sessionSize={remoteSize}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      onResize.mockClear();
      const resizeOperationCountBeforeBlur = ((globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? []).length;

      act(() => {
        window.dispatchEvent(new Event("blur"));
        vi.advanceTimersByTime(240);
      });

      const operationsAfterBlur = ((globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? []).slice(resizeOperationCountBeforeBlur);
      expect(operationsAfterBlur).not.toContainEqual({ op: "resize", cols: remoteSize.cols, rows: remoteSize.rows });
      expect(onResize).not.toHaveBeenCalledWith({
        rows: remoteSize.rows,
        cols: remoteSize.cols,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("snapshot 重绘期间的主动聚焦会在 snapshot 完成后补发 resize", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      expect(onResize).toHaveBeenCalledWith({
        rows: 31,
        cols: 101,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
      onResize.mockClear();

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode("root@host:~# "),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const operationsBeforeSnapshot = ((globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number; text?: string }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? []).length;
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs);
      });
      const snapshotBeginOperations = ((globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number; text?: string }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? []).slice(operationsBeforeSnapshot);
      // 中文注释：先确认测试已经进入 snapshot 重绘遮罩，但 xterm write callback
      // 还没有完成；此时主动聚焦必须被延后到 snapshot 渲染完成后补发。
      expect(snapshotBeginOperations).toContainEqual({ op: "reset" });
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      expect(onResize).not.toHaveBeenCalled();

      // 中文注释：snapshot 字节写入期间不能改 xterm 尺寸；但用户的主动聚焦不能丢，
      // snapshot 完成后要补发一次 resize，让当前客户端重新接管 shared PTY 尺寸。
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onResize).toHaveBeenCalledWith({
        rows: 31,
        cols: 101,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
      const operations = (globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number; text?: string }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
      expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
      expect(
        operations.some((operation) => operation.op === "write" && operation.text === "\x1b7\x1b[r\x1b8"),
      ).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端 visualViewport 变化期间 snapshot 完成后不会补发 focus resize 或 full resync", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 460, 0);
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onTerminalResync={onTerminalResync}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      onResize.mockClear();

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode("root@host:~# "),
          baseSeq: 0,
          size: { rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      viewport.setMetrics(820, 820, 0);
      window.dispatchEvent(new Event("resize"));
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onTerminalResync={onTerminalResync}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 32);
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });

      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端 visualViewport suppress 不会吞掉 snapshot 期间的显式 focus resize", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 460, 0);
    try {
      const onResize = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      let focusRequest = 1;
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      onResize.mockClear();

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode("root@host:~# "),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      viewport.setMetrics(820, 820, 0);
      window.dispatchEvent(new Event("resize"));
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 2);
      });
      focusRequest = 2;
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      expect(onResize).not.toHaveBeenCalled();

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onResize).toHaveBeenCalledWith({
        rows: 31,
        cols: 101,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("reload snapshot 从旧尺寸恢复时只补发最终本地尺寸", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          focusRequest={1}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });

      // 中文注释：reload snapshot 会先按 daemon 的旧 80x24 重放，但对 PTY 的回写
      // 只能是当前浏览器最终尺寸；不允许 80x24 和中间尺寸反复打回 daemon。
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);
      const operations = (globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_TERMINAL_STATS__?.operations ?? [];
      expect(operations.filter((operation) => operation.op === "resize" && operation.cols === 80 && operation.rows === 24)).toEqual([]);
      expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
    } finally {
      vi.useRealTimers();
    }
  });

  it("旧尺寸 snapshot 若在首次聚焦后才接管 resize，daemon 确认新尺寸后也会补一次 full snapshot 修复历史", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      const sharedProps = {
        attached: true,
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onInput: vi.fn(),
        onResize,
        onCursorChange: vi.fn(),
        onTerminalResync,
      };
      const { rerender } = render(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toContain("101x31");

      rerender(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });

      // 中文注释：初开页面时浏览器可能先显示旧 snapshot，直到用户第一次点进终端才接管
      // PTY resize。此时也必须在 daemon ack 后再拉一遍 full snapshot，否则底部输入区
      // 和旧网格正文会错开一行。
      expect(onTerminalResync).toHaveBeenCalledWith(undefined);
    } finally {
      vi.useRealTimers();
    }
  });

  it("旧尺寸 snapshot 在 daemon 确认新尺寸后会再请求一次 full snapshot 修复历史", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      const sharedProps = {
        attached: true,
        focusRequest: 1,
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onInput: vi.fn(),
        onResize,
        onCursorChange: vi.fn(),
        onTerminalResync,
      };
      const { rerender } = render(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });

      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);
      expect(onTerminalResync).not.toHaveBeenCalled();

      rerender(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });

      // 中文注释：daemon/supervisor 已经确认了新的 rows/cols，此时要再做一次 full snapshot，
      // 把旧 80x24 重放留下的 scrollback 一起按新尺寸重建。
      expect(onTerminalResync).toHaveBeenCalledWith(undefined);

      onTerminalResync.mockClear();
      rerender(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 4);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("历史修复等待 idle settle 时若继续有输出，会直接放弃自动 full snapshot", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      let drainOutput: (() => void) | undefined;
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      const sharedProps = {
        attached: true,
        focusRequest: 1,
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onInput: vi.fn(),
        onResize,
        onCursorChange: vi.fn(),
        onTerminalResync,
      };
      const { rerender } = render(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toEqual(["101x31"]);

      rerender(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(300);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();

      queue.push({
        kind: "output",
        bytes: encoder.encode("live-burst\n"),
        terminalSeq: 1,
      });
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(450);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();

      act(() => {
        vi.advanceTimersByTime(1500);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("daemon 新尺寸已先确认时，旧尺寸 snapshot 晚到也会补发历史修复 full snapshot", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      const takeOutput = vi.fn(() => queue.splice(0));
      let drainOutput: (() => void) | undefined;
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      const sharedProps = {
        attached: true,
        focusRequest: 1,
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onInput: vi.fn(),
        onResize,
        onCursorChange: vi.fn(),
        onTerminalResync,
      };
      render(
        <TerminalPane
          {...sharedProps}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();

      queue.push({
        kind: "snapshot",
        bytes: encoder.encode("reload-old-size\n"),
        baseSeq: 0,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      });
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();

      act(() => {
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledWith(undefined);
    } finally {
      vi.useRealTimers();
    }
  });

  it("daemon 新尺寸已先确认且用户稍后首次聚焦接管 resize 时，也会补发历史修复 full snapshot", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
          onTerminalResync={onTerminalResync}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toContain("101x31");
      expect(onTerminalResync).not.toHaveBeenCalled();

      act(() => {
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });
      expect(onTerminalResync).toHaveBeenCalledWith(undefined);
    } finally {
      vi.useRealTimers();
    }
  });

  it("旧尺寸 snapshot 之后若已经有 live output，即使 scrollback 计数被清零也不会再触发历史修复 full snapshot", async () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      let drainOutput: (() => void) | undefined;
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      render(
        <TerminalPane
          attached
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
          onTerminalResync={onTerminalResync}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });
      expect(onTerminalResync).not.toHaveBeenCalled();

      queue.push({
        kind: "output",
        // 中文注释：足够大的持续输出会让测试桩形成本地 scrollback，模拟真实 relay 长挂会话。
        bytes: encoder.encode(Array.from({ length: 96 }, (_, index) => `live-output-${index + 1}\n`).join("")),
        terminalSeq: 1,
      });
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 32);
      });

      const terminal = (globalThis as {
        __TERMD_TEST_TERMINAL__?: { baseY: () => number; viewportY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_TERMINAL__;
      expect(terminal?.baseY()).toBeGreaterThan(0);
      expect(terminal?.viewportY()).toBe(terminal?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -320 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });
      expect(terminal?.viewportY()).toBeLessThan(terminal?.baseY() ?? 0);

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      expect(onResize.mock.calls.map(([size]) => `${size.cols}x${size.rows}`)).toContain("101x31");

      act(() => {
        vi.advanceTimersByTime(1000 + animationFrameMs * 8);
      });

      // 中文注释：这次 pending repair 来自旧 snapshot，但该 snapshot 之后已经见过 live output。
      // 即使 scrollback 路径把统计计数清零，也不能再主动 full resync 打断当前 attach。
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("snapshot 历史修复在 ack 后若立刻 detach，不会再触发过期 full snapshot", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode("reload-old-size\n"),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      const sharedProps = {
        outputResetVersion: 0,
        takeOutput,
        registerOutputDrain,
        onInput: vi.fn(),
        onResize,
        onCursorChange: vi.fn(),
        onTerminalResync,
      };
      const { rerender } = render(
        <TerminalPane
          {...sharedProps}
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 32);
      });

      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      onTerminalResync.mockClear();

      act(() => {
        rerender(
          <TerminalPane
            {...sharedProps}
            attached
            sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
          />,
        );
      });
      rerender(
        <TerminalPane
          {...sharedProps}
          attached={false}
          sessionSize={{ rows: 31, cols: 101, pixel_width: 0, pixel_height: 0 }}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：ack 后排队的历史修复只属于当前 pane。
      // 一旦 detach/unmount，就必须取消这帧 full snapshot，不能让旧 pane 在下一帧
      // 再向 daemon 拉一次过期快照。
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端软键盘弹出导致可视高度变小时不向 daemon 上报较小尺寸", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        focusRequest={1}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    onResize.mockClear();

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 4));
    });

    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端 visualViewport 高度变化但 keyboardOpen 不变时也只做本地刷新", async () => {
    const onResize = vi.fn();
    const onTerminalResync = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={460}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    onResize.mockClear();

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 4));
    });

    expect(onResize).not.toHaveBeenCalled();
    expect(onTerminalResync).not.toHaveBeenCalled();
  });

  it("移动端键盘打开后会把终端底边贴住快捷键上方", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 820, 0);
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      const queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 40 }, (_, index) => `bottom-align-${index + 1}\n`).join("")),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const resizeCountBeforeKeyboardOpen = (globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: { resizes: number };
      }).__TERMD_TEST_TERMINAL_STATS__?.resizes ?? 0;

      const terminal = (globalThis as {
        __TERMD_TEST_TERMINAL__?: {
          viewportY: () => number;
          baseY: () => number;
          scrollToLine: (line: number) => void;
          forceCursorPosition: (cursorY: number) => void;
        };
      }).__TERMD_TEST_TERMINAL__;
      expect(terminal).toBeDefined();
      const pane = screen.getByTestId("terminal-pane");
      const scrollport = pane.querySelector<HTMLElement>(".terminal-scrollport");
      const frame = pane.querySelector<HTMLElement>(".terminal-frame");
      const canvas = pane.querySelector<HTMLElement>(".terminal-canvas");
      expect(scrollport).not.toBeNull();
      expect(frame).not.toBeNull();
      expect(canvas).not.toBeNull();
      Object.defineProperty(scrollport!, "clientHeight", {
        configurable: true,
        get: () => 12 * 16,
      });

      onResize.mockClear();
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      viewport.setMetrics(820, 460, 20);
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      // 中文注释：软键盘打开只缩小可见 viewport，不能把 xterm 本地 rows 改成键盘后的
      // 12 行，否则 helper textarea 会在键盘动画期被重新锚定，真机键盘容易立刻收起。
      expect((globalThis as {
        __TERMD_TEST_TERMINAL_STATS__?: { resizes: number };
      }).__TERMD_TEST_TERMINAL_STATS__?.resizes ?? 0).toBe(resizeCountBeforeKeyboardOpen);
      onResize.mockClear();

      terminal?.forceCursorPosition(4);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      expect(onResize).not.toHaveBeenCalled();
      const baseYAfterKeyboardOpen = terminal?.baseY() ?? 0;
      expect(baseYAfterKeyboardOpen).toBeGreaterThan(8);
      // 中文注释：软键盘打开后仍保持 24 行 session 网格；外层 scrollport 裁剪出底部
      // 12 行，让终端底边贴住快捷键上方，而不是把 xterm viewport 滚到输入焦点附近。
      expect(terminal?.viewportY()).toBe(baseYAfterKeyboardOpen);
      expect(frame!.style.height).toBe(`${24 * 16}px`);
      expect(canvas!.style.height).toBe(`${24 * 16}px`);
      expect(scrollport!.scrollTop).toBe(12 * 16);
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端光标在终端底部时打开键盘不会制造底部留白", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 820, 0);
    try {
      const onResize = vi.fn();
      const encoder = new TextEncoder();
      const queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 80 }, (_, index) => `history-bottom-${index + 1}\n`).join("")),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const terminal = (globalThis as {
        __TERMD_TEST_TERMINAL__?: {
          viewportY: () => number;
          baseY: () => number;
          scrollToLine: (line: number) => void;
          forceCursorPosition: (cursorY: number) => void;
        };
      }).__TERMD_TEST_TERMINAL__;
      expect(terminal).toBeDefined();

      terminal?.scrollToLine(12);
      expect(terminal?.viewportY()).toBe(12);
      const scrollport = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-scrollport");
      expect(scrollport).not.toBeNull();
      Object.defineProperty(scrollport!, "clientHeight", {
        configurable: true,
        get: () => 12 * 16,
      });

      onResize.mockClear();
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      viewport.setMetrics(820, 460, 20);
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      expect(onResize).not.toHaveBeenCalled();
      expect(terminal?.baseY()).toBeGreaterThan(8);
      // 中文注释：键盘打开不改变 terminal.rows；这里光标位于 buffer 最底部，
      // 外层 scrollport 只需要裁剪出最后一屏，不能再追加半屏底部留白把光标抬到中线。
      expect(terminal?.viewportY()).toBe(57);
      expect(scrollport?.scrollTop).toBe(12 * 16);
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端键盘打开后重复贴底不会让终端窗口高度继续膨胀", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 820, 0);
    try {
      const encoder = new TextEncoder();
      const queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 80 }, (_, index) => `stable-bottom-${index + 1}\n`).join("")),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const pane = screen.getByTestId("terminal-pane");
      const scrollport = pane.querySelector<HTMLElement>(".terminal-scrollport");
      const frame = pane.querySelector<HTMLElement>(".terminal-frame");
      const canvas = pane.querySelector<HTMLElement>(".terminal-canvas");
      const host = pane.querySelector<HTMLElement>(".terminal-host");
      expect(scrollport).not.toBeNull();
      expect(frame).not.toBeNull();
      expect(canvas).not.toBeNull();
      expect(host).not.toBeNull();

      Object.defineProperty(scrollport!, "clientHeight", {
        configurable: true,
        get: () => 12 * 16,
      });
      Object.defineProperty(host!, "clientHeight", {
        configurable: true,
        get: () => {
          const expandedHeight = Number.parseFloat(frame!.style.height);
          return Number.isFinite(expandedHeight) && expandedHeight > 0 ? expandedHeight : 12 * 16;
        },
      });

      const terminal = (globalThis as {
        __TERMD_TEST_TERMINAL__?: {
          viewportY: () => number;
          scrollToLine: (line: number) => void;
          forceCursorPosition: (cursorY: number) => void;
        };
      }).__TERMD_TEST_TERMINAL__;
      expect(terminal).toBeDefined();
      terminal?.scrollToLine(12);

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      viewport.setMetrics(820, 460, 20);
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      const firstFrameHeight = frame!.style.height;
      const firstCanvasHeight = canvas!.style.height;
      expect(firstFrameHeight).not.toBe("");
      expect(firstCanvasHeight).not.toBe("");

      terminal?.forceCursorPosition(23);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      expect(frame!.style.height).toBe(firstFrameHeight);
      expect(canvas!.style.height).toBe(firstCanvasHeight);
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端键盘态重建终端时会清理上一 session 的可视行缓存", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 460, 20);
    try {
      const encoder = new TextEncoder();
      const queue: TerminalOutputItem[] = [
        {
          kind: "snapshot",
          bytes: encoder.encode(Array.from({ length: 40 }, (_, index) => `session-cleanup-${index + 1}\n`).join("")),
          baseSeq: 0,
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      const pane = screen.getByTestId("terminal-pane");
      const scrollport = pane.querySelector<HTMLElement>(".terminal-scrollport");
      const frame = pane.querySelector<HTMLElement>(".terminal-frame");
      expect(scrollport).not.toBeNull();
      expect(frame).not.toBeNull();
      Object.defineProperty(scrollport!, "clientHeight", {
        configurable: true,
        get: () => 12 * 16,
      });

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });
      expect(frame!.style.height).toBe("384px");

      // 中文注释：session rebuild 期间如果不清 mobileCursorVisibleRowsRef，
      // 新 renderer 会沿用上一 session 的 12 行窗口；这里把 fit 恢复成 24 行来暴露缓存泄漏。
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={1}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      expect(frame!.style.height).toBe("192px");
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端 visualViewport 变化伴随 window resize 时也不会绕过成 PTY resize", async () => {
    const onResize = vi.fn();
    const onTerminalResync = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={460}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    onResize.mockClear();

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );
    window.dispatchEvent(new Event("resize"));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 16));
    });

    expect(onResize).not.toHaveBeenCalled();
    expect(onTerminalResync).not.toHaveBeenCalled();
  });

  it("移动端 visualViewport 变化超过旧 suppress 时间窗后 window resize 仍不会写回 PTY", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      act(() => {
        terminalInput!.focus();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      onResize.mockClear();

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(450);
        window.dispatchEvent(new Event("resize"));
        vi.advanceTimersByTime(animationFrameMs * 16);
      });

      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端真实 viewport 宽度变化时仍会上报 PTY resize", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportWidth={390}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      act(() => {
        terminalInput!.focus();
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      onResize.mockClear();

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 104,
      };
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportWidth={820}
          mobileViewportHeight={390}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        window.dispatchEvent(new Event("resize"));
        vi.advanceTimersByTime(animationFrameMs * 20);
      });

      expect(onResize).toHaveBeenCalledWith({
        rows: 24,
        cols: 104,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端 window resize 先于 visualViewport props 更新时也不会写回 PTY", async () => {
    const viewport = installMutableVisualViewport(820, 460, 0);
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      terminalInput!.focus();
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      onResize.mockClear();

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      viewport.setMetrics(820, 820, 0);
      window.dispatchEvent(new Event("resize"));
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 16));
      });

      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      viewport.restore();
    }
  });

  it("移动端 visualViewport 变化超过旧 suppress 时间窗后 snapshot 完成仍不补发 resize 或 resync", () => {
    vi.useFakeTimers();
    const viewport = installMutableVisualViewport(820, 460, 0);
    try {
      const onResize = vi.fn();
      const onTerminalResync = vi.fn();
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [];
      let drainOutput: (() => void) | undefined;
      const takeOutput = vi.fn(() => queue.splice(0));
      const registerOutputDrain = vi.fn((drain: () => void) => {
        drainOutput = drain;
        drain();
        return () => undefined;
      });
      (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = screen.getByRole("textbox", { name: "Terminal input" });
      act(() => {
        terminalInput.focus();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });
      onResize.mockClear();

      queue = [
        {
          kind: "snapshot",
          bytes: encoder.encode("root@host:~# "),
          baseSeq: 0,
          size: { rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ];
      viewport.setMetrics(820, 820, 0);
      window.dispatchEvent(new Event("resize"));
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onTerminalResync={onTerminalResync}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(450 + animationFrameMs * 32);
        vi.advanceTimersByTime(1000 + animationFrameMs * 4);
      });

      expect(onResize).not.toHaveBeenCalled();
      expect(onTerminalResync).not.toHaveBeenCalled();
    } finally {
      viewport.restore();
      vi.useRealTimers();
    }
  });

  it("移动端 focusRequest 先于 visualViewport 更新时不会被 mobile-viewport 合并覆盖", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 31,
        cols: 101,
      };
      let focusRequest = 1;
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 20);
      });
      onResize.mockClear();

      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 33,
        cols: 104,
      };
      focusRequest = 2;
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={460}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          focusRequest={focusRequest}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 20);
      });

      expect(onResize).toHaveBeenCalledWith({
        rows: 33,
        cols: 104,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端早到 helper focus 期间，visualViewport 变化不会提前上报尺寸", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={460}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(frame).not.toBeNull();
    expect(terminalInput).not.toBeNull();

    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    terminalInput!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });
    onResize.mockClear();

    fireTouchPointer(frame!, "pointerdown", {
      pointerId: 6,
      clientX: 24,
      clientY: 24,
    });
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 4));
    });

    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端键盘打开后短暂 blur 不会卸载输入框或上报 resize", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));

    terminalInput!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 40));
    });

    // 中文注释：jsdom 不能可靠模拟系统键盘的 activeElement 恢复，这里只断言
    // React 状态没有把输入框卸载，也没有误把键盘高度写回 PTY；真实焦点恢复
    // 由 mobile-chrome Playwright 回归覆盖。
    expect(document.activeElement).not.toBeNull();
    expect(screen.getByRole("textbox", { name: "Terminal input" })).toBeInTheDocument();

    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });

    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端键盘已打开时输入框短暂掉到 body 会立刻恢复焦点", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));

    terminalInput!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });

    // 中文注释：真实手机软键盘弹出时，浏览器可能先把 activeElement 掉回 body。
    // 如果此时不立即恢复 helper textarea 焦点，系统键盘会把这次输入当作失焦并收起。
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端键盘恢复时 host 先获得焦点仍会继续重试到 helper textarea", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      const host = screen.getByRole("textbox", { name: "Terminal input" });
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();

      const nativeFocus = terminalInput!.focus.bind(terminalInput);
      let ignoredFocusCount = 0;
      const focusSpy = vi.spyOn(terminalInput!, "focus").mockImplementation(() => {
        ignoredFocusCount += 1;
        if (ignoredFocusCount <= 2) {
          host.focus();
          return;
        }
        nativeFocus();
      });

      act(() => {
        terminalInput!.focus();
      });
      expect(document.activeElement).toBe(host);
      act(() => {
        host.blur();
        window.dispatchEvent(new Event("blur"));
      });
      expect(document.activeElement).not.toBe(terminalInput);
      const focusAttemptsBeforeTimers = focusSpy.mock.calls.length;

      act(() => {
        vi.advanceTimersByTime(16);
      });
      expect(focusSpy.mock.calls.length).toBeGreaterThan(focusAttemptsBeforeTimers);
      act(() => {
        vi.advanceTimersByTime(32);
      });

      expect(document.activeElement).toBe(terminalInput);
      expect(focusSpy.mock.calls.length).toBeGreaterThanOrEqual(3);
      expect(onResize).not.toHaveBeenCalled();
      focusSpy.mockRestore();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端 helper textarea 已经获得 DOM focus 但键盘收起时，新的 touch click 才会重新激活输入", async () => {
    const { host } = renderMobileTerminalPane();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const focusSpy = vi.spyOn(HTMLTextAreaElement.prototype, "focus");
    const blurSpy = vi.spyOn(HTMLTextAreaElement.prototype, "blur");

    try {
      act(() => {
        terminalInput!.focus();
      });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      focusSpy.mockClear();
      act(() => {
        terminalInput!.blur();
      });
      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, 160));
      });
      blurSpy.mockClear();

      // 中文注释：这里模拟的是“DOM 还在聚焦，但系统软键盘已经收起”的边界状态。
      // 新模型下只有明确 tap/click 才会重新激活输入；pointerdown 本身不应再主动 focus。
      fireTouchPointer(host, "pointerdown", {
        pointerId: 99,
        clientX: 120,
        clientY: 120,
      });
      fireTouchPointer(host, "pointerup", {
        pointerId: 99,
        clientX: 120,
        clientY: 120,
      });
      fireEvent.click(host, { clientX: 120, clientY: 120, button: 0 });

      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      expect(focusSpy).toHaveBeenCalled();
      expect(blurSpy).not.toHaveBeenCalled();
    } finally {
      focusSpy.mockRestore();
      blurSpy.mockRestore();
    }
  });

  it("移动端点击终端正文时也会重新激活 helper textarea", async () => {
    const { frame, host } = renderMobileTerminalPane();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const focusSpy = vi.spyOn(terminalInput!, "focus");

    try {
      act(() => {
        terminalInput!.focus();
      });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      focusSpy.mockClear();
      act(() => {
        terminalInput!.blur();
      });
      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, 160));
      });

      // 中文注释：这里模拟用户点终端主页面正文，而不是点到滚动条或输入框。
      // 这条路径必须和滚动条附近点击一样，重新把 helper textarea 激活起来。
      fireTouchPointer(frame, "pointerdown", {
        pointerId: 101,
        clientX: 160,
        clientY: 120,
      });
      fireTouchPointer(frame, "pointerup", {
        pointerId: 101,
        clientX: 160,
        clientY: 120,
      });
      fireEvent.click(frame, { clientX: 160, clientY: 120, button: 0 });

      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      expect(focusSpy).toHaveBeenCalled();
      expect(host).not.toBeNull();
    } finally {
      focusSpy.mockRestore();
    }
  });

  it("移动端键盘已收起但 helper textarea 仍保留 DOM focus 时，点击终端正文会强制重新激活输入", async () => {
    const { frame } = renderMobileTerminalPane();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const focusSpy = vi.spyOn(terminalInput!, "focus");

    try {
      act(() => {
        terminalInput!.focus();
      });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));
      focusSpy.mockClear();

      // 中文注释：这里模拟真实移动端常见边界：DOM 焦点还留在 helper textarea，
      // 但系统软键盘已经收起。用户再次 tap 终端正文时，前端必须重新执行一次 focus，
      // 让浏览器把这次 tap 当成新的输入激活，而不能误判成“已经有焦点了”。
      fireTouchPointer(frame, "pointerdown", {
        pointerId: 131,
        clientX: 140,
        clientY: 110,
      });
      fireTouchPointer(frame, "pointerup", {
        pointerId: 131,
        clientX: 140,
        clientY: 110,
      });
      fireEvent.click(frame, { clientX: 140, clientY: 110, button: 0 });

      await waitFor(() => expect(focusSpy).toHaveBeenCalled());
      expect(document.activeElement).toBe(terminalInput);
    } finally {
      focusSpy.mockRestore();
    }
  });

  it("移动端键盘打开 rerender 不替换 helper textarea，也不主动 blur", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const blurSpy = vi.spyOn(terminalInput!, "blur");

    try {
      terminalInput!.focus();
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));

      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );

      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 4));
      });

      // 中文注释：软键盘打开只改变外层可视 viewport；xterm 的真实输入 textarea
      // 不能被 React 重建，也不能被我们主动 blur，否则真机键盘会立刻收起。
      expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).toBe(terminalInput);
      expect(terminalInput!.isConnected).toBe(true);
      expect(document.activeElement).toBe(terminalInput);
      expect(blurSpy).not.toHaveBeenCalled();
      expect(onResize).not.toHaveBeenCalled();
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("移动端未形成 click 前的 pointercancel 不会主动 blur helper textarea", async () => {
    const { frame } = renderMobileTerminalPane();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const blurSpy = vi.spyOn(terminalInput!, "blur");

    try {
      act(() => {
        terminalInput!.focus();
      });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));

      fireTouchPointer(frame, "pointerdown", {
        pointerId: 77,
        clientX: 120,
        clientY: 120,
      });

      fireTouchPointer(frame, "pointercancel", {
        pointerId: 77,
        clientX: 120,
        clientY: 120,
      });

      // 中文注释：即使这次 touch 没有形成 click，后续 pointercancel 也不能反向 blur
      // 已经存在的 helper textarea DOM focus，否则真实软键盘会被应用代码主动关掉。
      expect(blurSpy).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(terminalInput);
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("移动端已有 helper textarea 焦点时，迟到 contextmenu 也不能主动 blur", async () => {
    const { frame, onInput, onResize } = renderMobileTerminalPane();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    const blurSpy = vi.spyOn(terminalInput!, "blur");

    try {
      act(() => {
        terminalInput!.focus();
      });
      await waitFor(() => expect(document.activeElement).toBe(terminalInput));

      fireTouchPointer(frame, "pointerdown", {
        pointerId: 78,
        clientX: 120,
        clientY: 120,
      });

      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, 800));
      });
      fireEvent.contextMenu(frame);

      // 中文注释：真实移动浏览器可能在长按链路末尾迟到补发 contextmenu。
      // 这类事件不能把已经存在的 helper textarea 焦点反向 blur 掉。
      expect(blurSpy).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(terminalInput);
      terminalInput!.dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: "c",
        }),
      );
      expect(onInput).toHaveBeenCalledWith("c");
      expect(onResize).not.toHaveBeenCalled();
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("移动端首次 tap/click 激活输入时，不会把 xterm fit 到键盘后行数", async () => {
    const onResize = vi.fn();
    const { frame } = renderMobileTerminalPane({ onResize });
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const resizeCountBeforeTouch = (globalThis as {
      __TERMD_TEST_TERMINAL_STATS__?: { resizes: number };
    }).__TERMD_TEST_TERMINAL_STATS__?.resizes ?? 0;

    fireTouchPointer(frame, "pointerdown", {
      pointerId: 79,
      clientX: 120,
      clientY: 120,
    });
    fireTouchPointer(frame, "pointerup", {
      pointerId: 79,
      clientX: 120,
      clientY: 120,
    });
    fireEvent.click(frame, { clientX: 120, clientY: 120, button: 0 });
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 16));
    });

    // 中文注释：软键盘刚被 click 激活但 visualViewport 还没稳定时，focus/click 不能先把
    // xterm fit 成键盘后的 12 行，否则 helper textarea 会被重锚，真机键盘容易立刻收起。
    expect((globalThis as {
      __TERMD_TEST_TERMINAL_STATS__?: { resizes: number };
    }).__TERMD_TEST_TERMINAL_STATS__?.resizes ?? 0).toBe(resizeCountBeforeTouch);
    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端键盘打开时 window 短暂 blur/focus 不会丢失输入焦点", async () => {
    const onInput = vi.fn();
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={onInput}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));

    window.dispatchEvent(new Event("blur"));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 40));
    });
    window.dispatchEvent(new Event("focus"));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 40));
    });

    expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    terminalInput!.dispatchEvent(
      new InputEvent("beforeinput", {
        bubbles: true,
        cancelable: true,
        inputType: "insertText",
        data: "k",
      }),
    );
    expect(onInput).toHaveBeenCalledWith("k");
    expect(onResize).not.toHaveBeenCalled();
  });

  it("移动端后台切换会取消已排队的输入焦点恢复", async () => {
    vi.useFakeTimers();
    const originalVisibilityState = Object.getOwnPropertyDescriptor(Document.prototype, "visibilityState");
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          mobileViewportHeight={460}
          mobileViewportOffsetTop={20}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();

      act(() => {
        terminalInput!.focus();
      });
      expect(document.activeElement).toBe(terminalInput);
      act(() => {
        terminalInput!.blur();
      });

      // 中文注释：先确认 blur 已经排入移动端短重试；随后真实切后台必须取消这些 timer，
      // 否则回前台时旧 timer 可能抢回终端焦点。
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        value: "hidden",
      });
      act(() => {
        window.dispatchEvent(new Event("blur"));
        document.dispatchEvent(new Event("visibilitychange"));
        vi.advanceTimersByTime(1000);
      });

      expect(document.activeElement).not.toBe(terminalInput);
      expect(onResize).not.toHaveBeenCalled();
    } finally {
      // 中文注释：本用例在 document 实例上覆写 visibilityState；必须先删掉实例属性，
      // 否则会继续遮蔽 Document.prototype，把后续用例误判成真实切后台。
      Reflect.deleteProperty(document, "visibilityState");
      if (originalVisibilityState) {
        Object.defineProperty(Document.prototype, "visibilityState", originalVisibilityState);
      } else {
        Reflect.deleteProperty(Document.prototype, "visibilityState");
      }
      vi.useRealTimers();
    }
  });

  it("移动端 visualViewport 还未更新时的 window blur 保持输入焦点状态", async () => {
    const onInput = vi.fn();
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    render(
      <TerminalPane
        attached
        sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={onInput}
        onResize={onResize}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    const blurSpy = vi.spyOn(terminalInput!, "blur");

    try {
      window.dispatchEvent(new Event("blur"));
      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, 40));
      });
      // 中文注释：visualViewport 尚未更新前的 window blur 是软键盘弹出/收起常见中间态。
      // 这里必须验证没有主动 blur helper textarea；只看最终 activeElement 会漏掉
      // “先 blur 再 refocus”的闪断，而真机会因此直接关掉系统键盘。
      expect(blurSpy).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(terminalInput);

      window.dispatchEvent(new Event("focus"));
      await act(async () => {
        await new Promise((resolve) => window.setTimeout(resolve, 40));
      });

      expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
      terminalInput!.dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: "m",
        }),
      );
      expect(onInput).toHaveBeenCalledWith("m");
      expect(onResize).not.toHaveBeenCalled();
    } finally {
      blurSpy.mockRestore();
    }
  });

  it("移动端 visualViewport 还未更新时 window blur 超过 settle 窗口也不主动 blur", () => {
    vi.useFakeTimers();
    try {
      const onResize = vi.fn();
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      render(
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />,
      );
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      const blurSpy = vi.spyOn(terminalInput!, "blur");

      act(() => {
        terminalInput!.focus();
      });
      expect(document.activeElement).toBe(terminalInput);

      act(() => {
        window.dispatchEvent(new Event("blur"));
        vi.advanceTimersByTime(200);
      });

      // 中文注释：真机软键盘弹出时 visualViewport/keyboardOpen 可能晚于 window.blur。
      // 即便超过 focusout settle 时间，也不能主动 blur helper textarea。
      expect(blurSpy).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(terminalInput);
      expect(onResize).not.toHaveBeenCalled();
      blurSpy.mockRestore();
    } finally {
      vi.useRealTimers();
    }
  });

  it("移动端焦点明确转到外部输入控件时仍允许终端失焦", async () => {
    const onResize = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    render(
      <>
        <TerminalPane
          attached
          sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          mobileViewportHeight={820}
          mobileViewportOffsetTop={0}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={onResize}
          onCursorChange={vi.fn()}
        />
        <input aria-label="outside input" />
      </>,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    const outsideInput = screen.getByRole("textbox", { name: "outside input" });
    expect(terminalInput).not.toBeNull();

    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    outsideInput.focus();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });

    // 中文注释：修复软键盘暂态失焦不能把终端焦点粘死；用户明确点到外部输入框时，
    // 终端必须释放输入 ownership。
    expect(document.activeElement).toBe(outsideInput);
  });

  it("移动端收起键盘导致输入框 blur 后不把可视高度写回 PTY", async () => {
    const onResize = vi.fn();
    const onTerminalResync = vi.fn();
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const { rerender } = render(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen
        mobileViewportHeight={460}
        mobileViewportOffsetTop={20}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    terminalInput!.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });
    onResize.mockClear();

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    rerender(
      <TerminalPane
        attached
        sessionSize={{ rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 }}
        mobileInputMode
        mobileKeyboardOpen={false}
        mobileViewportHeight={820}
        mobileViewportOffsetTop={0}
        outputResetVersion={0}
        takeOutput={takeOutput}
        registerOutputDrain={registerOutputDrain}
        onInput={vi.fn()}
        onResize={onResize}
        onTerminalResync={onTerminalResync}
        onCursorChange={vi.fn()}
      />,
    );

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, animationFrameMs * 4));
    });

    expect(onResize).not.toHaveBeenCalled();
    expect(onTerminalResync).not.toHaveBeenCalled();
  });

  it("分辨率不一致时也不显示缩放工具", async () => {
    const layout = mockTerminalLayout({ viewportWidth: 390, viewportHeight: 420 });
    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 30, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
      expect(terminalHostScale()).toBe(1);

      layout.setViewportHeight(260);
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 30, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
      expect(terminalHostScale()).toBe(1);
    } finally {
      layout.restore();
    }
  });
});
