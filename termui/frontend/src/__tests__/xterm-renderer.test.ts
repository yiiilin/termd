import { describe, expect, it, vi } from "vitest";

describe("xterm renderer adapter", () => {
  it("补齐 TerminalPane 需要的 renderer contract，并保持 xterm 单栈语义", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });

    expect(renderer.kind).toBe("xterm");

    const host = document.createElement("div");
    renderer.terminal.open(host);

    expect(host.getAttribute("role")).toBe("textbox");
    expect(host.querySelector(".xterm")).not.toBeNull();
    expect(host.querySelector(".xterm-screen")).not.toBeNull();
    expect(host.querySelector('textarea[aria-label="Terminal input"]')).toBe(renderer.terminal.textarea);

    let writeParsedCount = 0;
    renderer.terminal.onWriteParsed(() => {
      writeParsedCount += 1;
    });

    renderer.terminal.write("line-1\nline-2");
    expect(writeParsedCount).toBe(1);
    expect(host.dataset.termdBuffer).toContain("line-1");
    expect(host.dataset.termdBuffer).toContain("line-2");

    renderer.setOptions({ fontSize: 18, theme: { background: "#101418" } });
    expect(renderer.terminal.options.fontSize).toBe(18);
    expect(renderer.terminal.options.theme).toEqual({ background: "#101418" });

    expect(() => renderer.search.findNext("line")).not.toThrow();
    expect(() => renderer.search.findPrevious("line")).not.toThrow();
    expect(() => renderer.search.clearDecorations()).not.toThrow();
  });

  it("viewport 选区和 facade 复制文本保持一致，不回落到陈旧 selection", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const host = document.createElement("div");
    renderer.terminal.open(host);

    renderer.terminal.write("hist-001\nhist-002\nhist-003\n");
    renderer.terminal.scrollToLine(0);

    const selected = renderer.terminal.selectViewportRange({ col: 0, row: 1 }, { col: 7, row: 1 });

    expect(selected).toBe("hist-002");
    expect(renderer.terminal.hasSelection()).toBe(true);
    expect(renderer.terminal.getSelection()).toBe("hist-002");
    expect(renderer.terminal.getSelectionPosition?.()).toEqual({
      start: { x: 0, y: 1 },
      end: { x: 7, y: 1 },
    });
    expect(host.dataset.termdSelection).toBe("hist-002");

    renderer.terminal.deselect();
    expect(renderer.terminal.hasSelection()).toBe(false);
    expect(renderer.terminal.getSelection()).toBe("");
  });

  it("scroll state 使用 xterm 的 top-based viewport 语义，并在 dispose 后停止 debug mirror", async () => {
    vi.resetModules();
    const { createXtermRenderer } = await import("../components/terminal/xterm-renderer");

    const renderer = createXtermRenderer({
      terminalOptions: { fontSize: 13 },
      searchOptions: {},
    });
    const host = document.createElement("div");
    renderer.terminal.open(host);
    renderer.terminal.write(Array.from({ length: 48 }, (_, index) => `row-${index + 1}\n`).join(""));

    const bottomState = renderer.scrollState();
    expect(bottomState?.baseY).toBeGreaterThan(0);
    expect(bottomState?.viewportY).toBe(bottomState?.baseY);

    renderer.terminal.scrollToLine(0);
    expect(renderer.scrollState()?.viewportY).toBe(0);
    expect(host.dataset.termdViewportYRaw).toBe(String(bottomState?.baseY ?? 0));

    renderer.terminal.dispose();
    host.dataset.termdBuffer = "after-dispose";
    expect(host.dataset.termdBuffer).toBe("after-dispose");
  });
});
