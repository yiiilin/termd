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
  delete (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
    .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__;
  delete (globalThis as { __TERMD_TEST_SERIALIZE_XTERM_WRITES__?: boolean })
    .__TERMD_TEST_SERIALIZE_XTERM_WRITES__;
  delete (globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number; refreshes: number; writtenBytes: number } })
    .__TERMD_TEST_XTERM_STATS__;
  delete (globalThis as { __TERMD_TEST_XTERM__?: { select: (text: string) => void } }).__TERMD_TEST_XTERM__;
  clipboardWriteTextMock.mockClear();
  cleanup();
});

vi.mock("@xterm/xterm", () => {
  const textDecoder = new TextDecoder();

  function xtermStats() {
    const scope = globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number; refreshes: number; writtenBytes: number } };
    scope.__TERMD_TEST_XTERM_STATS__ ??= { writes: 0, refreshes: 0, writtenBytes: 0 };
    return scope.__TERMD_TEST_XTERM_STATS__;
  }

  class Terminal {
    private dataListeners: Array<(data: string) => void> = [];
    private cursorMoveListeners: Array<() => void> = [];
    private writeParsedListeners: Array<() => void> = [];
    private scrollListeners: Array<(viewportY: number) => void> = [];
    private selectionChangeListeners: Array<() => void> = [];
    private terminalOptions: Record<string, unknown>;
    private pendingRender = "";
    private pendingRenderReady = false;
    private serializedWriteInFlight = false;
    private serializedWriteQueue: Array<{ data: string | Uint8Array; callback?: () => void }> = [];
    private selection = "";
    public buffer = { active: { cursorY: 0, cursorX: 0, viewportY: 0, baseY: 0 } };
    public cols = 80;
    public rows = 24;
    public element: HTMLDivElement | undefined;

    constructor(options: Record<string, unknown> = {}) {
      this.terminalOptions = options;
    }

    get options() {
      return { ...this.terminalOptions, cols: this.cols, rows: this.rows };
    }

    set options(nextOptions: Record<string, unknown>) {
      // 真实 xterm 不允许在运行期重新设置 cols/rows；测试里保留这个约束，避免缩放时误写只读配置。
      if ("cols" in nextOptions || "rows" in nextOptions) {
        throw new Error('Option "cols" can only be set in the constructor');
      }
      this.terminalOptions = { ...this.terminalOptions, ...nextOptions };
    }

    open(element: HTMLElement) {
      this.element = document.createElement("div");
      this.element.className = "xterm";
      const textarea = document.createElement("textarea");
      textarea.className = "xterm-helper-textarea";
      textarea.addEventListener("input", () => {
        const value = textarea.value;
        textarea.value = "";
        this.buffer.active.cursorX += value.length;
        this.dataListeners.forEach((listener) => listener(value));
        this.cursorMoveListeners.forEach((listener) => listener());
      });
      this.element.append(textarea);
      element.append(this.element);
      (globalThis as { __TERMD_TEST_XTERM__?: { select: (text: string) => void } }).__TERMD_TEST_XTERM__ = {
        select: (text: string) => {
          this.selection = text;
          this.selectionChangeListeners.forEach((listener) => listener());
        },
      };
    }

    loadAddon(addon?: { activate?: (terminal: Terminal) => void }) {
      addon?.activate?.(this);
    }

    write(data: string | Uint8Array, callback?: () => void) {
      const serializeWrites = Boolean(
        (globalThis as { __TERMD_TEST_SERIALIZE_XTERM_WRITES__?: boolean })
          .__TERMD_TEST_SERIALIZE_XTERM_WRITES__,
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
      const stats = xtermStats();
      stats.writes += 1;
      stats.writtenBytes += typeof data === "string" ? data.length : data.byteLength;
      const deferRender = Boolean(
        (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
          .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__,
      );
      if (deferRender) {
        this.pendingRender += text;
        this.pendingRenderReady = false;
      } else {
        this.renderData(text);
      }
      const lines = text.split("\n");
      const lastLine = lines[lines.length - 1] ?? "";
      if (text.includes("\n")) {
        this.buffer.active.cursorY += lines.length - 1;
        this.buffer.active.cursorX = lastLine.length;
      } else {
        this.buffer.active.cursorX += text.length;
      }
      this.buffer.active.baseY = Math.max(0, this.buffer.active.cursorY - this.rows + 1);
      this.buffer.active.viewportY = this.buffer.active.baseY;
      this.writeParsedListeners.forEach((listener) => listener());
      this.cursorMoveListeners.forEach((listener) => listener());
      this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
      if (!deferRender) {
        callback?.();
        return;
      }

      // 真实 xterm 会异步完成 write 解析和绘制；这里延后三帧，让测试能覆盖
      // “write 后立即 refresh 但内容尚不可绘制”的浏览器时序。
      window.requestAnimationFrame(() => {
        window.requestAnimationFrame(() => {
          window.requestAnimationFrame(() => {
            this.pendingRenderReady = true;
            callback?.();
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

    onWriteParsed(listener: () => void) {
      this.writeParsedListeners.push(listener);
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

    scrollToLine(line: number) {
      this.buffer.active.viewportY = Math.min(this.buffer.active.baseY, Math.max(0, line));
      this.scrollListeners.forEach((listener) => listener(this.buffer.active.viewportY));
    }

    resize(cols: number, rows: number) {
      const previousBaseY = this.buffer.active.baseY;
      const wasAtBottom = this.buffer.active.viewportY >= previousBaseY;
      this.cols = cols;
      this.rows = rows;
      this.buffer.active.baseY = Math.max(0, this.buffer.active.cursorY - this.rows + 1);
      this.buffer.active.viewportY = wasAtBottom
        ? this.buffer.active.baseY
        : Math.min(this.buffer.active.viewportY, this.buffer.active.baseY);
    }

    focus() {
      this.element?.querySelector("textarea")?.focus();
    }

    refresh() {
      xtermStats().refreshes += 1;
      if (!this.pendingRenderReady || !this.pendingRender) {
        return;
      }
      const data = this.pendingRender;
      this.pendingRender = "";
      this.pendingRenderReady = false;
      this.renderData(data);
    }

    clear() {
      // mock 要真实清空 DOM buffer，才能覆盖 WebSocket 断线重连后的 snapshot 重放不会叠加旧内容。
      this.pendingRender = "";
      this.pendingRenderReady = false;
      if (!this.element) {
        return;
      }
      this.element.dataset.buffer = "";
      for (const node of Array.from(this.element.childNodes)) {
        if (node.nodeType === Node.TEXT_NODE) {
          node.remove();
        }
      }
      this.buffer.active.cursorY = 0;
      this.buffer.active.cursorX = 0;
      this.buffer.active.baseY = 0;
      this.buffer.active.viewportY = 0;
    }

    reset() {
      // xterm.reset() 会清空终端状态；测试 mock 复用 clear 的 DOM/游标重置逻辑即可。
      this.clear();
    }

    dispose() {}

    private renderData(data: string) {
      if (!this.element) {
        return;
      }
      this.element.dataset.buffer = `${this.element.dataset.buffer ?? ""}${data}`;
      this.element.append(document.createTextNode(data));
    }
  }

  return { Terminal };
});

vi.mock("@xterm/addon-fit", () => {
  class FitAddon {
    private terminal?: { resize: (cols: number, rows: number) => void };

    activate(terminal: { resize: (cols: number, rows: number) => void }) {
      this.terminal = terminal;
    }

    fit() {
      const proposed = this.proposeDimensions();
      // xterm 的 FitAddon.fit 会把终端尺寸同步成 proposeDimensions 的结果；
      // mock 也要这样做，才能覆盖 resize ack 后回聚焦的滚动保持逻辑。
      this.terminal?.resize(proposed.cols, proposed.rows);
    }

    proposeDimensions() {
      // 测试用例可显式覆盖 xterm 当前容器能容纳的尺寸，用来模拟浏览器窗口 resize。
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
    private terminal?: { element?: HTMLElement };

    activate(terminal: { element?: HTMLElement }) {
      this.terminal = terminal;
    }

    findNext(term: string, options?: { decorations?: unknown }) {
      this.renderSearchMarker(term, Boolean(options?.decorations));
      return Boolean(term);
    }

    findPrevious(term: string, options?: { decorations?: unknown }) {
      this.renderSearchMarker(term, Boolean(options?.decorations));
      return Boolean(term);
    }

    clearDecorations() {
      this.terminal?.element?.querySelectorAll("[data-testid='xterm-search-highlight']").forEach((node) => node.remove());
    }

    clearActiveDecoration() {}

    dispose() {
      this.clearDecorations();
    }

    private renderSearchMarker(term: string, enabled: boolean) {
      this.clearDecorations();
      if (!term || !enabled || !this.terminal?.element) {
        return;
      }
      // 测试 mock 不复刻 xterm 的真实 decoration DOM，只暴露一个稳定标记，
      // 用来验证 TerminalPane 确实调用 search addon 开启高亮。
      const marker = document.createElement("mark");
      marker.dataset.testid = "xterm-search-highlight";
      marker.textContent = term;
      this.terminal.element.append(marker);
    }
  }

  return { SearchAddon };
});
