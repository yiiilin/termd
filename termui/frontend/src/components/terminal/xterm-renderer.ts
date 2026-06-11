import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon, type ISearchOptions } from "@xterm/addon-search";
import { Terminal } from "@xterm/xterm";
import type {
  CreateTerminalRendererOptions,
  TerminalRendererFitAddon,
  TerminalRendererInstance,
  TerminalRendererTerminal,
} from "./renderer";

type XtermBufferRange = ReturnType<Terminal["getSelectionPosition"]>;
type XtermDebugBridge = {
  selectViewportRange: (start: XtermViewportCell, end: XtermViewportCell) => string | undefined;
  getSelection: () => string;
  deselect: () => void;
  hasSelection: () => boolean;
  scrollToLine: (line: number) => void;
};
type XtermViewportCell = { col: number; row: number };
type XtermDebugTerminal = Terminal & {
  __termdDebugBufferSync?: () => void;
};
type XtermAbsoluteRange = { startCol: number; startRow: number; endCol: number; endRow: number };

function clampLine(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function xtermDocumentAnimationFrameUnsafe(): boolean {
  return typeof document !== "undefined" && document.visibilityState === "hidden";
}

function scheduleXtermWriteCallbackRescue(callback: (() => void) | undefined): (() => void) | undefined {
  if (!callback) {
    return undefined;
  }
  let settled = false;
  let rescueTimer: number | undefined;
  const clearRescueTimer = () => {
    if (rescueTimer !== undefined) {
      window.clearTimeout(rescueTimer);
      rescueTimer = undefined;
    }
  };
  const handleVisibilityChange = () => {
    if (document.visibilityState === "hidden") {
      armRescueTimer();
    }
  };
  const handleWindowBlur = () => {
    armRescueTimer();
  };
  const cleanup = () => {
    clearRescueTimer();
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
    }
    if (typeof window !== "undefined") {
      window.removeEventListener("blur", handleWindowBlur);
    }
  };
  const settle = () => {
    if (settled) {
      return;
    }
    settled = true;
    cleanup();
    callback();
  };
  const armRescueTimer = () => {
    if (settled || rescueTimer !== undefined) {
      return;
    }
    // 中文注释：xterm.js 的 write 回调同样属于异步解析完成通知；页面 hidden/blur 时，
    // 浏览器可能冻结驱动它的调度。这里补一个 timer rescue，只兜底 callback 完成，
    // 避免上层 writer queue 永远等待。
    rescueTimer = window.setTimeout(() => {
      rescueTimer = undefined;
      settle();
    }, 0);
  };
  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", handleVisibilityChange);
  }
  if (typeof window !== "undefined") {
    window.addEventListener("blur", handleWindowBlur);
  }
  if (xtermDocumentAnimationFrameUnsafe()) {
    armRescueTimer();
  }
  return settle;
}

function xtermViewportRangeToAbsolute(
  terminal: Terminal,
  start: XtermViewportCell,
  end: XtermViewportCell,
): XtermAbsoluteRange {
  const activeBuffer = terminal.buffer.active;
  const maxRow = Math.max(0, terminal.rows - 1);
  const maxCol = Math.max(0, terminal.cols - 1);
  let startCol = clampLine(Math.floor(start.col), 0, maxCol);
  let endCol = clampLine(Math.floor(end.col), 0, maxCol);
  let startRow = clampLine(Math.floor(start.row), 0, maxRow) + activeBuffer.viewportY;
  let endRow = clampLine(Math.floor(end.row), 0, maxRow) + activeBuffer.viewportY;
  if (startRow > endRow || (startRow === endRow && startCol > endCol)) {
    [startCol, endCol] = [endCol, startCol];
    [startRow, endRow] = [endRow, startRow];
  }
  return { startCol, startRow, endCol, endRow };
}

function xtermAbsoluteRangeText(
  terminal: Terminal,
  range: XtermAbsoluteRange,
): string | undefined {
  const activeBuffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let row = range.startRow; row <= range.endRow; row += 1) {
    const line = activeBuffer.getLine(row);
    if (!line) {
      continue;
    }
    const startColumn = row === range.startRow ? range.startCol : 0;
    const endColumn = row === range.endRow ? range.endCol + 1 : terminal.cols;
    lines.push(line.translateToString(false, startColumn, endColumn).replace(/\s+$/u, ""));
  }
  return lines.join("\n");
}

function xtermViewportRangeText(
  terminal: Terminal,
  start: XtermViewportCell,
  end: XtermViewportCell,
): string | undefined {
  return xtermAbsoluteRangeText(terminal, xtermViewportRangeToAbsolute(terminal, start, end));
}

function xtermLinearSelectionLength(terminal: Terminal, range: XtermAbsoluteRange): number {
  return ((range.endRow - range.startRow) * terminal.cols) + (range.endCol - range.startCol) + 1;
}

function xtermSelectionRangePosition(range: XtermAbsoluteRange): { start: { x: number; y: number }; end: { x: number; y: number } } {
  return {
    start: { x: range.startCol, y: range.startRow },
    end: { x: range.endCol, y: range.endRow },
  };
}

function xtermSelectionPosition(range: XtermBufferRange): { start: { x: number; y: number }; end: { x: number; y: number } } | undefined {
  if (!range) {
    return undefined;
  }
  return {
    start: { x: Math.max(0, range.start.x - 1), y: Math.max(0, range.start.y - 1) },
    end: { x: Math.max(0, range.end.x - 1), y: Math.max(0, range.end.y - 1) },
  };
}

function terminalDebugText(terminal: Terminal): string {
  const activeBuffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let row = 0; row < activeBuffer.length; row += 1) {
    const line = activeBuffer.getLine(row);
    if (!line) {
      continue;
    }
    lines.push(line.translateToString(true));
  }
  return lines.join("\n");
}

function terminalDebugViewportText(terminal: Terminal): string {
  const activeBuffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let row = 0; row < terminal.rows; row += 1) {
    const line = activeBuffer.getLine(activeBuffer.viewportY + row);
    lines.push(line?.translateToString(true) ?? "");
  }
  return lines.join("\n");
}

function installDebugSelectionBridge(
  terminal: Terminal,
  terminalFacade: TerminalRendererTerminal,
): () => void {
  const importMetaEnv = (import.meta as ImportMeta & {
    env?: { MODE?: string; VITE_TERMD_E2E_DEBUG_BUFFER?: string };
  }).env;
  const enabled = importMetaEnv?.MODE === "test" || importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";
  if (!enabled || typeof window === "undefined") {
    return () => undefined;
  }
  const debugTerminal = terminal as XtermDebugTerminal;
  const scope = window as typeof window & {
    __TERMD_DEBUG_TERMINAL__?: XtermDebugBridge;
  };
  const bridge: XtermDebugBridge = {
    selectViewportRange: (start, end) => {
      const selection = terminalFacade.selectViewportRange(start, end);
      debugTerminal.__termdDebugBufferSync?.();
      return selection;
    },
    getSelection: () => terminalFacade.getSelection(),
    deselect: () => {
      terminalFacade.deselect();
      debugTerminal.__termdDebugBufferSync?.();
    },
    hasSelection: () => terminalFacade.hasSelection(),
    scrollToLine: (line) => {
      terminalFacade.scrollToLine(line);
      debugTerminal.__termdDebugBufferSync?.();
    },
  };
  scope.__TERMD_DEBUG_TERMINAL__ = bridge;
  return () => {
    if (scope.__TERMD_DEBUG_TERMINAL__ === bridge) {
      delete scope.__TERMD_DEBUG_TERMINAL__;
    }
  };
}

function installDebugBufferMirror(
  terminal: Terminal,
  terminalFacade: TerminalRendererTerminal,
): () => void {
  const importMetaEnv = (import.meta as ImportMeta & {
    env?: { DEV?: boolean; MODE?: string; VITE_TERMD_E2E_DEBUG_BUFFER?: string };
  }).env;
  const enabled =
    Boolean(importMetaEnv?.DEV) ||
    importMetaEnv?.MODE === "test" ||
    importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";
  const viewportEnabled = importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";
  if (!enabled) {
    return () => undefined;
  }
  const debugTerminal = terminal as XtermDebugTerminal;
  let disposed = false;
  const sync = () => {
    const host = terminalFacade.element;
    if (disposed || !host) {
      return;
    }
    const activeBuffer = terminal.buffer.active;
    host.dataset.termdBuffer = terminalDebugText(terminal);
    host.dataset.termdCols = String(terminal.cols);
    host.dataset.termdRows = String(terminal.rows);
    host.dataset.termdViewportYRaw = String(activeBuffer.viewportY);
    host.dataset.termdScrollbackLength = String(activeBuffer.baseY);
    if (viewportEnabled) {
      host.dataset.termdViewportText = terminalDebugViewportText(terminal);
    }
    host.dataset.termdHasSelection = String(terminalFacade.hasSelection());
    host.dataset.termdSelection = terminalFacade.getSelection();
    host.dataset.termdSelectionPosition = JSON.stringify(terminalFacade.getSelectionPosition?.() ?? null);
  };
  debugTerminal.__termdDebugBufferSync = sync;
  const subscriptions = [
    terminal.onRender(sync),
    terminal.onCursorMove(sync),
    terminal.onScroll(sync),
    terminal.onSelectionChange(sync),
    terminal.onWriteParsed(sync),
  ];
  return () => {
    disposed = true;
    for (const subscription of subscriptions) {
      subscription.dispose();
    }
    if (debugTerminal.__termdDebugBufferSync === sync) {
      delete debugTerminal.__termdDebugBufferSync;
    }
  };
}

function createTermdXtermFitAddon(terminal: Terminal, fitAddon: FitAddon): TerminalRendererFitAddon {
  return {
    fit: () => {
      fitAddon.fit();
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    proposeDimensions: () => fitAddon.proposeDimensions(),
    proposeStableDimensions: () => fitAddon.proposeDimensions(),
  };
}

function createXtermSearchAddon(options: CreateTerminalRendererOptions): SearchAddon {
  return new SearchAddon(options.searchOptions as Partial<{ highlightLimit: number }>);
}

function adaptXtermTerminal(
  terminal: Terminal,
  cleanupDebugBufferMirror: () => void = () => undefined,
): TerminalRendererTerminal {
  let cleanupDebugSelectionBridge: () => void = () => undefined;
  let hostElement: HTMLElement | undefined;
  let manualSelectionText: string | undefined;
  let manualSelectionPosition:
    | {
        start: { x: number; y: number };
        end: { x: number; y: number };
      }
    | undefined;
  const clearManualSelection = () => {
    manualSelectionText = undefined;
    manualSelectionPosition = undefined;
  };
  const setManualSelection = (range: XtermAbsoluteRange) => {
    manualSelectionText = xtermAbsoluteRangeText(terminal, range) ?? "";
    manualSelectionPosition = xtermSelectionRangePosition(range);
  };
  const terminalFacade: TerminalRendererTerminal = {
    get cols() {
      return terminal.cols;
    },
    get rows() {
      return terminal.rows;
    },
    get element() {
      return hostElement ?? terminal.element;
    },
    get textarea() {
      return terminal.textarea;
    },
    get buffer() {
      return terminal.buffer as TerminalRendererTerminal["buffer"];
    },
    get options() {
      return terminal.options as unknown as Record<string, unknown>;
    },
    open: (parent) => {
      hostElement = parent;
      parent.tabIndex = 0;
      parent.setAttribute("role", "textbox");
      parent.setAttribute("aria-label", "Terminal input");
      parent.setAttribute("aria-multiline", "true");
      terminal.open(parent);
      terminal.textarea?.setAttribute("aria-label", "Terminal input");
      cleanupDebugSelectionBridge();
      cleanupDebugSelectionBridge = installDebugSelectionBridge(terminal, terminalFacade);
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    write: (data, callback) => {
      const settle = scheduleXtermWriteCallbackRescue(() => {
        (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
        callback?.();
      });
      terminal.write(data, () => settle?.());
    },
    resize: (cols, rows) => {
      terminal.resize(cols, rows);
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    reset: () => {
      clearManualSelection();
      terminal.reset();
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    refresh: (start, end) => terminal.refresh(start, end),
    focus: () => terminal.focus(),
    scrollToLine: (line) => {
      terminal.scrollToLine(clampLine(Math.floor(line), 0, Math.max(0, terminal.buffer.active.length - 1)));
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    onData: (listener) => terminal.onData(listener),
    onCursorMove: (listener) => terminal.onCursorMove(listener),
    onWriteParsed: (listener) => terminal.onWriteParsed(listener),
    onScroll: (listener) => terminal.onScroll(() => listener()),
    onSelectionChange: (listener) => terminal.onSelectionChange(() => {
      if (!terminal.hasSelection()) {
        clearManualSelection();
      }
      listener();
    }),
    hasSelection: () => Boolean(manualSelectionText?.length) || terminal.hasSelection(),
    getSelection: () => manualSelectionText ?? terminal.getSelection(),
    getSelectionPosition: () => manualSelectionPosition ?? xtermSelectionPosition(terminal.getSelectionPosition()),
    select: (column, row, length) => {
      clearManualSelection();
      terminal.select(column, row, length);
    },
    selectViewportRange: (start, end) => {
      const range = xtermViewportRangeToAbsolute(terminal, start, end);
      terminal.select(range.startCol, range.startRow, xtermLinearSelectionLength(terminal, range));
      setManualSelection(range);
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
      return manualSelectionText;
    },
    getViewportRangeText: (start, end) => xtermViewportRangeText(terminal, start, end),
    deselect: () => {
      clearManualSelection();
      terminal.clearSelection();
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    dispose: () => {
      cleanupDebugSelectionBridge();
      cleanupDebugBufferMirror();
      hostElement = undefined;
      terminal.dispose();
    },
  };
  return terminalFacade;
}

export function createXtermRenderer(options: CreateTerminalRendererOptions): TerminalRendererInstance {
  const terminal = new Terminal(options.terminalOptions);
  const fitAddon = new FitAddon();
  const searchAddon = createXtermSearchAddon(options);
  terminal.loadAddon(fitAddon);
  terminal.loadAddon(searchAddon);
  let cleanupDebugBufferMirror: () => void = () => undefined;
  const terminalFacade = adaptXtermTerminal(terminal, () => cleanupDebugBufferMirror());
  cleanupDebugBufferMirror = installDebugBufferMirror(terminal, terminalFacade);

  return {
    kind: "xterm",
    terminal: terminalFacade,
    fit: createTermdXtermFitAddon(terminal, fitAddon),
    search: {
      clearDecorations: () => searchAddon.clearDecorations(),
      findNext: (query, searchOptions) => {
        searchAddon.findNext(query, searchOptions as ISearchOptions | undefined);
      },
      findPrevious: (query, searchOptions) => {
        searchAddon.findPrevious(query, searchOptions as ISearchOptions | undefined);
      },
    },
    getInputElement: (host) => terminal.textarea ?? host.querySelector<HTMLTextAreaElement>("textarea") ?? undefined,
    isActivationTarget: (target) => Boolean(
      target.closest(".xterm") ||
      target.closest("canvas") ||
      target.closest(".terminal-host") ||
      target.closest(".terminal-frame"),
    ),
    setOptions: (nextOptions) => {
      terminal.options = {
        ...terminal.options,
        ...nextOptions,
      };
      (terminal as XtermDebugTerminal).__termdDebugBufferSync?.();
    },
    scrollState: () => {
      const activeBuffer = terminal.buffer.active;
      return {
        viewportY: activeBuffer.viewportY,
        baseY: activeBuffer.baseY,
        cursorBottomLine: activeBuffer.baseY,
        length: activeBuffer.length,
      };
    },
    syncInputAnchor: ({ recordDiagnostic, reason }) => {
      // 中文注释：xterm.js 自己管理 helper textarea 的焦点和定位；
      // TerminalPane 不再猜测 renderer 私有 DOM，只记录调试口径。
      recordDiagnostic({ reason, mode: "xterm-renderer-owned" });
    },
  };
}
