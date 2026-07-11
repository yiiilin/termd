import "@testing-library/jest-dom/vitest";
import "fake-indexeddb/auto";
import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import WebSocket from "ws";

// 单元测试在 Node/jsdom 中运行，使用 ws 提供真实 WebSocket，确保 DirectClient
// 测试经过当前明文 WebSocket wire 和协议状态机；HTTP E2EE 兼容路径另行测试。
Object.assign(globalThis, { WebSocket });

const nativeFetch = globalThis.fetch?.bind(globalThis);

if (nativeFetch) {
  (globalThis as typeof globalThis & { fetch: typeof fetch }).fetch = (async (input, init) => {
    const requestUrl = new URL(input instanceof Request ? input.url : String(input));
    const registry = globalThis as typeof globalThis & {
      __TERMD_TEST_HTTP_DAEMONS__?: Map<string, { handleHttpControlRequest: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response> }>;
    };
    const daemon = registry.__TERMD_TEST_HTTP_DAEMONS__?.get(requestUrl.origin);
    if (daemon && /\/api\/control\//u.test(requestUrl.pathname)) {
      const signal = init?.signal ?? (input instanceof Request ? input.signal : undefined);
      if (signal?.aborted) {
        throw new DOMException("The operation was aborted", "AbortError");
      }
      return await Promise.race([
        daemon.handleHttpControlRequest(input, init),
        new Promise<Response>((_, reject) => {
          if (!signal) {
            return;
          }
          const onAbort = () => {
            signal.removeEventListener("abort", onAbort);
            reject(new DOMException("The operation was aborted", "AbortError"));
          };
          signal.addEventListener("abort", onAbort, { once: true });
        }),
      ]);
    }
    return nativeFetch(input as RequestInfo | URL, init);
  }) as typeof fetch;
}

const clipboardWriteTextMock = vi.fn(() => Promise.resolve());
Object.defineProperty(globalThis.navigator, "clipboard", {
  configurable: true,
  get: () => ({ writeText: clipboardWriteTextMock }),
});

type TerminalStats = {
  writes: number;
  refreshes: number;
  writtenBytes: number;
  resizes: number;
  operations: Array<{ op: string; cols?: number; rows?: number; bytes?: number; text?: string }>;
};

type TerminalSelectionPosition = {
  start: { x: number; y: number };
  end: { x: number; y: number };
};

type MockTerminalControl = {
  select: (text: string) => void;
  deselect: () => void;
  viewportY: () => number;
  baseY: () => number;
  scrollToLine: (line: number) => void;
  forceCursorPosition: (cursorY: number) => void;
  forceSelectionPosition: (position: TerminalSelectionPosition | undefined) => void;
};

function terminalStats(): TerminalStats {
  const scope = globalThis as { __TERMD_TEST_TERMINAL_STATS__?: TerminalStats };
  scope.__TERMD_TEST_TERMINAL_STATS__ ??= {
    writes: 0,
    refreshes: 0,
    writtenBytes: 0,
    resizes: 0,
    operations: [],
  };
  scope.__TERMD_TEST_TERMINAL_STATS__.resizes ??= 0;
  scope.__TERMD_TEST_TERMINAL_STATS__.operations ??= [];
  return scope.__TERMD_TEST_TERMINAL_STATS__;
}

afterEach(() => {
  delete (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
    .__TERMD_TEST_FIT_DIMENSIONS__;
  delete (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
    .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__;
  delete (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
    .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__;
  delete (globalThis as { __TERMD_TEST_SERIALIZE_TERMINAL_WRITES__?: boolean })
    .__TERMD_TEST_SERIALIZE_TERMINAL_WRITES__;
  delete (globalThis as { __TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__?: boolean })
    .__TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__;
  delete (globalThis as { __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void })
    .__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__;
  delete (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__?: {
      schedule: (callback: () => void) => number;
      cancel: (handle: number) => void;
    };
  }).__TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__;
  delete (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__?: {
      schedule: (callback: () => void) => number;
      cancel: (handle: number) => void;
    };
  }).__TERMD_TEST_HOLD_TERMINAL_WRITE_RAF__;
  delete (globalThis as { __TERMD_TEST_TERMINAL_STATS__?: TerminalStats }).__TERMD_TEST_TERMINAL_STATS__;
  delete (globalThis as { __TERMD_TEST_TERMINAL__?: MockTerminalControl }).__TERMD_TEST_TERMINAL__;
  delete (globalThis as { __TERMD_TEST_FORCE_SELECTION_POSITION__?: TerminalSelectionPosition })
    .__TERMD_TEST_FORCE_SELECTION_POSITION__;
  clipboardWriteTextMock.mockClear();
  cleanup();
});

vi.mock("@xterm/xterm", () => {
  const textDecoder = new TextDecoder();
  const mockCellMetrics = { width: 8, height: 16, baseline: 12 };
  const terminalByCanvas = new WeakMap<HTMLCanvasElement, MockTerminal>();
  const scope = globalThis as typeof globalThis & {
    __TERMD_TEST_TERMINAL_CANVAS_RECT_PATCHED__?: boolean;
  };
  if (!scope.__TERMD_TEST_TERMINAL_CANVAS_RECT_PATCHED__) {
    scope.__TERMD_TEST_TERMINAL_CANVAS_RECT_PATCHED__ = true;
    const originalGetBoundingClientRect = HTMLCanvasElement.prototype.getBoundingClientRect;
    HTMLCanvasElement.prototype.getBoundingClientRect = function getMockTerminalCanvasRect() {
      const terminal = terminalByCanvas.get(this);
      if (!terminal) {
        return originalGetBoundingClientRect.call(this);
      }
      const width = terminal.cols * mockCellMetrics.width;
      const height = terminal.rows * mockCellMetrics.height;
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

  class MockBufferLine {
    constructor(private readonly text: string) {}

    translateToString(trimRight = false, startColumn = 0, endColumn = this.text.length): string {
      const slice = this.text.slice(startColumn, endColumn);
      return trimRight ? slice.replace(/\s+$/u, "") : slice;
    }
  }

  class MockTerminal {
    private dataListeners: Array<(data: string) => void> = [];
    private cursorMoveListeners: Array<() => void> = [];
    private renderListeners: Array<() => void> = [];
    private scrollListeners: Array<(viewportY: number) => void> = [];
    private selectionChangeListeners: Array<() => void> = [];
    private writeParsedListeners: Array<() => void> = [];
    private optionsRecord: Record<string, unknown>;
    private pendingRender = "";
    private pendingRenderReady = false;
    private serializedWriteInFlight = false;
    private serializedWriteQueue: Array<{ data: string | Uint8Array; callback?: () => void }> = [];
    private selection = "";
    private selectionPosition: { start: { x: number; y: number }; end: { x: number; y: number } } | undefined;
    private absoluteCursorY = 0;
    private allLines: string[] = [""];
    private xtermWrapper?: HTMLDivElement;
    public cols = 80;
    public rows = 24;
    public element: HTMLDivElement | undefined;
    public textarea: HTMLTextAreaElement | undefined;
    public buffer = {
      active: {
        cursorY: 0,
        cursorX: 0,
        viewportY: 0,
        baseY: 0,
        length: 24,
        getLine: (row: number) => this.getBufferLine(row),
      },
    };

    constructor(options: Record<string, unknown> = {}) {
      this.optionsRecord = options;
      const seededCols = typeof options.cols === "number" ? options.cols : undefined;
      const seededRows = typeof options.rows === "number" ? options.rows : undefined;
      if (seededCols && seededRows) {
        this.cols = seededCols;
        this.rows = seededRows;
        this.buffer.active.length = seededRows;
      }
    }

    get options() {
      return this.optionsRecord;
    }

    set options(nextOptions: Record<string, unknown>) {
      // 中文注释：xterm.js v6 把 rows/cols 视为构造期选项，运行期写入会抛错；
      // mock 必须保持这个约束，才能覆盖 theme/font 热更新时的真实浏览器异常。
      if (Object.hasOwn(nextOptions, "cols")) {
        throw new Error('Option "cols" can only be set in the constructor');
      }
      if (Object.hasOwn(nextOptions, "rows")) {
        throw new Error('Option "rows" can only be set in the constructor');
      }
      this.optionsRecord = { ...this.optionsRecord, ...nextOptions };
    }

    loadAddon(addon?: { activate?: (terminal: MockTerminal) => void }) {
      addon?.activate?.(this);
    }

    open(parent: HTMLElement) {
      this.element = parent as HTMLDivElement;
      this.element.dataset.buffer = "";
      this.element.dataset.termdBuffer = "";
      this.element.tabIndex = 0;
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
      this.xtermWrapper = document.createElement("div");
      this.xtermWrapper.className = "xterm";
      const viewport = document.createElement("div");
      viewport.className = "xterm-viewport";
      const screen = document.createElement("div");
      screen.className = "xterm-screen";
      const canvas = document.createElement("canvas");
      terminalByCanvas.set(canvas, this);
      screen.append(canvas);
      const textarea = document.createElement("textarea");
      textarea.className = "xterm-helper-textarea";
      textarea.setAttribute("aria-label", "Terminal input");
      textarea.addEventListener("input", () => {
        const value = textarea.value;
        textarea.value = "";
        this.buffer.active.cursorX += value.length;
        this.dataListeners.forEach((listener) => listener(value));
        this.cursorMoveListeners.forEach((listener) => listener());
      });
      this.textarea = textarea;
      this.xtermWrapper.append(viewport, screen, textarea);
      this.element.append(this.xtermWrapper);

      (globalThis as { __TERMD_TEST_TERMINAL__?: MockTerminalControl }).__TERMD_TEST_TERMINAL__ = {
        select: (text) => {
          this.selection = text;
          this.selectionPosition = {
            start: { x: 1, y: 1 },
            end: { x: Math.max(1, text.length), y: 1 },
          };
          this.selectionChangeListeners.forEach((listener) => listener());
        },
        deselect: () => this.clearSelection(),
        viewportY: () => this.buffer.active.viewportY,
        baseY: () => this.buffer.active.baseY,
        scrollToLine: (line) => this.scrollToLine(line),
        forceCursorPosition: (cursorY) => {
          this.absoluteCursorY = Math.max(
            0,
            Math.min(this.buffer.active.length - 1, this.buffer.active.baseY + cursorY),
          );
          this.syncCursorWindow();
          this.cursorMoveListeners.forEach((listener) => listener());
          this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
        },
        forceSelectionPosition: (position) => {
          this.selectionPosition = position
            ? {
                start: { x: position.start.x + 1, y: position.start.y + 1 },
                end: { x: position.end.x + 1, y: position.end.y + 1 },
              }
            : undefined;
        },
      };
    }

    write(data: string | Uint8Array, callback?: () => void) {
      const serializeWrites = Boolean(
        (globalThis as { __TERMD_TEST_SERIALIZE_TERMINAL_WRITES__?: boolean })
          .__TERMD_TEST_SERIALIZE_TERMINAL_WRITES__,
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
      const text = typeof data === "string" ? data : this.decodeWriteText(data);
      const stats = terminalStats();
      stats.writes += 1;
      stats.writtenBytes += typeof data === "string" ? data.length : data.byteLength;
      stats.operations.push({
        op: "write",
        bytes: typeof data === "string" ? data.length : data.byteLength,
        text: text.length <= 32 ? text : undefined,
      });
      const deferRender = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__,
      );
      const deferRenderReadyAfterCallback = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__,
      );
      const deferBufferUntilCallback = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_TERMINAL_BUFFER_UNTIL_WRITE_CALLBACK__,
      );
      const suppressWriteCallback = Boolean(
        (globalThis as { __TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_SUPPRESS_TERMINAL_WRITE_CALLBACK__,
      );
      const keepViewportAtTop = Boolean(
        (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
          .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_WRITE__,
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
        this.buffer.active.viewportY = keepViewportAtTop
          ? Math.min(this.buffer.active.viewportY, this.buffer.active.baseY)
          : wasAtBottom
            ? this.buffer.active.baseY
            : Math.min(this.buffer.active.viewportY, this.buffer.active.baseY);
        this.renderListeners.forEach((listener) => listener());
        this.cursorMoveListeners.forEach((listener) => listener());
        this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
        this.writeParsedListeners.forEach((listener) => listener());
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

      // 中文注释：测试桩同样把解析/绘制拆到多帧，覆盖“write callback 晚于首轮布局”的竞态。
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
                this.requestRender();
              });
              return;
            }
            this.pendingRenderReady = true;
            this.requestRender();
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

    onWriteParsed(listener: () => void) {
      this.writeParsedListeners.push(listener);
      return { dispose: () => undefined };
    }

    hasSelection() {
      return this.selection.length > 0;
    }

    getSelection() {
      return this.selection;
    }

    getSelectionPosition() {
      return this.selectionPosition;
    }

    clearSelection() {
      this.selection = "";
      this.selectionPosition = undefined;
      this.selectionChangeListeners.forEach((listener) => listener());
    }

    select(column: number, row: number, length: number) {
      const forcedSelectionPosition = (globalThis as {
        __TERMD_TEST_FORCE_SELECTION_POSITION__?: TerminalSelectionPosition;
      }).__TERMD_TEST_FORCE_SELECTION_POSITION__;
      const range = this.rangeFromLinearSelection(column, row, length);
      this.selection = this.bufferRangeText(range);
      this.selectionPosition = forcedSelectionPosition
        ? {
            start: { x: forcedSelectionPosition.start.x + 1, y: forcedSelectionPosition.start.y + 1 },
            end: { x: forcedSelectionPosition.end.x + 1, y: forcedSelectionPosition.end.y + 1 },
          }
        : {
            start: { x: range.startCol + 1, y: range.startRow + 1 },
            end: { x: range.endCol + 1, y: range.endRow + 1 },
          };
      this.selectionChangeListeners.forEach((listener) => listener());
    }

    selectLines(start: number, end: number) {
      const startRow = Math.max(0, start);
      const endRow = Math.max(startRow, end);
      this.selection = this.bufferRangeText({
        startCol: 0,
        startRow,
        endCol: Math.max(0, this.cols - 1),
        endRow,
      });
      this.selectionPosition = {
        start: { x: 1, y: startRow + 1 },
        end: { x: this.cols, y: endRow + 1 },
      };
      this.selectionChangeListeners.forEach((listener) => listener());
    }

    scrollToLine(line: number) {
      this.buffer.active.viewportY = Math.max(0, Math.min(this.buffer.active.baseY, Math.floor(line)));
      this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
    }

    resize(cols: number, rows: number) {
      const stats = terminalStats();
      stats.resizes += 1;
      stats.operations.push({ op: "resize", cols, rows });
      const previousBaseY = this.buffer.active.baseY;
      const wasAtBottom = this.buffer.active.viewportY >= previousBaseY;
      const keepViewportAtTop = Boolean(
        (globalThis as { __TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
          .__TERMD_TEST_KEEP_TERMINAL_VIEWPORT_AT_TOP_AFTER_RESIZE__,
      );
      this.cols = cols;
      this.rows = rows;
      this.syncCursorWindow();
      this.buffer.active.viewportY = keepViewportAtTop
        ? Math.min(this.buffer.active.viewportY, this.buffer.active.baseY)
        : wasAtBottom
          ? this.buffer.active.baseY
          : Math.min(this.buffer.active.viewportY, this.buffer.active.baseY);
    }

    focus() {
      if ((globalThis as { __TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__?: boolean }).__TERMD_TEST_TERMINAL_SKIP_NATIVE_FOCUS__) {
        return;
      }
      this.textarea?.focus();
    }

    refresh() {
      terminalStats().refreshes += 1;
      this.renderListeners.forEach((listener) => listener());
      this.writeParsedListeners.forEach((listener) => listener());
    }

    reset() {
      terminalStats().operations.push({ op: "reset" });
      const shouldBlurOnReset = Boolean(
        (globalThis as { __TERMD_TEST_TERMINAL_BLUR_ON_RESET__?: boolean })
          .__TERMD_TEST_TERMINAL_BLUR_ON_RESET__,
      );
      this.pendingRender = "";
      this.pendingRenderReady = false;
      this.selection = "";
      this.selectionPosition = undefined;
      this.absoluteCursorY = 0;
      this.allLines = [""];
      this.buffer.active.cursorY = 0;
      this.buffer.active.cursorX = 0;
      this.buffer.active.baseY = 0;
      this.buffer.active.viewportY = 0;
      this.buffer.active.length = this.rows;
      if (this.element) {
        this.element.dataset.buffer = "";
        this.element.dataset.termdBuffer = "";
        for (const node of Array.from(this.element.childNodes)) {
          if (node.nodeType === Node.TEXT_NODE) {
            node.remove();
          }
        }
      }
      if (shouldBlurOnReset && document.activeElement === this.textarea) {
        // 中文注释：真实 renderer/browser 在 reset/reflow 期间可能短暂丢掉 helper textarea 焦点；
        // 测试用这个开关稳定复现打开终端后“闪一下导致失焦”的时序。
        this.textarea.blur();
      }
    }

    dispose() {
      this.element?.replaceChildren();
      this.element?.removeAttribute("tabindex");
      this.element?.removeAttribute("role");
      this.element?.removeAttribute("aria-label");
      this.element?.removeAttribute("aria-multiline");
      if (this.element) {
        delete this.element.dataset.buffer;
        delete this.element.dataset.termdBuffer;
      }
      this.textarea = undefined;
      this.element = undefined;
    }

    private syncCursorWindow() {
      const lineCount = Math.max(1, this.allLines.length);
      this.buffer.active.length = Math.max(this.rows, lineCount);
      this.buffer.active.baseY = Math.max(0, this.buffer.active.length - this.rows);
      this.buffer.active.cursorY = Math.max(0, this.absoluteCursorY - this.buffer.active.baseY);
    }

    private appendBufferText(text: string) {
      const normalized = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
      for (const char of normalized) {
        if (char === "\n") {
          this.allLines.push("");
        } else {
          this.allLines[this.allLines.length - 1] = `${this.allLines.at(-1) ?? ""}${char}`;
        }
      }
    }

    private getBufferLine(row: number) {
      if (row < 0 || row >= this.buffer.active.length) {
        return undefined;
      }
      return new MockBufferLine(this.allLines[row] ?? "");
    }

    private rangeFromLinearSelection(column: number, row: number, length: number) {
      const safeLength = Math.max(1, length);
      const lastIndex = column + safeLength - 1;
      return {
        startCol: Math.max(0, column),
        startRow: Math.max(0, row),
        endCol: lastIndex % Math.max(1, this.cols),
        endRow: Math.max(0, row) + Math.floor(lastIndex / Math.max(1, this.cols)),
      };
    }

    private bufferRangeText(range: { startCol: number; startRow: number; endCol: number; endRow: number }) {
      const lines: string[] = [];
      for (let row = range.startRow; row <= range.endRow; row += 1) {
        const line = this.getBufferLine(row);
        if (!line) {
          continue;
        }
        const startColumn = row === range.startRow ? range.startCol : 0;
        const endColumn = row === range.endRow ? range.endCol + 1 : this.cols;
        lines.push(line.translateToString(false, startColumn, endColumn).replace(/\s+$/u, ""));
      }
      return lines.join("\n");
    }

    private renderData(data: string) {
      if (!this.element || data.length === 0) {
        return;
      }
      this.element.dataset.buffer = `${this.element.dataset.buffer ?? ""}${data}`;
      this.element.dataset.termdBuffer = this.element.dataset.buffer;
      this.element.append(document.createTextNode(data));
    }

    private decodeWriteText(data: Uint8Array) {
      for (const byte of data) {
        if (byte !== 0) {
          return textDecoder.decode(data).replace(/\u0000+/gu, "");
        }
      }
      return "";
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
  }

  return {
    Terminal: MockTerminal,
  };
});

vi.mock("@xterm/addon-fit", () => {
  class FitAddon {
    private terminal?: { resize: (cols: number, rows: number) => void };

    activate(terminal: { resize: (cols: number, rows: number) => void }) {
      this.terminal = terminal;
    }

    fit() {
      const proposed = this.proposeDimensions();
      this.terminal?.resize(proposed.cols, proposed.rows);
    }

    proposeDimensions() {
      return (
        (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } })
          .__TERMD_TEST_FIT_DIMENSIONS__ ?? { rows: 24, cols: 80 }
      );
    }
  }

  return { FitAddon };
});

vi.mock("@xterm/addon-search", () => {
  class SearchAddon {
    constructor(_options?: unknown) {}

    activate() {}

    clearDecorations() {}

    findNext() {}

    findPrevious() {}
  }

  return { SearchAddon };
});
