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
    private listeners: Array<(data: string) => void> = [];
    public element: HTMLDivElement | undefined;

    open(element: HTMLElement) {
      this.element = document.createElement("div");
      this.element.className = "xterm";
      const textarea = document.createElement("textarea");
      textarea.className = "xterm-helper-textarea";
      textarea.addEventListener("input", () => {
        const value = textarea.value;
        textarea.value = "";
        this.listeners.forEach((listener) => listener(value));
      });
      this.element.append(textarea);
      element.append(this.element);
    }

    loadAddon() {}

    write(data: string) {
      if (this.element) {
        this.element.dataset.buffer = `${this.element.dataset.buffer ?? ""}${data}`;
        this.element.append(document.createTextNode(data));
      }
    }

    onData(listener: (data: string) => void) {
      this.listeners.push(listener);
      return { dispose: () => undefined };
    }

    focus() {}

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
