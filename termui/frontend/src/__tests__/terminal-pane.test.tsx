import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TerminalPane, type TerminalOutputItem } from "../components/TerminalPane";
import type { SessionSearchResultPayload } from "../protocol/types";

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

function renderMobileTerminalPane(onInput = vi.fn()) {
  const takeOutput = vi.fn(() => []);
  const registerOutputDrain = vi.fn(() => () => undefined);
  render(
    <TerminalPane
      attached
      sessionSize={{ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }}
      mobileInputMode
      outputResetVersion={0}
      takeOutput={takeOutput}
      registerOutputDrain={registerOutputDrain}
      onInput={onInput}
      onResize={vi.fn()}
      onCursorChange={vi.fn()}
    />,
  );
  return {
    frame: screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame")!,
    onInput,
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
  it("TerminalPane 不直接绑定 Ghostty 私有 DOM 或具体 renderer import", () => {
    const source = readFileSync(resolve(process.cwd(), "src/components/TerminalPane.tsx"), "utf8");

    expect(source).not.toContain("@Ghostty/");
    const legacyGhosttyWrapperSelector = [".ghostty", "terminal"].join("-");
    expect(source).not.toContain(legacyGhosttyWrapperSelector);
    expect(source).not.toContain("Ghostty-helper-textarea");
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

  it("隐藏 Ghostty textarea 不重复暴露 Terminal input 可访问名称", async () => {
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
    // 但真实键盘与中文 IME 输入要落到 ghostty-web 的 textarea 输入 sink。
    await waitFor(() => expect(document.activeElement).toBe(textarea));
    expect(document.activeElement).not.toBe(terminalInput);
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

  it("连续 output frame 合并成批量 Ghostty write，但仍逐帧确认 terminal_seq", async () => {
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
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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

      // 中文注释：真实 Ghostty 某些渲染时序下，最后一笔 write 已解析但尚未 repaint。
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
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
        .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__ = true;
      const snapshot = Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      // 中文注释：刚 attach 时早期 resize/stabilize 可能已经在 snapshot 写完前执行过。
      // write callback 后仍必须再次贴底，否则用户会停在 snapshot 顶部附近。
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      act(() => {
        Ghostty?.scrollToLine(0);
      });
      expect(Ghostty?.viewportY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：点击空白处只应该聚焦终端；如果用户正在看历史，不能强行滚回最新输出。
      expect(Ghostty?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("拖拽选区会驱动 Ghostty selection 并触发复制", async () => {
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

  it("Ghostty scrollbar gutter click 不会把焦点抢回隐藏 textarea", async () => {
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

      expect(document.activeElement).not.toBe(terminalInput);
    } finally {
      rectSpy.mockRestore();
    }
  });

  it("自定义拖拽复制不依赖 Ghostty 返回的 selectionPosition", async () => {
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

  it("终端有 Ghostty 选区时 Ctrl+C 会优先走浏览器原生 copy 事务", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-shortcut-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-shortcut-line"));
    const Ghostty = (globalThis as {
      __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void };
    }).__TERMD_TEST_GHOSTTY__;
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
      Ghostty?.select("copy-shortcut-line");
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
    const Ghostty = (globalThis as {
      __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void };
    }).__TERMD_TEST_GHOSTTY__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const originalExecCommand = document.execCommand;
    const execCommandMock = vi.fn(() => false);
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: execCommandMock,
    });

    try {
      Ghostty?.select("copy-shortcut-fallback-line");
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

  it("浏览器 copy 事件会把 Ghostty 选区写入 clipboardData", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("copy-event-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("copy-event-line"));
    const Ghostty = (globalThis as {
      __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void };
    }).__TERMD_TEST_GHOSTTY__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;
    const setData = vi.fn();

    Ghostty?.select("copy-event-line");
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

  it("点击终端外后，旧的 Ghostty 选区不会继续劫持 Ctrl+C", async () => {
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
    const Ghostty = (globalThis as {
      __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void };
    }).__TERMD_TEST_GHOSTTY__;
    const clipboardWriteTextMock = navigator.clipboard.writeText as unknown as ReturnType<typeof vi.fn>;

    Ghostty?.select("copy-outside-line");
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

  it("已有 Ghostty 选区时，点击终端内其他位置会取消选区", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("selection-clear-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("selection-clear-line"));
    const Ghostty = (globalThis as {
      __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void };
    }).__TERMD_TEST_GHOSTTY__;
    const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    expect(frame).not.toBeNull();

    Ghostty?.select("selection-clear-line");
    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("true"));

    fireEvent.mouseDown(frame!, { clientX: 20, clientY: 20, button: 0 });
    fireEvent.click(frame!, { clientX: 20, clientY: 20, button: 0 });

    await waitFor(() => expect(terminalHost().dataset.termdHasSelection).toBe("false"));
    expect(terminalHost().dataset.termdSelection).toBe("");
  });

  it("selectionManager 持有的 Ghostty 选区也会在终端内点击后清掉", async () => {
    renderTerminalPaneWithOutput([
      { kind: "snapshot", bytes: new TextEncoder().encode("selection-manager-line\n"), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
    ]);

    await waitFor(() => expect(terminalHost().dataset.buffer).toContain("selection-manager-line"));
    const debugGhostty = (window as typeof window & {
      __TERMD_DEBUG_GHOSTTY__?: {
        selectViewportRange: (
          start: { col: number; row: number },
          end: { col: number; row: number },
        ) => string | undefined;
      };
    }).__TERMD_DEBUG_GHOSTTY__;
    expect(debugGhostty).toBeDefined();
    const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    expect(frame).not.toBeNull();

    const selection = debugGhostty?.selectViewportRange({ col: 0, row: 0 }, { col: 21, row: 0 });
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
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
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
      // 强制把本地 Ghostty 缩回远端尺寸，避免肉眼看到一次远端网格闪回。
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
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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
      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      queue = [{ kind: "output", bytes: encoder.encode("tail\n"), terminalSeq: 1 }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      act(() => {
        Ghostty?.scrollToLine(0);
      });
      expect(Ghostty?.viewportY()).toBe(0);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 10);
      });

      // 中文注释：用户已经开始查看历史后，后续排队的贴底帧只能在“当前仍在底部”时生效。
      expect(Ghostty?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("位于底部时纯 resize frame 也会重新贴到新的 PTY 底部", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
        .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__ = true;
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
      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
      const baseYBeforeResize = Ghostty?.baseY() ?? 0;

      queue = [{ kind: "resize", terminalSeq: 1, size: { rows: 12, cols: 80, pixel_width: 0, pixel_height: 0 } }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：resize frame 没有 output bytes；即使没有后续 write callback，也必须完成贴底。
      expect(Ghostty?.baseY()).toBeGreaterThan(baseYBeforeResize);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
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
      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);

      act(() => {
        Ghostty?.scrollToLine(0);
      });
      expect(Ghostty?.viewportY()).toBe(0);

      queue = [{ kind: "output", bytes: encoder.encode("tail\n"), terminalSeq: 1 }];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：用户已经上滚时，PTY 继续输出只能更新 buffer，不能把视口抢回底部。
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(0);
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      // 中文注释：scrollback 仍由 Ghostty 自己管理；termd 不再额外渲染 DOM 拖动条。
      expect(document.querySelector(".terminal-scroll-track")).toBeNull();
      expect(document.querySelector(".terminal-scroll-thumb")).toBeNull();
      expect(screen.queryByRole("button", { name: "Terminal scroll" })).toBeNull();

      act(() => {
        Ghostty?.scrollToLine(0);
      });
      expect(Ghostty?.viewportY()).toBe(0);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 2);
      });
      expect(document.querySelector(".terminal-scroll-track")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("全屏程序重绘导致 Ghostty 无 scrollback 时会在稳定窗口后主动请求一次 snapshot resync", async () => {
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBe(0);
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

  it("用户向上滚动但 Ghostty 无本地 scrollback 时会请求 supervisor snapshot history", async () => {
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      const wheelEvent = new WheelEvent("wheel", { deltaY: -900, bubbles: true, cancelable: true });
      frame!.dispatchEvent(wheelEvent);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：真实 supervisor attach 输出可能不让 Ghostty 本地形成 scrollback；
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

  it("用户向上滚动且 Ghostty 已有本地 scrollback 时会直接移动 viewport", async () => {
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.wheel(frame!, { deltaY: -320 });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：真实 Ghostty 有时不会自己消费 React wheel；termd 必须把
      // wheel 明确转换成 renderer-neutral scrollToLine，避免有 scrollback 也滚不动。
      expect(Ghostty?.viewportY()).toBeLessThan(Ghostty?.baseY() ?? 0);
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      for (let index = 0; index < 4; index += 1) {
        frame!.dispatchEvent(new WheelEvent("wheel", { deltaY: -4, bubbles: true, cancelable: true }));
      }

      await act(async () => {
        await vi.advanceTimersByTimeAsync(animationFrameMs * 4);
      });

      // 中文注释：像素级小 delta 以前会被逐次 trunc 掉；累积后至少应该滚动一行。
      expect(Ghostty?.viewportY()).toBeLessThan(Ghostty?.baseY() ?? 0);
      expect(onTerminalResync).not.toHaveBeenCalled();
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      // 中文注释：这次 snapshot 是用户向上滚触发的历史拉取；回来后应该直接看到历史，
      // 不能像普通 attach 一样又贴回底部，导致用户必须再滚第二次。
      expect(Ghostty?.viewportY()).toBeLessThan(Ghostty?.baseY() ?? 0);
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      // 中文注释：App 的 full snapshot 重连会重建 Ghostty；reveal intent 必须跟着
      // snapshot item 进入新实例，否则新实例会按普通 attach 自动贴底。
      expect(Ghostty?.viewportY()).toBeLessThan(Ghostty?.baseY() ?? 0);
    } finally {
      vi.useRealTimers();
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

      const firstGhostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(firstGhostty?.baseY()).toBeGreaterThan(0);
      expect(firstGhostty?.viewportY()).toBeLessThan(firstGhostty?.baseY() ?? 0);

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

      const secondGhostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      // 中文注释：revealHistory 只属于当前 snapshot。下一次普通 attach/theme/full snapshot
      // 必须恢复默认贴底，不能被上一轮用户上滚意图污染。
      expect(secondGhostty?.viewportY()).toBe(secondGhostty?.baseY());
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

      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      // 中文注释：用户上滚意图如果还没等到 snapshot 就遇到 reset，必须随旧 Ghostty buffer 失效。
      // 新 buffer 收到普通 snapshot 时仍要贴底，不能继承旧 ref 的“停在历史区”行为。
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
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
      const Ghostty = (globalThis as {
        __TERMD_TEST_GHOSTTY__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_GHOSTTY__;
      expect(Ghostty?.baseY()).toBeGreaterThan(0);
      expect(Ghostty?.viewportY()).toBe(Ghostty?.baseY());
    } finally {
      vi.useRealTimers();
    }
  });

  it("可见时排队的 Ghostty write 在 hidden 后会切到 timer fallback", async () => {
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

  it("可见时排队的 Ghostty write 在 blur 后会切到 timer fallback", async () => {
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

  it("窗口 blur 后新到的 Ghostty write 直接走 timer fallback", async () => {
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

  it("Ghostty write callback 在 blur 后被 rescue 时，后续 stdout 不会继续卡在 writeInFlight", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__ = true;
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

      // 中文注释：这里模拟的是真实 bug：上一个 Ghostty write 的 completion callback
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
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__?: boolean })
        .__TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__ = true;
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
      (globalThis as { __TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__?: boolean }).__TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__ = true;
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

  it("聚焦终端上报 resize 时先按本地可用高度撑开 Ghostty", async () => {
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
    // 否则 Ghostty 会长期停在默认 24 行，外层面板下方只剩大片空白。
    expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
  });

  it("窗口 blur 不会把本地 Ghostty 强制缩回远端尺寸，回焦时不经历远端网格闪回", async () => {
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
      (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs);
      });
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      expect(onResize).not.toHaveBeenCalled();

      // 中文注释：snapshot 字节写入期间不能改 Ghostty 尺寸；但用户的主动聚焦不能丢，
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
    terminalInput!.focus();
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

  it("移动端 visualViewport 高度变化但 keyboardOpen 不变时也重新上报尺寸", async () => {
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
        onCursorChange={vi.fn()}
      />,
    );

    await waitFor(() =>
      expect(onResize).toHaveBeenCalledWith({
        rows: 24,
        cols: 80,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      }),
    );
  });

  it("移动端收起键盘导致输入框 blur 后仍按恢复后的可视高度上报尺寸", async () => {
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
        onCursorChange={vi.fn()}
      />,
    );

    await waitFor(() =>
      expect(onResize).toHaveBeenCalledWith({
        rows: 24,
        cols: 80,
        pixel_width: expect.any(Number),
        pixel_height: expect.any(Number),
      }),
    );
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
