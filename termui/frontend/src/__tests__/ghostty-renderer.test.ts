import { describe, expect, it, vi } from "vitest";

const ghosttyWebMock = vi.hoisted(() => {
  const textCells = (text: string) => Array.from(text, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 }));

  class MockTerminal {
    cols = 80;
    rows = 24;
    element?: HTMLElement;
    textarea?: HTMLTextAreaElement;
    options: Record<string, unknown>;
    viewportY = 0;
    publicScrollbackLength?: number;
    selectionPosition?: { start: { x: number; y: number }; end: { x: number; y: number } };
    selectCalls: Array<{ column: number; row: number; length: number }> = [];
    deselectCalls = 0;
    hasSelectionValue = false;
    scrollbackLines: Array<Array<{ codepoint: number; grapheme_len: number }>> = [];
    screenLines: Array<Array<{ codepoint: number; grapheme_len: number }>> = [];
    renderListeners: Array<() => void> = [];
    scrollLineCalls: number[] = [];
    requestRenderCalls = 0;
    rendererRenderCalls = 0;
    lastRendererRenderArgs?: {
      buffer: unknown;
      forceAll?: boolean;
      viewportY?: number;
      scrollbackProvider?: unknown;
      scrollbarOpacity?: number;
    };
    rendererTheme?: unknown;
    lastWriteData?: string | Uint8Array;
    rendererMetrics = { width: 8, height: 14, baseline: 12 };
    rendererLatinMeasure = { width: 8, actualBoundingBoxAscent: 10, actualBoundingBoxDescent: 2 };
    rendererCjkMeasure = { width: 16, actualBoundingBoxAscent: 10, actualBoundingBoxDescent: 2 };
    selectionManager = {
      selectionStart: undefined as { col: number; absoluteRow: number } | undefined,
      selectionEnd: undefined as { col: number; absoluteRow: number } | undefined,
      clearSelection: vi.fn(() => {
        this.selectionManager.selectionStart = undefined;
        this.selectionManager.selectionEnd = undefined;
        this.hasSelectionValue = false;
      }),
      markCurrentSelectionDirty: vi.fn(),
      requestRender: vi.fn(() => {
        this.requestRender();
      }),
      selectionChangedEmitter: {
        fire: vi.fn(() => {
          this.hasSelectionValue = true;
        }),
      },
    };
    renderer = {
      ctx: {
        font: "",
        save: () => undefined,
        restore: () => undefined,
        measureText: (text: string) => (text === "中" ? this.rendererCjkMeasure : this.rendererLatinMeasure),
      },
      fontSize: 13,
      fontFamily: "mock-mono",
      measureFont: () => this.rendererMetrics,
      remeasureFont: () => {
        this.rendererMetrics = this.renderer.measureFont();
      },
      getMetrics: () => this.rendererMetrics,
      setTheme: (theme: unknown) => {
        this.rendererTheme = theme;
      },
      render: (buffer?: unknown, forceAll?: boolean, viewportY?: number, scrollbackProvider?: unknown, scrollbarOpacity?: number) => {
        this.rendererRenderCalls += 1;
        this.lastRendererRenderArgs = {
          buffer,
          forceAll,
          viewportY,
          scrollbackProvider,
          scrollbarOpacity,
        };
      },
    };
    buffer = {
      active: {
        baseY: 0,
        cursorX: 0,
        cursorY: 0,
        viewportY: 0,
        length: 24,
      },
    };
    wasmTerm = {
      scrollbackLength: 0,
      getScrollbackLength: () => this.wasmTerm.scrollbackLength,
      getScrollbackLine: (offset: number) => this.scrollbackLines[offset] ?? null,
      getLine: (row: number) => this.screenLines[row] ?? null,
      getGraphemeString: (row: number, col: number) => String.fromCodePoint(this.screenLines[row]?.[col]?.codepoint ?? 0),
      getScrollbackGraphemeString: (offset: number, col: number) =>
        String.fromCodePoint(this.scrollbackLines[offset]?.[col]?.codepoint ?? 0),
    };

    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      ghosttyWebMock.terminals.push(this);
    }

    loadAddon(addon: { activate?: (terminal: MockTerminal) => void }): void {
      addon.activate?.(this);
    }

    open(parent: HTMLElement): void {
      this.element = parent;
      this.element.tabIndex = 0;
      this.element.setAttribute("contenteditable", "true");
      this.element.setAttribute("role", "textbox");
      this.element.setAttribute("aria-label", "Terminal input");
      this.element.setAttribute("aria-multiline", "true");
      const canvas = document.createElement("canvas");
      this.textarea = document.createElement("textarea");
      this.textarea.setAttribute("aria-label", "Terminal input");
      parent.append(canvas, this.textarea);
    }

    write(data: string | Uint8Array, callback?: () => void): void {
      this.lastWriteData = data;
      if (this.viewportY !== 0) {
        this.scrollToBottom();
      }
      callback?.();
    }

    resize(cols: number, rows: number): void {
      this.cols = cols;
      this.rows = rows;
    }

    reset(): void {}

    refresh(): void {
      throw new Error("ghostty mock must not expose Ghostty refresh");
    }

    requestRender(): void {
      this.requestRenderCalls += 1;
    }

    focus(): void {
      this.textarea?.focus();
    }

    scrollToLine(line: number): void {
      this.scrollLineCalls.push(line);
      this.viewportY = line;
    }

    scrollToBottom(): void {
      this.viewportY = 0;
    }

    getScrollbackLength(): number {
      return this.publicScrollbackLength ?? this.wasmTerm.scrollbackLength;
    }

    onData(): { dispose: () => void } {
      return { dispose: () => undefined };
    }

    onCursorMove(): { dispose: () => void } {
      return { dispose: () => undefined };
    }

    onRender(listener: () => void): { dispose: () => void } {
      this.renderListeners.push(listener);
      return {
        dispose: () => {
          this.renderListeners = this.renderListeners.filter((candidate) => candidate !== listener);
        },
      };
    }

    onScroll(): { dispose: () => void } {
      return { dispose: () => undefined };
    }

    onSelectionChange(): { dispose: () => void } {
      return { dispose: () => undefined };
    }

    hasSelection(): boolean {
      return this.hasSelectionValue;
    }

    getSelection(): string {
      return (this as { nativeSelectionText?: string }).nativeSelectionText ?? "";
    }

    select(column: number, row: number, length: number): void {
      this.selectCalls.push({ column, row, length });
      this.selectionPosition = {
        start: { x: column, y: row },
        end: { x: column + Math.max(0, length - 1), y: row },
      };
      this.hasSelectionValue = true;
    }

    deselect(): void {
      this.deselectCalls += 1;
      this.selectionPosition = undefined;
      this.hasSelectionValue = false;
    }

    getSelectionPosition(): { start: { x: number; y: number }; end: { x: number; y: number } } | undefined {
      return this.selectionPosition;
    }

    dispose(): void {
      this.element?.replaceChildren();
      this.element = undefined;
      this.textarea = undefined;
    }
  }

  class MockFitAddon {
    activate(): void {}
    fit(): void {}
    proposeDimensions(): { cols: number; rows: number } {
      return { cols: 80, rows: 24 };
    }
    dispose(): void {}
  }

  return {
    init: vi.fn(() => Promise.resolve()),
    Terminal: MockTerminal,
    FitAddon: MockFitAddon,
    terminals: [] as MockTerminal[],
    textCells,
  };
});

vi.mock("ghostty-web", () => ghosttyWebMock);

describe("ghostty renderer adapter", () => {
  it("补齐 TerminalPane 需要的 renderer contract 并归一化 ghostty scroll/options 语义", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const first = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const second = await createGhosttyRenderer({
      terminalOptions: { fontSize: 12 },
      searchOptions: {},
    });

    expect(ghosttyWebMock.init).toHaveBeenCalledTimes(1);
    expect(first.kind).toBe("ghostty");
    expect(second.kind).toBe("ghostty");

    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    first.terminal.open(host);
    expect(terminal.element).toBe(host);
    expect(host.querySelector("canvas")).not.toBeNull();
    expect(host.querySelector('textarea[aria-label="Terminal input"]')).toBe(terminal.textarea);

    let writeParsedCount = 0;
    first.terminal.onWriteParsed(() => {
      writeParsedCount += 1;
    });
    host.dataset.buffer = "same-line-output";
    first.terminal.write("hello");
    expect(writeParsedCount).toBe(1);
    expect(host.dataset.termdBuffer).toBe("same-line-output");

    first.terminal.write(new TextEncoder().encode("line-1\nline-2\n"));
    expect(terminal.lastWriteData).toBeDefined();
    expect(new TextDecoder().decode(terminal.lastWriteData as Uint8Array)).toBe("line-1\r\nline-2\r\n");

    first.terminal.refresh(0, first.terminal.rows - 1);
    expect(terminal.requestRenderCalls).toBe(1);

    const optionsObject = terminal.options;
    const theme = { background: "#000000" };
    const previousTheme = terminal.options.theme;
    first.setOptions({ fontSize: 18, theme });
    expect(terminal.options).toBe(optionsObject);
    expect(terminal.options.fontSize).toBe(18);
    // 中文注释：Ghostty 运行期换 theme 不能完整重写 WASM buffer；
    // renderer contract 对 theme 保持 no-op，App 负责重建终端并重放 snapshot。
    expect(terminal.options.theme).toBe(previousTheme);
    expect(terminal.rendererTheme).toBeUndefined();
    expect(terminal.rendererRenderCalls).toBe(0);

    terminal.wasmTerm.scrollbackLength = 40;
    terminal.viewportY = 0;
    expect(first.scrollState()).toMatchObject({
      viewportY: 40,
      baseY: 40,
      cursorBottomLine: 40,
      length: 64,
    });

    terminal.viewportY = 7;
    expect(first.scrollState()?.viewportY).toBe(33);
    terminal.requestRenderCalls = 0;
    first.terminal.scrollToLine(99);
    expect(terminal.scrollLineCalls.at(-1)).toBe(0);
    expect(terminal.requestRenderCalls).toBe(1);
    first.terminal.scrollToLine(0);
    expect(terminal.scrollLineCalls.at(-1)).toBe(40);
    expect(host.dataset.termdViewportYRaw).toBe("40");
    terminal.scrollToLine(5);
    expect(terminal.scrollLineCalls.at(-1)).toBe(5);

    // 中文注释：真实 ghostty-web 公开了 Terminal.getScrollbackLength()；内部 wasmTerm
    // 不是稳定业务 API，adapter 必须优先读公开方法，才能让侧边滚动条判断可用。
    terminal.publicScrollbackLength = 42;
    terminal.wasmTerm = undefined as unknown as typeof terminal.wasmTerm;
    terminal.viewportY = 2;
    expect(first.scrollState()).toMatchObject({
      viewportY: 40,
      baseY: 42,
      cursorBottomLine: 42,
      length: 66,
    });
    first.terminal.scrollToLine(0);
    expect(terminal.scrollLineCalls.at(-1)).toBe(42);

    terminal.publicScrollbackLength = undefined;
    terminal.wasmTerm = {
      scrollbackLength: 40,
      getScrollbackLength: () => terminal.wasmTerm.scrollbackLength,
      getScrollbackLine: () => null,
      getLine: () => null,
      getGraphemeString: () => "",
      getScrollbackGraphemeString: () => "",
    } as unknown as typeof terminal.wasmTerm;
    terminal.viewportY = 7;
    terminal.scrollLineCalls.length = 0;
    first.terminal.write("new output while reading scrollback");
    expect(terminal.viewportY).toBe(7);
    expect(terminal.scrollLineCalls.at(-1)).toBe(7);
    expect(first.scrollState()?.viewportY).toBe(33);

    expect(() => first.search.findNext("hello")).not.toThrow();
    expect(() => first.search.clearDecorations()).not.toThrow();
  });

  it("selection API 会在 ghostty 原生 getSelection 为空时按 position 回退重建文本", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    terminal.screenLines = [Array.from("hello", (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 }))];
    renderer.terminal.select(0, 0, 5);
    expect(terminal.selectCalls).toEqual([{ column: 0, row: 0, length: 5 }]);
    expect(renderer.terminal.hasSelection()).toBe(true);
    expect(renderer.terminal.getSelection()).toBe("hello");
    renderer.terminal.deselect();
    expect(terminal.deselectCalls).toBe(1);
    expect(renderer.terminal.hasSelection()).toBe(false);
  });

  it("renderer patch 只归一化 viewportY，不隐藏 Ghostty 自带滚动条", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });

    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);

    const buffer = { id: "buffer" };
    const scrollbackProvider = { id: "scrollback" };
    terminal.renderer.render(buffer, true, 12.75, scrollbackProvider, 0.42);

    expect(terminal.rendererRenderCalls).toBe(1);
    expect(terminal.lastRendererRenderArgs).toEqual({
      buffer,
      forceAll: true,
      viewportY: 12,
      scrollbackProvider,
      scrollbarOpacity: 0.42,
    });
  });

  it("open 时会按 CJK 样本抬高 font metrics，避免首行汉字被裁掉", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });

    const terminal = ghosttyWebMock.terminals[0];
    terminal.rendererMetrics = { width: 8, height: 14, baseline: 12 };
    terminal.rendererLatinMeasure = { width: 8, actualBoundingBoxAscent: 9, actualBoundingBoxDescent: 2 };
    terminal.rendererCjkMeasure = { width: 16, actualBoundingBoxAscent: 11, actualBoundingBoxDescent: 3 };

    const host = document.createElement("div");
    renderer.terminal.open(host);

    // 中文注释：宽度仍沿用 monospace 单格宽，避免列数抖动；只把高度和 baseline
    // 抬到能容纳 CJK 首行字头的水平。
    expect(terminal.rendererMetrics).toEqual({ width: 8, height: 17, baseline: 14 });
  });

  it("CJK metrics 补偿后 fit 列数保持不变，且重复测量结果稳定", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    try {
      ghosttyWebMock.init.mockClear();
      ghosttyWebMock.terminals.length = 0;
      const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

      const renderer = await createGhosttyRenderer({
        terminalOptions: { fontSize: 14, convertEol: true },
        searchOptions: {},
      });

      const terminal = ghosttyWebMock.terminals[0];
      terminal.rendererMetrics = { width: 8, height: 14, baseline: 12 };
      terminal.rendererLatinMeasure = { width: 8, actualBoundingBoxAscent: 9, actualBoundingBoxDescent: 2 };
      terminal.rendererCjkMeasure = { width: 16, actualBoundingBoxAscent: 11, actualBoundingBoxDescent: 3 };

      const host = document.createElement("div");
      Object.defineProperty(host, "clientWidth", { configurable: true, value: 790 });
      Object.defineProperty(host, "clientHeight", { configurable: true, value: 860 });
      renderer.terminal.open(host);

      const first = renderer.fit.proposeDimensions();
      const second = renderer.fit.proposeDimensions();

      // 中文注释：首行中文补偿只能影响行高，不能把单格宽度改掉；
      // 否则 cols 会在 open/remeasure 之后抖动，重新触发 PTY resize。
      expect(terminal.rendererMetrics.width).toBe(8);
      expect(first).toEqual({ cols: 98, rows: 50 });
      expect(second).toEqual(first);

      renderer.fit.fit();
      expect(terminal.cols).toBe(first?.cols);
      expect(terminal.rows).toBe(first?.rows);

      vi.advanceTimersByTime(60);
      renderer.fit.fit();
      expect(terminal.cols).toBe(first?.cols);
      expect(terminal.rows).toBe(first?.rows);
    } finally {
      vi.useRealTimers();
    }
  });

  it("renderer 缺少 font metrics hooks 时会安全退化，不阻断 open", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });

    const terminal = ghosttyWebMock.terminals[0];
    terminal.renderer.measureFont = undefined as unknown as typeof terminal.renderer.measureFont;
    terminal.renderer.remeasureFont = undefined as unknown as typeof terminal.renderer.remeasureFont;
    terminal.renderer.ctx = undefined as unknown as typeof terminal.renderer.ctx;

    const host = document.createElement("div");

    expect(() => renderer.terminal.open(host)).not.toThrow();
    expect(host.querySelector("canvas")).not.toBeNull();
    expect(host.querySelector('textarea[aria-label="Terminal input"]')).toBe(terminal.textarea);
  });

  it("debug buffer mirror 在上滚时镜像当前 viewport 文本，避免 scrollback 视图和底部尾巴串屏", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });

    const terminal = ghosttyWebMock.terminals[0];
    terminal.rows = 3;
    terminal.wasmTerm.scrollbackLength = 2;
    terminal.scrollbackLines = [ghosttyWebMock.textCells("scroll-001"), ghosttyWebMock.textCells("scroll-002")];
    terminal.screenLines = [
      ghosttyWebMock.textCells("screen-003"),
      ghosttyWebMock.textCells("screen-004"),
      ghosttyWebMock.textCells("screen-005"),
    ];
    const host = document.createElement("div");
    host.dataset.buffer = "stale-fallback-buffer";
    renderer.terminal.open(host);

    renderer.terminal.scrollToLine(0);

    expect(host.dataset.termdBuffer).toBe(
      ["scroll-001", "scroll-002", "screen-003"].join("\n"),
    );
  });

  it("dispose 后会关闭 debug mirror，旧异步回调不能重新写入明文 dataset", async () => {
    vi.resetModules();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);
    host.dataset.buffer = "before-dispose";
    renderer.terminal.write("hello");
    expect(host.dataset.termdBuffer).toBe("before-dispose");

    renderer.terminal.dispose();
    host.dataset.buffer = "after-dispose";
    host.dataset.termdBuffer = "cleared-by-pane";
    terminal.renderListeners.forEach((listener) => listener());
    terminal.scrollLineCalls.length = 0;

    // 中文注释：TerminalPane cleanup 会删除 debug dataset；旧 Ghostty write/render
    // 回调即使晚到，也不能把旧终端明文重新写回已清理的 host。
    expect(host.dataset.termdBuffer).toBe("cleared-by-pane");
    expect((terminal as { __termdDebugBufferSync?: () => void }).__termdDebugBufferSync).toBeUndefined();
  });

  it("selection API 优先按 position 重建文本，避免原生 selection 返回陈旧内容", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0] as typeof ghosttyWebMock.terminals[number] & { nativeSelectionText?: string };
    terminal.screenLines = [Array.from("current", (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 }))];
    terminal.nativeSelectionText = "stale";
    renderer.terminal.select(0, 0, 7);

    // 中文注释：Ghostty 原生 selection 文本偶发会落在旧 viewport；业务层必须以
    // selectionPosition + 当前 buffer 为准，避免“看到的行”和复制文本不一致。
    expect(renderer.terminal.getSelection()).toBe("current");
  });

  it("viewport selection 使用 absolute row，避免 ghostty public select 在 scrollback 中选错行", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);
    terminal.wasmTerm.scrollbackLength = 80;
    terminal.viewportY = 10;
    terminal.scrollbackLines = Array.from({ length: 80 }, (_, row) =>
      Array.from(`hist-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );
    terminal.screenLines = Array.from({ length: 24 }, (_, row) =>
      Array.from(`screen-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );

    const selected = renderer.terminal.selectViewportRange({ col: 0, row: 5 }, { col: 7, row: 5 });

    // 中文注释：Ghostty public select() 内部把 viewportY 当 absolute row 的起点，
    // scrollback 中会选到错误历史行；termd 必须直接写 selectionManager 的 absolute row。
    expect(terminal.selectCalls).toEqual([]);
    expect(terminal.selectionManager.selectionStart).toEqual({ col: 0, absoluteRow: 75 });
    expect(terminal.selectionManager.selectionEnd).toEqual({ col: 7, absoluteRow: 75 });
    expect(selected).toBe("hist-076");
    expect(renderer.terminal.hasSelection()).toBe(true);
    expect(renderer.terminal.getSelection()).toBe("hist-076");
  });

  it("selectionManager-only 选区也会被 facade 识别，避免 public hasSelection 漏报", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);
    terminal.wasmTerm.scrollbackLength = 80;
    terminal.viewportY = 10;
    terminal.scrollbackLines = Array.from({ length: 80 }, (_, row) =>
      Array.from(`hist-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );
    terminal.screenLines = Array.from({ length: 24 }, (_, row) =>
      Array.from(`screen-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );

    terminal.selectionManager.selectionStart = { col: 0, absoluteRow: 75 };
    terminal.selectionManager.selectionEnd = { col: 7, absoluteRow: 75 };
    terminal.hasSelectionValue = false;

    // 中文注释：这里故意不走 select()/selectionChangedEmitter.fire()，
    // 保证 facade 确实是在 public hasSelection 漏报时回退到 selectionManager。
    expect(terminal.hasSelection()).toBe(false);
    expect(renderer.terminal.hasSelection()).toBe(true);
    expect(renderer.terminal.getSelection()).toBe("hist-076");
  });

  it("deselect 会清掉 selectionManager 持有的选区，避免终端点击后仍残留高亮", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);
    terminal.wasmTerm.scrollbackLength = 80;
    terminal.viewportY = 10;
    terminal.scrollbackLines = Array.from({ length: 80 }, (_, row) =>
      Array.from(`hist-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );
    terminal.screenLines = Array.from({ length: 24 }, (_, row) =>
      Array.from(`screen-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );

    renderer.terminal.selectViewportRange({ col: 0, row: 5 }, { col: 7, row: 5 });
    expect(renderer.terminal.getSelection()).toBe("hist-076");
    terminal.selectionManager.clearSelection.mockClear();

    renderer.terminal.deselect();

    expect(terminal.deselectCalls).toBe(1);
    expect(terminal.selectionManager.clearSelection).toHaveBeenCalledTimes(1);
    expect(terminal.selectionManager.selectionStart).toBeUndefined();
    expect(terminal.selectionManager.selectionEnd).toBeUndefined();
    expect(renderer.terminal.hasSelection()).toBe(false);
    expect(renderer.terminal.getSelection()).toBe("");
  });

  it("debug dataset 复用 facade selection 语义，避免 bridge 与复制链路分裂", async () => {
    vi.resetModules();
    ghosttyWebMock.init.mockClear();
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 14, convertEol: true },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    renderer.terminal.open(host);
    terminal.wasmTerm.scrollbackLength = 80;
    terminal.viewportY = 10;
    terminal.scrollbackLines = Array.from({ length: 80 }, (_, row) =>
      Array.from(`hist-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );
    terminal.screenLines = Array.from({ length: 24 }, (_, row) =>
      Array.from(`screen-${String(row + 1).padStart(3, "0")}`, (ch) => ({ codepoint: ch.codePointAt(0) ?? 0, grapheme_len: 0 })),
    );

    renderer.terminal.selectViewportRange({ col: 0, row: 5 }, { col: 7, row: 5 });
    (terminal as typeof terminal & { nativeSelectionText?: string }).nativeSelectionText = "";
    terminal.renderListeners.forEach((listener) => listener());

    // 中文注释：dataset 必须和 facade 读到的是同一行，不能继续回落到 raw selection 空字符串。
    expect(host.dataset.termdHasSelection).toBe("true");
    expect(host.dataset.termdSelection).toBe("hist-076");
  });

  it("fit 使用完整 host 宽度，不沿用 ghostty-web 的 15px 滚动条预留", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    try {
    ghosttyWebMock.terminals.length = 0;
    const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

    const renderer = await createGhosttyRenderer({
      terminalOptions: { fontSize: 13 },
      searchOptions: {},
    });
    const terminal = ghosttyWebMock.terminals[0];
    const host = document.createElement("div");
    Object.defineProperty(host, "clientWidth", { configurable: true, value: 790 });
    Object.defineProperty(host, "clientHeight", { configurable: true, value: 860 });
    renderer.terminal.open(host);

    // 中文注释：如果继续使用 ghostty-web 默认算法，这里会先扣 15px 得到 96 列；
    // termd 需要按完整 host 计算，让 canvas 尽量铺满终端区域。
    expect(renderer.fit.proposeDimensions()).toEqual({ cols: 98, rows: 61 });

    renderer.fit.fit();

    expect(terminal.cols).toBe(98);
    expect(terminal.rows).toBe(61);
    // 中文注释：98 列实际网格宽 784px，但 host 还有 6px 不足一列的余量。
    // 不能拉伸 canvas，否则字体会被非整数比例重采样；只用背景 filler 遮住这段余量。
    const filler = host.querySelector<HTMLElement>(".terminal-host-grid-filler");
    expect(host.querySelector("canvas")?.style.width).toBe("");
    expect(filler?.style.width).toBe("6px");
    expect(filler?.hidden).toBe(false);
    expect(filler?.getAttribute("aria-hidden")).toBe("true");
    expect(filler?.getAttribute("contenteditable")).toBe("false");
    expect(terminal.renderer.getMetrics().width).toBe(8);

    vi.advanceTimersByTime(60);
    terminal.resize(80, 24);
    renderer.fit.fit();
    expect(terminal.cols).toBe(98);
    expect(terminal.rows).toBe(61);

    Object.defineProperty(host, "clientWidth", { configurable: true, value: 800 });
    renderer.fit.fit();
    expect(terminal.cols).toBe(98);
    expect(terminal.rows).toBe(61);

    // 中文注释：刷新/聚焦时浏览器布局可能在 Ghostty resize 锁期间继续变化；
    // 锁内收到的新尺寸不能丢，否则随后会被外层下一轮调度纠正，看起来像分辨率抖动。
    vi.advanceTimersByTime(60);
    expect(terminal.cols).toBe(100);
    expect(terminal.rows).toBe(61);

    vi.advanceTimersByTime(60);
    Object.defineProperty(host, "clientWidth", { configurable: true, value: 784 });
    renderer.fit.fit();

    // 中文注释：host 缩回整字符宽时 filler 必须隐藏，不能留下旧的右侧遮罩宽度。
    expect(host.querySelector("canvas")?.style.width).toBe("");
    expect(host.querySelector<HTMLElement>(".terminal-host-grid-filler")?.style.width).toBe("0px");
    expect(host.querySelector<HTMLElement>(".terminal-host-grid-filler")?.hidden).toBe(true);
    expect(terminal.renderer.getMetrics().width).toBe(8);
    } finally {
      vi.useRealTimers();
    }
  });

  it("fit 会忽略 reload 初期 ghostty renderer 的临时错误 metrics", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect");
    try {
      ghosttyWebMock.terminals.length = 0;
      const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

      const renderer = await createGhosttyRenderer({
        terminalOptions: { fontSize: 13 },
        searchOptions: {},
      });
      const terminal = ghosttyWebMock.terminals[0];
      const host = document.createElement("div");
      Object.defineProperty(host, "clientWidth", { configurable: true, value: 716 });
      Object.defineProperty(host, "clientHeight", { configurable: true, value: 668 });
      renderer.terminal.open(host);

      rectSpy.mockReturnValue({
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: 640,
        bottom: 336,
        width: 640,
        height: 336,
        toJSON() {
          return this;
        },
      } as DOMRect);
      renderer.fit.fit();
      expect(terminal.cols).toBe(89);
      expect(terminal.rows).toBe(47);

      vi.advanceTimersByTime(60);
      rectSpy.mockReturnValue({
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: 712,
        bottom: 658,
        width: 712,
        height: 658,
        toJSON() {
          return this;
        },
      } as DOMRect);
      terminal.rendererMetrics = { width: 4.8, height: 10, baseline: 8 };
      renderer.fit.fit();

      // 中文注释：真实 reload 曾出现 host/canvas 已经稳定在 89x47，
      // 但 Ghostty renderer 临时 metrics 会把同一块画布误算成 149x66。
      // 此时必须沿用上一次被 canvas 验证过的稳定 metrics。
      expect(terminal.cols).toBe(89);
      expect(terminal.rows).toBe(47);
    } finally {
      rectSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("stable fit 在 metrics 跳变且 canvas 也不匹配旧 metrics 时不通过验证", async () => {
    vi.resetModules();
    vi.useFakeTimers();
    const rectSpy = vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect");
    try {
      ghosttyWebMock.terminals.length = 0;
      const { createGhosttyRenderer } = await import("../components/terminal/ghostty-renderer");

      const renderer = await createGhosttyRenderer({
        terminalOptions: { fontSize: 13 },
        searchOptions: {},
      });
      const terminal = ghosttyWebMock.terminals[0];
      const host = document.createElement("div");
      Object.defineProperty(host, "clientWidth", { configurable: true, value: 716 });
      Object.defineProperty(host, "clientHeight", { configurable: true, value: 668 });
      renderer.terminal.open(host);

      rectSpy.mockReturnValue({
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: 640,
        bottom: 336,
        width: 640,
        height: 336,
        toJSON() {
          return this;
        },
      } as DOMRect);
      renderer.fit.fit();
      expect(terminal.cols).toBe(89);
      expect(terminal.rows).toBe(47);

      vi.advanceTimersByTime(60);
      rectSpy.mockReturnValue({
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: 650,
        bottom: 600,
        width: 650,
        height: 600,
        toJSON() {
          return this;
        },
      } as DOMRect);
      terminal.rendererMetrics = { width: 4.8, height: 10, baseline: 8 };

      // 中文注释：这组 canvas 既解释不了当前错误 metrics，也解释不了上一组稳定 metrics；
      // 因此不能通过 stable proposal 写回 daemon/supervisor，否则刷新期间可能上报虚假分辨率。
      expect(renderer.fit.proposeStableDimensions?.()).toBeUndefined();
    } finally {
      rectSpy.mockRestore();
      vi.useRealTimers();
    }
  });
});
