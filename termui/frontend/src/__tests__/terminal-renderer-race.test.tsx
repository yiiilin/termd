import { act, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { TerminalRendererInstance, TerminalRendererTerminal } from "../components/terminal/renderer";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((innerResolve) => {
    resolve = innerResolve;
  });
  return { promise, resolve };
}

function fakeDisposable() {
  return { dispose: () => undefined };
}

function fakeRenderer(label: string): TerminalRendererInstance {
  const marker = document.createElement("div");
  marker.dataset.rendererLabel = label;
  marker.textContent = label;
  const terminal = {
    cols: 80,
    rows: 24,
    modes: { applicationCursorKeysMode: false },
    options: {},
    buffer: {
      active: {
        baseY: 0,
        cursorX: 0,
        cursorY: 0,
        viewportY: 0,
        length: 24,
      },
    },
    open(parent: HTMLElement) {
      parent.appendChild(marker);
    },
    write(_data: string | Uint8Array, callback?: () => void) {
      callback?.();
    },
    resize() {},
    reset() {},
    refresh() {},
    focus() {},
    input() {},
    scrollToLine() {},
    onData: fakeDisposable,
    onCursorMove: fakeDisposable,
    onWriteParsed: fakeDisposable,
    onScroll: fakeDisposable,
    onSelectionChange: fakeDisposable,
    hasSelection: () => false,
    getSelection: () => "",
    select: () => undefined,
    selectViewportRange: () => undefined,
    getViewportRangeText: () => undefined,
    deselect: () => undefined,
    dispose: () => marker.remove(),
  } satisfies TerminalRendererTerminal;

  return {
    kind: "xterm",
    terminal,
    fit: {
      fit: () => undefined,
      proposeDimensions: () => ({ cols: 80, rows: 24 }),
    },
    getInputElement: () => undefined,
    isActivationTarget: () => false,
    setOptions: (options) => {
      terminal.options = options;
    },
    scrollState: () => ({
      viewportY: 0,
      baseY: 0,
      cursorLine: 0,
      cursorBottomLine: 0,
      length: 24,
    }),
    syncInputAnchor: () => undefined,
  };
}

describe("TerminalPane async renderer lifecycle", () => {
  it("迟到的旧 attach async renderer 不会清空新 attach 已挂载的 renderer DOM", async () => {
    vi.resetModules();
    const firstRenderer = deferred<TerminalRendererInstance>();
    const secondRenderer = deferred<TerminalRendererInstance>();
    let createCount = 0;
    vi.doMock("../components/terminal/renderer", () => ({
      createTerminalRendererInstance: () => {
        createCount += 1;
        return createCount === 1 ? firstRenderer.promise : secondRenderer.promise;
      },
      sameTerminalDimensions: (
        a: { rows: number; cols: number } | undefined,
        b: { rows: number; cols: number } | undefined,
      ) => Boolean(a && b && a.rows === b.rows && a.cols === b.cols),
    }));
    const { TerminalPane } = await import("../components/TerminalPane");
    const takeOutput = vi.fn(() => []);
    const registerOutputDrain = vi.fn(() => () => undefined);

    const props = {
      attached: false,
      sessionSize: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      takeOutput,
      registerOutputDrain,
      onInput: vi.fn(),
      onResize: vi.fn(),
      onCursorChange: vi.fn(),
    };
    const { rerender } = render(<TerminalPane {...props} outputResetVersion={0} />);

    // 中文注释：当前实现里 outputResetVersion 只做原地 reset，不再重建第二个 renderer。
    // 这里覆盖真实仍然存在的竞态：旧 attach 已经 detach 并发起新 attach 后，迟到返回的旧
    // async renderer 不能把新 attach 已挂载的 DOM 清掉。
    rerender(<TerminalPane {...props} attached outputResetVersion={0} />);
    rerender(<TerminalPane {...props} attached={false} outputResetVersion={0} />);
    rerender(<TerminalPane {...props} attached outputResetVersion={0} />);

    await act(async () => {
      secondRenderer.resolve(fakeRenderer("second-renderer"));
      await Promise.resolve();
    });
    expect(screen.getByTestId("terminal-pane")).toHaveTextContent("second-renderer");

    await act(async () => {
      firstRenderer.resolve(fakeRenderer("first-renderer"));
      await Promise.resolve();
    });
    expect(screen.getByTestId("terminal-pane")).toHaveTextContent("second-renderer");
    expect(screen.getByTestId("terminal-pane")).not.toHaveTextContent("first-renderer");

    vi.doUnmock("../components/terminal/renderer");
  });
});
