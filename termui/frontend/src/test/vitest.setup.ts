import "@testing-library/jest-dom/vitest";
import "fake-indexeddb/auto";
import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import WebSocket from "ws";

// 单元测试在 Node/jsdom 中运行，使用 ws 提供真实 WebSocket，确保 DirectClient
// 测试仍经过完整 E2EE wire 流程，而不是绕过协议状态机。
Object.assign(globalThis, { WebSocket });

afterEach(() => {
  cleanup();
});

vi.mock("@xterm/xterm", () => {
  class Terminal {
    private dataListeners: Array<(data: string) => void> = [];
    private cursorMoveListeners: Array<() => void> = [];
    private writeParsedListeners: Array<() => void> = [];
    private terminalOptions: Record<string, unknown>;
    public buffer = { active: { cursorY: 0, cursorX: 0 } };
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
    }

    loadAddon() {}

    write(data: string, callback?: () => void) {
      if (this.element) {
        this.element.dataset.buffer = `${this.element.dataset.buffer ?? ""}${data}`;
        this.element.append(document.createTextNode(data));
      }
      const lines = data.split("\n");
      const lastLine = lines[lines.length - 1] ?? "";
      if (data.includes("\n")) {
        this.buffer.active.cursorY += lines.length - 1;
        this.buffer.active.cursorX = lastLine.length;
      } else {
        this.buffer.active.cursorX += data.length;
      }
      this.writeParsedListeners.forEach((listener) => listener());
      this.cursorMoveListeners.forEach((listener) => listener());
      callback?.();
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

    resize(cols: number, rows: number) {
      this.cols = cols;
      this.rows = rows;
    }

    focus() {
      this.element?.querySelector("textarea")?.focus();
    }

    dispose() {}
  }

  return { Terminal };
});

vi.mock("@xterm/addon-fit", () => {
  class FitAddon {
    fit() {}

    proposeDimensions() {
      return { rows: 24, cols: 80 };
    }
  }

  return { FitAddon };
});
