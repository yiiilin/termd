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
      resizeEnabled
      outputResetVersion={0}
      takeOutput={takeOutput}
      registerOutputDrain={registerOutputDrain}
      onInput={onInput}
      onResize={vi.fn()}
      onCursorChange={vi.fn()}
    />,
  );
  return {
    frame: screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-viewer-frame")!,
    onInput,
  };
}

function mockTerminalViewerLayout(input: { viewportWidth: number; viewportHeight: number }) {
  let viewportHeight = input.viewportHeight;
  const clientWidthSpy = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? input.viewportWidth : 0;
  });
  const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? viewportHeight : 0;
  });
  const offsetWidthSpy = vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-viewer-frame") ? Number.parseFloat(this.style.width || "0") : 0;
  });
  const offsetHeightSpy = vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-viewer-frame") ? Number.parseFloat(this.style.height || "0") : 0;
  });
  const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport")
      ? Math.max(viewportHeight, Number.parseFloat(this.querySelector<HTMLElement>(".terminal-viewer-frame")?.style.height || "0"))
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

function viewerScaleFromHost(): number {
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
      resizeEnabled
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
          resizeEnabled
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
          resizeEnabled
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

describe("TerminalPane mobile viewer layout", () => {
  it("移动端 viewer 模式在软键盘打开后按新的可视高度重新缩放并贴底", async () => {
    const layout = mockTerminalViewerLayout({ viewportWidth: 390, viewportHeight: 420 });
    try {
      const takeOutput = vi.fn(() => []);
      const registerOutputDrain = vi.fn(() => () => undefined);
      const { rerender } = render(
        <TerminalPane
          attached
          sessionSize={{ rows: 30, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen={false}
          resizeEnabled={false}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true"));
      await waitFor(() => expect(viewerScaleFromHost()).toBeLessThan(1));
      const scaleBeforeKeyboard = viewerScaleFromHost();

      layout.setViewportHeight(260);
      rerender(
        <TerminalPane
          attached
          sessionSize={{ rows: 30, cols: 80, pixel_width: 0, pixel_height: 0 }}
          mobileInputMode
          mobileKeyboardOpen
          resizeEnabled={false}
          outputResetVersion={0}
          takeOutput={takeOutput}
          registerOutputDrain={registerOutputDrain}
          onInput={vi.fn()}
          onResize={vi.fn()}
          onCursorChange={vi.fn()}
        />,
      );

      await waitFor(() => expect(viewerScaleFromHost()).toBeLessThan(scaleBeforeKeyboard));
      const scrollport = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-scrollport");
      await waitFor(() => expect(scrollport?.scrollTop).toBeGreaterThan(0));
    } finally {
      layout.restore();
    }
  });
});
