import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TerminalPane, type TerminalOutputItem } from "../components/TerminalPane";

const animationFrameMs = 16;

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
  expect(screen.getByLabelText("mobile direction gesture")).toBeInTheDocument();
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

describe("TerminalPane terminal sequence rendering", () => {
  it("snapshot 后推进 base seq，连续 output 正常写入并推进 terminal_seq", async () => {
    const onTerminalSeqRendered = vi.fn();

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10 },
        { kind: "output", bytes: new TextEncoder().encode("tail\n"), terminalSeq: 11 },
      ],
      { onTerminalSeqRendered },
    );

    await screen.findByText("snapshot", { exact: false });
    const xterm = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".xterm");
    await waitFor(() => expect(xterm?.dataset.buffer).toContain("tail"));
    expect(onTerminalSeqRendered.mock.calls).toEqual([[10], [11]]);
  });

  it("output terminal_seq 不连续时触发 resync，且不推进到跳号 frame", async () => {
    const onTerminalResync = vi.fn();
    const onTerminalSeqRendered = vi.fn();

    renderTerminalPaneWithOutput(
      [
        { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 10 },
        { kind: "output", bytes: new TextEncoder().encode("gap\n"), terminalSeq: 13 },
      ],
      { onTerminalResync, onTerminalSeqRendered },
    );

    await screen.findByText("snapshot", { exact: false });
    await waitFor(() => expect(onTerminalResync).toHaveBeenCalledWith(10));
    expect(onTerminalSeqRendered.mock.calls).toEqual([[10]]);
  });

  it("连续 output frame 合并成批量 xterm write，但仍逐帧确认 terminal_seq", async () => {
    const onTerminalSeqRendered = vi.fn();
    const items: TerminalOutputItem[] = [
      { kind: "snapshot", bytes: new TextEncoder().encode("snapshot\n"), baseSeq: 0 },
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
        { kind: "snapshot", bytes: encoder.encode("snapshot\n"), baseSeq: 0 },
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
        { kind: "snapshot", bytes: new TextEncoder().encode(snapshot), baseSeq: 0 },
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

  it("切换 session 时旧的异步 write 回调不能阻塞或确认新 session", async () => {
    vi.useFakeTimers();
    try {
      (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
        .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
      (globalThis as { __TERMD_TEST_SERIALIZE_XTERM_WRITES__?: boolean })
        .__TERMD_TEST_SERIALIZE_XTERM_WRITES__ = true;
      const encoder = new TextEncoder();
      let queue: TerminalOutputItem[] = [
        { kind: "snapshot", bytes: encoder.encode("old-session\n"), baseSeq: 10 },
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
        { kind: "snapshot", bytes: encoder.encode("new-session\n"), baseSeq: 30 },
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

describe("TerminalPane mobile direction gesture", () => {
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
