import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TerminalPane, type TerminalOutputItem } from "../components/TerminalPane";
import type { SessionSearchResultPayload } from "../protocol/types";

const animationFrameMs = 16;
const DEFAULT_TERMINAL_SIZE = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((innerResolve, innerReject) => {
    resolve = innerResolve;
    reject = innerReject;
  });
  return { promise, resolve, reject };
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
    const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
    await waitFor(() => expect(xterm?.dataset.buffer).toContain("tail"));
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

    const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
    await waitFor(() => expect(xterm?.dataset.buffer).toContain("after-resize"));
    const operations = (globalThis as {
      __TERMD_TEST_XTERM_STATS__?: {
        operations: Array<{ op: string; cols?: number; rows?: number }>;
      };
    }).__TERMD_TEST_XTERM_STATS__?.operations ?? [];
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
        const stats = (globalThis as { __TERMD_TEST_XTERM_STATS__?: { writtenBytes: number } }).__TERMD_TEST_XTERM_STATS__;
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

    const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
    await waitFor(() => expect(xterm?.dataset.buffer).toContain("line-32"));
    const stats = (globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number } })
      .__TERMD_TEST_XTERM_STATS__;
    expect(stats?.writes ?? 0).toBeLessThan(10);
    expect(onTerminalSeqRendered.mock.calls.at(-1)).toEqual([32]);
    expect(onTerminalSeqRendered).toHaveBeenCalledTimes(33);
  });

  it("live output 停止后也刷新最后一笔写入，不需要等待下一次输入", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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
      const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
      expect(xterm?.dataset.buffer).toContain("snapshot");

      queue = [
        { kind: "output", bytes: encoder.encode("final-tail\n"), terminalSeq: 1 },
      ];
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      // 中文注释：真实 xterm 某些渲染时序下，最后一笔 write 已解析但尚未 repaint。
      // 如果 TerminalPane 只在下一次输入/resize 时 refresh，就会表现为“按一下键才出现尾巴”。
      expect(xterm?.dataset.buffer).toContain("final-tail");
      expect(onTerminalSeqRendered.mock.calls).toEqual([[0], [1]]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("进入 session 的异步 snapshot 写入完成后会贴到底部", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_DEFER_XTERM_BUFFER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_BUFFER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_KEEP_XTERM_VIEWPORT_AT_TOP_AFTER_WRITE__?: boolean })
        .__TERMD_TEST_KEEP_XTERM_VIEWPORT_AT_TOP_AFTER_WRITE__ = true;
      const snapshot = Array.from({ length: 80 }, (_, index) => `snapshot-line-${index}\n`).join("");

      renderTerminalPaneWithOutput([
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0, size: DEFAULT_TERMINAL_SIZE },
      ]);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 12);
      });

      const xterm = (globalThis as {
        __TERMD_TEST_XTERM__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_XTERM__;
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
        __TERMD_TEST_XTERM__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_XTERM__;
      expect(xterm?.baseY()).toBeGreaterThan(0);
      expect(xterm?.viewportY()).toBe(xterm?.baseY());

      act(() => {
        xterm?.scrollToLine(0);
      });
      expect(xterm?.viewportY()).toBe(0);

      const frame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      fireEvent.mouseDown(frame!);
      fireEvent.click(frame!);
      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 8);
      });

      // 中文注释：点击空白处只应该聚焦终端；如果用户正在看历史，不能强行滚回最新输出。
      expect(xterm?.viewportY()).toBe(0);
    } finally {
      vi.useRealTimers();
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
      // 本端即使仍处于聚焦态，也只能被动按远端尺寸重绘，不能立刻把自己的本地尺寸写回 daemon。
      expect(onResize).not.toHaveBeenCalled();
      const operations = (globalThis as {
        __TERMD_TEST_XTERM_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number }>;
        };
      }).__TERMD_TEST_XTERM_STATS__?.operations ?? [];
      expect(operations).toContainEqual({ op: "resize", cols: remoteSize.cols, rows: remoteSize.rows });
    } finally {
      vi.useRealTimers();
    }
  });

  it("snapshot 渲染完成后的本地刷新不会回写本地尺寸", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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

      expect(screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm")?.dataset.buffer)
        .toContain("remote-snapshot");
      // 中文注释：snapshot 渲染完成后的 refresh/stabilize 只修正本地显示，
      // 不能把聚焦客户端的本地尺寸再次写回 daemon，否则双客户端会形成 resize/snapshot 风暴。
      expect(onResize).not.toHaveBeenCalled();
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
        __TERMD_TEST_XTERM__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_XTERM__;
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

  it("位于底部时纯 resize frame 也会重新贴到新的 PTY 底部", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_KEEP_XTERM_VIEWPORT_AT_TOP_AFTER_RESIZE__?: boolean })
        .__TERMD_TEST_KEEP_XTERM_VIEWPORT_AT_TOP_AFTER_RESIZE__ = true;
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
        __TERMD_TEST_XTERM__?: { viewportY: () => number; baseY: () => number };
      }).__TERMD_TEST_XTERM__;
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
        __TERMD_TEST_XTERM__?: { viewportY: () => number; baseY: () => number; scrollToLine: (line: number) => void };
      }).__TERMD_TEST_XTERM__;
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

  it("切换 session 时旧的异步 write 回调不能阻塞或确认新 session", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_SERIALIZE_XTERM_WRITES__?: boolean })
        .__TERMD_TEST_SERIALIZE_XTERM_WRITES__ = true;
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

      act(() => {
        vi.advanceTimersByTime(animationFrameMs);
      });
      expect((globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number } }).__TERMD_TEST_XTERM_STATS__?.writes).toBe(1);
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

      expect(onOutputResetApplied).toHaveBeenCalledWith(1);
      act(() => {
        drainOutput?.();
        vi.advanceTimersByTime(animationFrameMs);
      });

      // 中文注释：旧 session 的 write 回调尚未返回时，新 session 的 snapshot 也必须能开始写入；
      // 否则用户快速切 session 会被旧的大量输出拖住，表现为整个 Web 延迟数秒。
      expect((globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number } }).__TERMD_TEST_XTERM_STATS__?.writes).toBe(2);

      act(() => {
        vi.advanceTimersByTime(animationFrameMs * 6);
      });

      expect(onTerminalSeqRendered.mock.calls).toEqual([[30]]);
      const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
      act(() => {
        // 中文注释：TerminalPane 在 write callback 之后再排一帧 refresh；这里单独推进，确认新实例完成绘制。
        vi.advanceTimersByTime(animationFrameMs);
      });
      expect(xterm?.dataset.buffer).toContain("new-session");
      expect(xterm?.dataset.buffer).not.toContain("old-session");
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
    expect(screen.getByTestId("xterm-search-highlight")).toHaveTextContent("beta");

    await act(async () => {
      alphaSearch.resolve(searchResult("alpha", 1));
      await alphaSearch.promise;
    });

    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByTestId("xterm-search-highlight")).toHaveTextContent("beta");
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

    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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
      __TERMD_TEST_XTERM_STATS__?: {
        operations: Array<{ op: string; cols?: number; rows?: number }>;
      };
    }).__TERMD_TEST_XTERM_STATS__?.operations ?? [];
    // 中文注释：daemon 确认可能因为持续输出延迟；聚焦客户端仍要先撑开本地视口，
    // 否则 xterm 会长期停在默认 24 行，外层面板下方只剩大片空白。
    expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
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
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
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
        __TERMD_TEST_XTERM_STATS__?: {
          operations: Array<{ op: string; cols?: number; rows?: number; text?: string }>;
        };
      }).__TERMD_TEST_XTERM_STATS__?.operations ?? [];
      expect(operations).toContainEqual({ op: "resize", cols: 101, rows: 31 });
      expect(
        operations.some((operation) => operation.op === "write" && operation.text === "\x1b7\x1b[r\x1b8"),
      ).toBe(false);
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
    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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
    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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
    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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
