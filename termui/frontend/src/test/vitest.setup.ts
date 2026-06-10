import "@testing-library/jest-dom/vitest";
import "fake-indexeddb/auto";
import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import WebSocket from "ws";

// 单元测试在 Node/jsdom 中运行，使用 ws 提供真实 WebSocket，确保 DirectClient
// 测试仍经过完整 E2EE wire 流程，而不是绕过协议状态机。
Object.assign(globalThis, { WebSocket });
const clipboardWriteTextMock = vi.fn(() => Promise.resolve());
Object.defineProperty(globalThis.navigator, "clipboard", {
  configurable: true,
  get: () => ({ writeText: clipboardWriteTextMock }),
});

afterEach(() => {
  delete (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
    .__TERMD_TEST_FIT_DIMENSIONS__;
  delete (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_READY_AFTER_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_GHOSTTY_RENDER_READY_AFTER_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
    .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__;
  delete (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
    .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__;
  delete (globalThis as { __TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__?: boolean })
    .__TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__;
  delete (globalThis as { __TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__?: boolean })
    .__TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__;
  delete (globalThis as { __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void })
    .__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__;
  delete (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__?: { schedule: (callback: () => void) => number; cancel: (handle: number) => void };
  }).__TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__;
  delete (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: { schedule: (callback: () => void) => number; cancel: (handle: number) => void };
  }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
  delete (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: {
    writes: number;
    refreshes: number;
    writtenBytes: number;
    resizes: number;
    operations: Array<{ op: string; cols?: number; rows?: number; bytes?: number }>;
  } })
    .__TERMD_TEST_TERMINAL_STATS__;
  delete (globalThis as { __TERMD_TEST_GHOSTTY__?: { select: (text: string) => void; deselect: () => void } }).__TERMD_TEST_GHOSTTY__;
  delete (globalThis as {
    __TERMD_TEST_FORCE_SELECTION_POSITION__?: { start: { x: number; y: number }; end: { x: number; y: number } };
  }).__TERMD_TEST_FORCE_SELECTION_POSITION__;
  clipboardWriteTextMock.mockClear();
  cleanup();
});

vi.mock("ghostty-web", () => {
  const textDecoder = new TextDecoder();
  const mockCellMetrics = { width: 8, height: 16, baseline: 12 };
  const terminalByCanvas = new WeakMap<HTMLCanvasElement, {
    cols: number;
    rows: number;
    renderer?: { getMetrics?: () => { width: number; height: number; baseline: number } };
  }>();
  const scope = globalThis as typeof globalThis & {
    __TERMD_TEST_GHOSTTY_CANVAS_RECT_PATCHED__?: boolean;
  };
  if (!scope.__TERMD_TEST_GHOSTTY_CANVAS_RECT_PATCHED__) {
    scope.__TERMD_TEST_GHOSTTY_CANVAS_RECT_PATCHED__ = true;
    const originalGetBoundingClientRect = HTMLCanvasElement.prototype.getBoundingClientRect;
    HTMLCanvasElement.prototype.getBoundingClientRect = function getMockGhosttyCanvasRect() {
      const terminal = terminalByCanvas.get(this);
      if (!terminal) {
        return originalGetBoundingClientRect.call(this);
      }
      const metrics = terminal.renderer?.getMetrics?.() ?? mockCellMetrics;
      const width = terminal.cols * metrics.width;
      const height = terminal.rows * metrics.height;
      return {
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: width,
        bottom: height,
        width,
        height,
        toJSON() {
          return this;
        },
      } as DOMRect;
    };
  }

  function terminalStats() {
    const scope = globalThis as { __TERMD_TEST_TERMINAL_STATS__?: {
      writes: number;
      refreshes: number;
      writtenBytes: number;
      resizes: number;
      operations: Array<{ op: string; cols?: number; rows?: number; bytes?: number; text?: string }>;
    } };
    scope.__TERMD_TEST_TERMINAL_STATS__ ??= { writes: 0, refreshes: 0, writtenBytes: 0, resizes: 0, operations: [] };
    scope.__TERMD_TEST_TERMINAL_STATS__.resizes ??= 0;
    scope.__TERMD_TEST_TERMINAL_STATS__.operations ??= [];
    return scope.__TERMD_TEST_TERMINAL_STATS__;
  }

  class Terminal {
    private dataListeners: Array<(data: string) => void> = [];
    private cursorMoveListeners: Array<() => void> = [];
    private renderListeners: Array<() => void> = [];
    private scrollListeners: Array<(viewportY: number) => void> = [];
    private selectionChangeListeners: Array<() => void> = [];
    private terminalOptions: Record<string, unknown>;
    private pendingRender = "";
    private pendingRenderReady = false;
    private serializedWriteInFlight = false;
    private serializedWriteQueue: Array<{ data: string | Uint8Array; callback?: () => void }> = [];
    private selection = "";
    private selectionPosition: { start: { x: number; y: number }; end: { x: number; y: number } } | undefined;
    private absoluteCursorY = 0;
    private allLines: string[] = [""];
    public buffer = { active: { cursorY: 0, cursorX: 0, viewportY: 0, baseY: 0, length: 24 } };
    public wasmTerm = {
      scrollbackLength: 0,
      getScrollbackLength: () => this.wasmTerm.scrollbackLength,
      getScrollbackLine: (offset: number) => this.cellsForLine(this.allLines[offset] ?? ""),
      getLine: (row: number) => this.cellsForLine(this.allLines[this.wasmTerm.scrollbackLength + row] ?? ""),
      getGraphemeString: (row: number, col: number) => this.graphemeForLine(this.allLines[this.wasmTerm.scrollbackLength + row] ?? "", col),
      getScrollbackGraphemeString: (offset: number, col: number) => this.graphemeForLine(this.allLines[offset] ?? "", col),
    };
    public cols = 80;
    public rows = 24;
    public renderer = {
      getMetrics: () => {
        const fitDimensions = (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
          .__TERMD_TEST_FIT_DIMENSIONS__;
        if (!this.element || !fitDimensions) {
          return mockCellMetrics;
        }
        // 中文注释：生产 Ghostty 的 fit 从 host 像素和字体 metrics 计算 rows/cols。
        // 测试仍用 __TERMD_TEST_FIT_DIMENSIONS__ 表达目标行列，但这里把它反推成稳定 metrics，
        // 避免 daemon resize 路径退回不可信 fallback。
        return {
          width: Math.max(1, this.element.clientWidth / fitDimensions.cols),
          height: Math.max(1, this.element.clientHeight / fitDimensions.rows),
          baseline: mockCellMetrics.baseline,
        };
      },
      render: () => undefined,
    };
    public viewportY = 0;
    public element: HTMLDivElement | undefined;
    public textarea: HTMLTextAreaElement | undefined;

    constructor(options: Record<string, unknown> = {}) {
      this.terminalOptions = options;
    }

    get options() {
      return { ...this.terminalOptions, cols: this.cols, rows: this.rows };
    }

    set options(nextOptions: Record<string, unknown>) {
      // 真实 Ghostty 不允许在运行期重新设置 cols/rows；测试里保留这个约束，避免缩放时误写只读配置。
      if ("cols" in nextOptions || "rows" in nextOptions) {
        throw new Error('Option "cols" can only be set in the constructor');
      }
      this.terminalOptions = { ...this.terminalOptions, ...nextOptions };
    }

    open(element: HTMLElement) {
      this.element = element as HTMLDivElement;
      this.element.dataset.buffer = "";
      this.element.dataset.termdBuffer = "";
      this.element.tabIndex = 0;
      this.element.setAttribute("contenteditable", "true");
      this.element.setAttribute("role", "textbox");
      this.element.setAttribute("aria-label", "Terminal input");
      this.element.setAttribute("aria-multiline", "true");
      if (this.element.clientWidth <= 0 && this.element.clientHeight <= 0) {
        Object.defineProperty(this.element, "clientWidth", {
          configurable: true,
          get: () =>
            ((globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
              .__TERMD_TEST_FIT_DIMENSIONS__?.cols ?? this.cols) * mockCellMetrics.width,
        });
        Object.defineProperty(this.element, "clientHeight", {
          configurable: true,
          get: () =>
            ((globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
              .__TERMD_TEST_FIT_DIMENSIONS__?.rows ?? this.rows) * mockCellMetrics.height,
        });
      }
      const canvas = document.createElement("canvas");
      terminalByCanvas.set(canvas, this);
      const textarea = document.createElement("textarea");
      textarea.setAttribute("aria-label", "Terminal input");
      textarea.addEventListener("input", () => {
        const value = textarea.value;
        textarea.value = "";
        this.buffer.active.cursorX += value.length;
        this.dataListeners.forEach((listener) => listener(value));
        this.cursorMoveListeners.forEach((listener) => listener());
      });
      this.textarea = textarea;
      this.element.append(canvas, textarea);
      (globalThis as { __TERMD_TEST_GHOSTTY__?: {
        select: (text: string) => void;
        deselect: () => void;
        viewportY: () => number;
        baseY: () => number;
        scrollToLine: (line: number) => void;
        forceSelectionPosition: (position: { start: { x: number; y: number }; end: { x: number; y: number } } | undefined) => void;
      } }).__TERMD_TEST_GHOSTTY__ = {
        select: (text: string) => {
          this.selection = text;
          this.selectionPosition = {
            start: { x: 0, y: 0 },
            end: { x: Math.max(0, text.length - 1), y: 0 },
          };
          this.selectionChangeListeners.forEach((listener) => listener());
        },
        deselect: () => {
          this.selection = "";
          this.selectionPosition = undefined;
          this.selectionChangeListeners.forEach((listener) => listener());
        },
        viewportY: () => Math.max(0, this.wasmTerm.scrollbackLength - this.viewportY),
        baseY: () => this.wasmTerm.scrollbackLength,
        scrollToLine: (line: number) => this.scrollToRawLine(Math.max(0, this.wasmTerm.scrollbackLength - line)),
        forceSelectionPosition: (position) => {
          this.selectionPosition = position;
        },
      };
    }

    loadAddon(addon?: { activate?: (terminal: Terminal) => void }) {
      addon?.activate?.(this);
    }

    private syncCursorWindow() {
      const baseY = Math.max(0, this.absoluteCursorY - this.rows + 1);
      this.buffer.active.baseY = baseY;
      this.buffer.active.length = baseY + this.rows;
      this.wasmTerm.scrollbackLength = baseY;
      // 中文注释：真实终端 renderer 的公开 cursorY 表示当前 screen 内的光标行，
      // 不是累计 scrollback 行号；测试桩也要保持这个语义，才能覆盖 resize 后
      // “内容已经铺满，但光标还停在旧高度”的真实浏览器问题。
      this.buffer.active.cursorY = Math.max(0, this.absoluteCursorY - this.buffer.active.baseY);
    }

    private appendBufferText(text: string) {
      // 中文注释：测试桩需要维护和 Ghostty wasmTerm 类似的文本行，否则自定义选区复制
      // 会退回 DOM debug mirror，无法覆盖生产路径。
      const normalized = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
      for (const char of normalized) {
        if (char === "\n") {
          this.allLines.push("");
        } else {
          this.allLines[this.allLines.length - 1] = `${this.allLines.at(-1) ?? ""}${char}`;
        }
      }
    }

    private cellsForLine(line: string) {
      const cells = Array.from(line, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 }));
      while (cells.length < this.cols) {
        cells.push({ codepoint: 0, grapheme_len: 0 });
      }
      return cells;
    }

    private graphemeForLine(line: string, col: number) {
      return Array.from(line)[col] ?? "";
    }

    write(data: string | Uint8Array, callback?: () => void) {
      const serializeWrites = Boolean(
        (globalThis as { __TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__?: boolean })
          .__TERMD_TEST_SERIALIZE_GHOSTTY_WRITES__,
      );
      if (serializeWrites && this.serializedWriteInFlight) {
        this.serializedWriteQueue.push({ data, callback });
        return;
      }
      if (serializeWrites) {
        this.serializedWriteInFlight = true;
      }
      this.processWrite(data, () => {
        callback?.();
        if (!serializeWrites) {
          return;
        }
        this.serializedWriteInFlight = false;
        const next = this.serializedWriteQueue.shift();
        if (next) {
          this.write(next.data, next.callback);
        }
      });
    }

    private processWrite(data: string | Uint8Array, callback?: () => void) {
      const text = typeof data === "string" ? data : textDecoder.decode(data);
      const stats = terminalStats();
      stats.writes += 1;
      stats.writtenBytes += typeof data === "string" ? data.length : data.byteLength;
      stats.operations.push({
        op: "write",
        bytes: typeof data === "string" ? data.length : data.byteLength,
        text: text.length <= 32 ? text : undefined,
      });
      const deferRender = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_GHOSTTY_RENDER_UNTIL_WRITE_CALLBACK__,
      );
      const deferRenderReadyAfterCallback = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_RENDER_READY_AFTER_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_GHOSTTY_RENDER_READY_AFTER_WRITE_CALLBACK__,
      );
      const deferBufferUntilCallback = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_GHOSTTY_BUFFER_UNTIL_WRITE_CALLBACK__,
      );
      const suppressWriteCallback = Boolean(
        (globalThis as { __TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_SUPPRESS_GHOSTTY_WRITE_CALLBACK__,
      );
      const keepViewportAtTop = Boolean(
        (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
          .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_WRITE__,
      );
      if (deferRender) {
        this.pendingRender += text;
        this.pendingRenderReady = false;
      } else {
        this.renderData(text);
      }
      const applyBufferEffects = () => {
        this.appendBufferText(text);
        const lines = text.split("\n");
        const lastLine = lines[lines.length - 1] ?? "";
        const previousBaseY = this.buffer.active.baseY;
        const wasAtBottom = this.buffer.active.viewportY >= previousBaseY;
        if (text.includes("\n")) {
          this.absoluteCursorY += lines.length - 1;
          this.buffer.active.cursorX = lastLine.length;
        } else {
          this.buffer.active.cursorX += text.length;
        }
        this.syncCursorWindow();
        const nextViewportY = keepViewportAtTop
          ? Math.min(this.buffer.active.viewportY, this.buffer.active.baseY)
            : wasAtBottom
              ? this.buffer.active.baseY
              : Math.min(this.buffer.active.viewportY, this.buffer.active.baseY);
        this.buffer.active.viewportY = nextViewportY;
        this.viewportY = Math.max(0, this.wasmTerm.scrollbackLength - nextViewportY);
        this.renderListeners.forEach((listener) => listener());
        this.cursorMoveListeners.forEach((listener) => listener());
        this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
      };
      if (!deferBufferUntilCallback) {
        applyBufferEffects();
      }
      if (!deferRender) {
        if (deferBufferUntilCallback) {
          applyBufferEffects();
        }
        if (!suppressWriteCallback) {
          callback?.();
        }
        return;
      }

      // 真实 Ghostty canvas 会异步完成 write 解析和绘制；这里延后三帧，让测试能覆盖
      // “write 后立即 refresh 但内容尚不可绘制”的浏览器时序。
      window.requestAnimationFrame(() => {
        window.requestAnimationFrame(() => {
          window.requestAnimationFrame(() => {
            if (deferRenderReadyAfterCallback) {
              if (deferBufferUntilCallback) {
                applyBufferEffects();
              }
              callback?.();
              window.requestAnimationFrame(() => {
                this.pendingRenderReady = true;
              });
              return;
            }
            this.pendingRenderReady = true;
            if (deferBufferUntilCallback) {
              applyBufferEffects();
            }
            if (!suppressWriteCallback) {
              callback?.();
            }
          });
        });
      });
    }

    onData(listener: (data: string) => void) {
      this.dataListeners.push(listener);
      return { dispose: () => undefined };
    }

    onCursorMove(listener: () => void) {
      this.cursorMoveListeners.push(listener);
      return { dispose: () => undefined };
    }

    onRender(listener: () => void) {
      this.renderListeners.push(listener);
      return { dispose: () => undefined };
    }

    onScroll(listener: (viewportY: number) => void) {
      this.scrollListeners.push(listener);
      return { dispose: () => undefined };
    }

    onSelectionChange(listener: () => void) {
      this.selectionChangeListeners.push(listener);
      return { dispose: () => undefined };
    }

    hasSelection() {
      return this.selection.length > 0;
    }

    getSelection() {
      return this.selection;
    }

    select(column: number, row: number, length: number) {
      this.selection = "x".repeat(Math.max(0, length));
      const forcedSelectionPosition = (globalThis as {
        __TERMD_TEST_FORCE_SELECTION_POSITION__?: { start: { x: number; y: number }; end: { x: number; y: number } };
      }).__TERMD_TEST_FORCE_SELECTION_POSITION__;
      this.selectionPosition = forcedSelectionPosition ?? {
        start: { x: column, y: row },
        end: { x: column + Math.max(0, length - 1), y: row },
      };
      this.selectionChangeListeners.forEach((listener) => listener());
    }

    deselect() {
      this.selection = "";
      this.selectionPosition = undefined;
      this.selectionChangeListeners.forEach((listener) => listener());
    }

    private scrollToRawLine(line: number) {
      this.viewportY = Math.max(0, Math.min(this.wasmTerm.scrollbackLength, line));
      this.buffer.active.viewportY = Math.max(0, this.wasmTerm.scrollbackLength - this.viewportY);
      this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
    }

    scrollToLine(line: number) {
      this.scrollToRawLine(line);
    }

    resize(cols: number, rows: number) {
      const stats = terminalStats();
      stats.resizes += 1;
      stats.operations.push({ op: "resize", cols, rows });
      const previousBaseY = this.buffer.active.baseY;
      const wasAtBottom = this.buffer.active.viewportY >= previousBaseY;
      const keepViewportAtTop = Boolean(
        (globalThis as { __TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
          .__TERMD_TEST_KEEP_GHOSTTY_VIEWPORT_AT_TOP_AFTER_RESIZE__,
      );
      this.cols = cols;
      this.rows = rows;
      this.syncCursorWindow();
      const nextViewportY = keepViewportAtTop
        ? Math.min(this.buffer.active.viewportY, this.buffer.active.baseY)
        : wasAtBottom
        ? this.buffer.active.baseY
        : Math.min(this.buffer.active.viewportY, this.buffer.active.baseY);
      this.buffer.active.viewportY = nextViewportY;
      this.viewportY = Math.max(0, this.wasmTerm.scrollbackLength - nextViewportY);
    }

    focus() {
      if ((globalThis as { __TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__?: boolean }).__TERMD_TEST_GHOSTTY_SKIP_NATIVE_FOCUS__) {
        return;
      }
      this.element?.focus();
    }

    requestRender() {
      terminalStats().refreshes += 1;
      if (!this.pendingRenderReady || !this.pendingRender) {
        return;
      }
      const data = this.pendingRender;
      this.pendingRender = "";
      this.pendingRenderReady = false;
      this.renderData(data);
    }

    clear() {
      terminalStats().operations.push({ op: "clear" });
      // mock 要真实清空 DOM buffer，才能覆盖 WebSocket 断线重连后的 snapshot 重放不会叠加旧内容。
      this.pendingRender = "";
      this.pendingRenderReady = false;
      if (!this.element) {
        return;
      }
      this.element.dataset.buffer = "";
      this.element.dataset.termdBuffer = "";
      for (const node of Array.from(this.element.childNodes)) {
        if (node.nodeType === Node.TEXT_NODE) {
          node.remove();
        }
      }
      this.absoluteCursorY = 0;
      this.allLines = [""];
      this.buffer.active.cursorY = 0;
      this.buffer.active.cursorX = 0;
      this.buffer.active.baseY = 0;
      this.buffer.active.viewportY = 0;
      this.buffer.active.length = this.rows;
      this.viewportY = 0;
      this.wasmTerm.scrollbackLength = 0;
    }

    reset() {
      terminalStats().operations.push({ op: "reset" });
      // Ghostty reset 会清空终端状态；测试 mock 复用 clear 的 DOM/游标重置逻辑即可。
      this.clear();
    }

    dispose() {
      if (!this.element) {
        return;
      }
      this.element.replaceChildren();
      this.element.removeAttribute("tabindex");
      this.element.removeAttribute("contenteditable");
      this.element.removeAttribute("role");
      this.element.removeAttribute("aria-label");
      this.element.removeAttribute("aria-multiline");
      delete this.element.dataset.buffer;
      delete this.element.dataset.termdBuffer;
      this.textarea = undefined;
      this.element = undefined;
    }

    private renderData(data: string) {
      if (!this.element) {
        return;
      }
      this.element.dataset.buffer = `${this.element.dataset.buffer ?? ""}${data}`;
      this.element.dataset.termdBuffer = this.element.dataset.buffer;
      this.element.append(document.createTextNode(data));
    }
  }

  class FitAddon {
    private terminal?: { resize: (cols: number, rows: number) => void };

    activate(terminal: { resize: (cols: number, rows: number) => void }) {
      this.terminal = terminal;
    }

    fit() {
      const proposed = this.proposeDimensions();
      // Ghostty 的 FitAddon.fit 会把终端尺寸同步成 proposeDimensions 的结果；
      // mock 也要这样做，才能覆盖 resize ack 后回聚焦的滚动保持逻辑。
      this.terminal?.resize(proposed.cols, proposed.rows);
    }

    proposeDimensions() {
      // 测试用例可显式覆盖 Ghostty 当前容器能容纳的尺寸，用来模拟浏览器窗口 resize。
      return (
        (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
          .__TERMD_TEST_FIT_DIMENSIONS__ ?? { rows: 24, cols: 80 }
      );
    }
  }

  return {
    init: vi.fn(() => undefined),
    Terminal,
    FitAddon,
  };
});
