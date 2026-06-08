import { FitAddon, Terminal, init } from "ghostty-web";
import type {
  CreateTerminalRendererOptions,
  TerminalRendererFitAddon,
  TerminalRendererInstance,
  TerminalRendererTerminal,
} from "./renderer";

let ghosttyInitPromise: Promise<void> | undefined;
let ghosttyInitialized = false;
const patchedGhosttyRenderers = new WeakSet<object>();
const patchedGhosttyMetricRenderers = new WeakSet<object>();
type ViteImportMeta = ImportMeta & {
  env?: {
    DEV?: boolean;
    MODE?: string;
    VITE_TERMD_E2E_DEBUG_BUFFER?: string;
  };
};
const importMetaEnv = (import.meta as ViteImportMeta).env;
const DEBUG_BUFFER_MIRROR_ENABLED =
  Boolean(importMetaEnv?.DEV) ||
  importMetaEnv?.MODE === "test" ||
  importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";
const DEBUG_VIEWPORT_MIRROR_ENABLED = importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";
const DEBUG_SELECTION_BRIDGE_ENABLED =
  importMetaEnv?.MODE === "test" ||
  importMetaEnv?.VITE_TERMD_E2E_DEBUG_BUFFER === "1";

function noopSearch() {
  return {
    clearDecorations: () => undefined,
    findNext: () => undefined,
    findPrevious: () => undefined,
  };
}

function clampLine(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

type GhosttyRuntimeTerminal = InstanceType<typeof Terminal> & {
  getScrollbackLength?: () => number;
  getViewportY?: () => number;
  viewportY?: number;
  wasmTerm?: {
    getScrollbackLength?: () => number;
  };
};

type GhosttyDebugTerminal = InstanceType<typeof Terminal> & {
  __termdDebugBufferSync?: () => void;
};

type GhosttyWindowDebugBridge = {
  selectViewportRange: (start: GhosttyViewportCell, end: GhosttyViewportCell) => string | undefined;
  getSelection: () => string;
  deselect: () => void;
  hasSelection: () => boolean;
};

type GhosttyDisposable = { dispose?: () => void };

type GhosttyFontMetrics = { width: number; height: number; baseline: number };

type GhosttyStableMetricsResult = {
  metrics: GhosttyFontMetrics | undefined;
  stableMetrics: GhosttyFontMetrics | undefined;
  verified: boolean;
};

type GhosttyFitTerminal = InstanceType<typeof Terminal> & {
  requestRender?: () => void;
  renderer?: unknown;
};

type GhosttyPatchedRenderer = {
  getMetrics?: () => GhosttyFontMetrics | undefined;
  setTheme?: (theme: Record<string, unknown>) => void;
  measureFont?: () => GhosttyFontMetrics;
  remeasureFont?: () => void;
  ctx?: Pick<CanvasRenderingContext2D, "font" | "save" | "restore" | "measureText">;
  fontSize?: number;
  fontFamily?: string;
  render?: (
    buffer: unknown,
    forceAll?: boolean,
    viewportY?: number,
    scrollbackProvider?: unknown,
    scrollbarOpacity?: number,
  ) => void;
};

function ghosttyPatchedRenderer(terminal: InstanceType<typeof Terminal>): GhosttyPatchedRenderer | undefined {
  return (terminal as GhosttyFitTerminal).renderer as GhosttyPatchedRenderer | undefined;
}

type GhosttyTerminalOptions = {
  convertEol?: boolean;
};

type GhosttySelectionPosition = {
  start: { x: number; y: number };
  end: { x: number; y: number };
};

type GhosttyViewportCell = { col: number; row: number };

type GhosttySelectionCell = {
  codepoint: number;
  grapheme_len?: number;
};

type GhosttyAbsoluteSelectionRange = {
  startCol: number;
  startAbsoluteRow: number;
  endCol: number;
  endAbsoluteRow: number;
  scrollbackLength: number;
};

type GhosttySelectionManager = {
  selectionStart?: { col: number; absoluteRow: number } | null;
  selectionEnd?: { col: number; absoluteRow: number } | null;
  markCurrentSelectionDirty?: () => void;
  clearSelection?: () => void;
  requestRender?: () => void;
  selectionChangedEmitter?: { fire?: () => void };
};

type GhosttySelectionTerminal = {
  cols: number;
  rows: number;
  viewportY?: number;
  getSelectionPosition?: () => GhosttySelectionPosition | undefined;
  select?: (column: number, row: number, length: number) => void;
  deselect?: () => void;
  getViewportY?: () => number;
  requestRender?: () => void;
  selectionManager?: GhosttySelectionManager;
  wasmTerm?: {
    getScrollbackLength?: () => number;
    getScrollbackLine?: (offset: number) => GhosttySelectionCell[] | null;
    getLine?: (row: number) => GhosttySelectionCell[] | null;
    getGraphemeString?: (row: number, col: number) => string;
    getScrollbackGraphemeString?: (offset: number, col: number) => string;
  };
};

type GhosttyAbsoluteRowCells = {
  cells: GhosttySelectionCell[];
  isScrollbackRow: boolean;
  rowIndex: number;
};

function ghosttyCellsToText(
  cells: GhosttySelectionCell[],
  graphemeAt: (column: number, cell: GhosttySelectionCell) => string,
): string {
  let lineText = "";
  let lastVisibleLength = -1;
  for (let column = 0; column < cells.length; column += 1) {
    const cell = cells[column];
    if (cell && cell.codepoint !== 0) {
      const grapheme = graphemeAt(column, cell);
      lineText += grapheme;
      if (grapheme.trim()) {
        lastVisibleLength = lineText.length;
      }
    } else {
      lineText += " ";
    }
  }
  return lastVisibleLength >= 0 ? lineText.slice(0, lastVisibleLength) : "";
}

function ghosttyAbsoluteRowCells(terminal: GhosttySelectionTerminal, absoluteRow: number): GhosttyAbsoluteRowCells | undefined {
  const wasmTerm = terminal.wasmTerm;
  if (!wasmTerm) {
    return undefined;
  }
  const scrollbackLength = Math.max(0, wasmTerm.getScrollbackLength?.() ?? 0);
  const isScrollbackRow = absoluteRow < scrollbackLength;
  const rowIndex = isScrollbackRow ? absoluteRow : absoluteRow - scrollbackLength;
  const cells = isScrollbackRow ? wasmTerm.getScrollbackLine?.(rowIndex) : wasmTerm.getLine?.(rowIndex);
  if (!cells) {
    return undefined;
  }
  return {
    cells,
    isScrollbackRow,
    rowIndex,
  };
}

function ghosttyAbsoluteRowText(terminal: GhosttySelectionTerminal, absoluteRow: number): string {
  const row = ghosttyAbsoluteRowCells(terminal, absoluteRow);
  const wasmTerm = terminal.wasmTerm;
  if (!row || !wasmTerm) {
    return "";
  }
  return ghosttyCellsToText(row.cells, (column, cell) => {
    if (!cell.grapheme_len || cell.grapheme_len <= 0) {
      return String.fromCodePoint(cell.codepoint);
    }
    return row.isScrollbackRow
      ? wasmTerm.getScrollbackGraphemeString?.(row.rowIndex, column) ?? String.fromCodePoint(cell.codepoint)
      : wasmTerm.getGraphemeString?.(row.rowIndex, column) ?? String.fromCodePoint(cell.codepoint);
  });
}

function ghosttyElementAvailableSize(element: HTMLElement): { width: number; height: number } | undefined {
  const style = window.getComputedStyle(element);
  const paddingLeft = Number.parseInt(style.getPropertyValue("padding-left"), 10) || 0;
  const paddingRight = Number.parseInt(style.getPropertyValue("padding-right"), 10) || 0;
  const paddingTop = Number.parseInt(style.getPropertyValue("padding-top"), 10) || 0;
  const paddingBottom = Number.parseInt(style.getPropertyValue("padding-bottom"), 10) || 0;
  const width = element.clientWidth - paddingLeft - paddingRight;
  const height = element.clientHeight - paddingTop - paddingBottom;
  if (width <= 0 || height <= 0) {
    return undefined;
  }
  return { width, height };
}

function stableGhosttyFontMetrics(
  terminal: InstanceType<typeof Terminal>,
  metrics: GhosttyFontMetrics | undefined,
  previousStableMetrics: GhosttyFontMetrics | undefined,
): GhosttyStableMetricsResult {
  if (!metrics || metrics.width <= 0 || metrics.height <= 0) {
    return { metrics: previousStableMetrics, stableMetrics: previousStableMetrics, verified: Boolean(previousStableMetrics) };
  }

  const canvas = terminal.element?.querySelector<HTMLCanvasElement>("canvas");
  const canvasRect = canvas?.getBoundingClientRect();
  const canValidateCanvasMetrics =
    Boolean(canvasRect) &&
    canvasRect!.width > 0 &&
    canvasRect!.height > 0 &&
    terminal.cols > 0 &&
    terminal.rows > 0;
  if (!canValidateCanvasMetrics) {
    return { metrics, stableMetrics: previousStableMetrics, verified: false };
  }

  const expectedWidth = terminal.cols * metrics.width;
  const expectedHeight = terminal.rows * metrics.height;
  const widthMatches = Math.abs((canvasRect?.width ?? 0) - expectedWidth) <= Math.max(2, metrics.width);
  const heightMatches = Math.abs((canvasRect?.height ?? 0) - expectedHeight) <= Math.max(2, metrics.height);
  if (widthMatches && heightMatches) {
    // 中文注释：只有当 canvas 实际尺寸能被当前 cols/rows 和 renderer metrics 解释时，
    // 才把这组 metrics 记为稳定值；刷新初期 canvas 可能先被 CSS 撑满，不能用它反推字体。
    return { metrics, stableMetrics: metrics, verified: true };
  }

  if (previousStableMetrics) {
    const canvasCellWidth = (canvasRect?.width ?? 0) / terminal.cols;
    const canvasCellHeight = (canvasRect?.height ?? 0) / terminal.rows;
    const canvasMatchesStable =
      Math.abs(canvasCellWidth - previousStableMetrics.width) <= Math.max(1, previousStableMetrics.width * 0.08) &&
      Math.abs(canvasCellHeight - previousStableMetrics.height) <= Math.max(1, previousStableMetrics.height * 0.08);
    if (canvasMatchesStable) {
      // 中文注释：ghostty-web 在 reload/fit 交错时可能短暂返回半成品 metrics，
      // 但 canvas 栅格仍证明真实单元格没有变化；此时继续用稳定 metrics 防止 89x47 -> 149x66 抖动。
      return { metrics: previousStableMetrics, stableMetrics: previousStableMetrics, verified: true };
    }

    const metricJumped =
      Math.abs(metrics.width - previousStableMetrics.width) > previousStableMetrics.width * 0.2 ||
      Math.abs(metrics.height - previousStableMetrics.height) > previousStableMetrics.height * 0.2;
    if (metricJumped) {
      // 中文注释：metrics 大幅跳变时，普通 fit 可以继续沿用上一组已验证 metrics 来避免
      // canvas 视觉抖动；但如果当前 canvas 也不能证明旧 metrics 仍然成立，就不能把它
      // 标记为 verified stable proposal，否则会把未经验证的尺寸写回 daemon/tmux。
      return { metrics: previousStableMetrics, stableMetrics: previousStableMetrics, verified: false };
    }
  }

  return { metrics, stableMetrics: previousStableMetrics, verified: false };
}

function normalizeGhosttyWriteBytes(data: Uint8Array): Uint8Array {
  let extraBytes = 0;
  for (let index = 0; index < data.byteLength; index += 1) {
    if (data[index] === 10) {
      extraBytes += 1;
    }
  }
  if (extraBytes === 0) {
    return data;
  }
  // 中文注释：ghostty-web 只会在 string write 路径里处理 convertEol；
  // 这里把 Uint8Array 也补成同样的 CRLF 语义，避免浏览器看到的内容和选区不一致。
  const normalized = new Uint8Array(data.byteLength + extraBytes);
  let offset = 0;
  for (let index = 0; index < data.byteLength; index += 1) {
    const byte = data[index];
    if (byte === 10) {
      normalized[offset] = 13;
      offset += 1;
    }
    normalized[offset] = byte;
    offset += 1;
  }
  return normalized;
}

function ghosttyViewportRangeToAbsolute(
  terminal: GhosttySelectionTerminal,
  start: GhosttyViewportCell,
  end: GhosttyViewportCell,
): GhosttyAbsoluteSelectionRange | undefined {
  const wasmTerm = terminal.wasmTerm;
  if (!wasmTerm) {
    return undefined;
  }
  const scrollbackLength = Math.max(0, wasmTerm.getScrollbackLength?.() ?? 0);
  const viewportY = Math.max(0, Math.floor(terminal.getViewportY?.() ?? terminal.viewportY ?? 0));
  let startCol = clampLine(Math.floor(start.col), 0, Math.max(0, terminal.cols - 1));
  let endCol = clampLine(Math.floor(end.col), 0, Math.max(0, terminal.cols - 1));
  const startRow = clampLine(Math.floor(start.row), 0, Math.max(0, terminal.rows - 1));
  const endRow = clampLine(Math.floor(end.row), 0, Math.max(0, terminal.rows - 1));
  let startAbsoluteRow = scrollbackLength + startRow - viewportY;
  let endAbsoluteRow = scrollbackLength + endRow - viewportY;
  if (startAbsoluteRow > endAbsoluteRow || (startAbsoluteRow === endAbsoluteRow && startCol > endCol)) {
    [startCol, endCol] = [endCol, startCol];
    [startAbsoluteRow, endAbsoluteRow] = [endAbsoluteRow, startAbsoluteRow];
  }
  return { startCol, startAbsoluteRow, endCol, endAbsoluteRow, scrollbackLength };
}

function ghosttySelectionTextFromAbsoluteRange(
  terminal: GhosttySelectionTerminal,
  range: GhosttyAbsoluteSelectionRange,
): string | undefined {
  const wasmTerm = terminal.wasmTerm;
  if (!wasmTerm) {
    return undefined;
  }
  let text = "";
  for (let absoluteRow = range.startAbsoluteRow; absoluteRow <= range.endAbsoluteRow; absoluteRow += 1) {
    const fullLineText = ghosttyAbsoluteRowText(terminal, absoluteRow);
    const lineStartCol = absoluteRow === range.startAbsoluteRow ? range.startCol : 0;
    const lineEndCol = absoluteRow === range.endAbsoluteRow ? range.endCol + 1 : undefined;
    text += fullLineText.slice(lineStartCol, lineEndCol);
    if (absoluteRow < range.endAbsoluteRow) {
      text += "\n";
    }
  }
  return text;
}

function ghosttySelectionTextFromPosition(terminal: GhosttySelectionTerminal): string | undefined {
  const selection = terminal.getSelectionPosition?.();
  if (!selection) {
    return undefined;
  }
  const range = ghosttyViewportRangeToAbsolute(
    terminal,
    { col: selection.start.x, row: selection.start.y },
    { col: selection.end.x, row: selection.end.y },
  );
  return range ? ghosttySelectionTextFromAbsoluteRange(terminal, range) : undefined;
}

function ghosttySelectionTextFromSelectionManager(terminal: GhosttySelectionTerminal): string | undefined {
  const selectionManager = terminal.selectionManager;
  if (!selectionManager?.selectionStart || !selectionManager.selectionEnd) {
    return undefined;
  }
  const scrollbackLength = Math.max(0, terminal.wasmTerm?.getScrollbackLength?.() ?? 0);
  let startCol = selectionManager.selectionStart.col;
  let endCol = selectionManager.selectionEnd.col;
  let startAbsoluteRow = selectionManager.selectionStart.absoluteRow;
  let endAbsoluteRow = selectionManager.selectionEnd.absoluteRow;
  if (startAbsoluteRow > endAbsoluteRow || (startAbsoluteRow === endAbsoluteRow && startCol > endCol)) {
    [startCol, endCol] = [endCol, startCol];
    [startAbsoluteRow, endAbsoluteRow] = [endAbsoluteRow, startAbsoluteRow];
  }
  return ghosttySelectionTextFromAbsoluteRange(terminal, {
    startCol,
    startAbsoluteRow,
    endCol,
    endAbsoluteRow,
    scrollbackLength,
  });
}

function ghosttyViewportRangeText(
  terminal: GhosttySelectionTerminal,
  start: GhosttyViewportCell,
  end: GhosttyViewportCell,
): string | undefined {
  const range = ghosttyViewportRangeToAbsolute(terminal, start, end);
  return range ? ghosttySelectionTextFromAbsoluteRange(terminal, range) : undefined;
}

function ghosttySelectViewportRange(
  terminal: GhosttySelectionTerminal,
  start: GhosttyViewportCell,
  end: GhosttyViewportCell,
): string | undefined {
  const range = ghosttyViewportRangeToAbsolute(terminal, start, end);
  if (!range) {
    return undefined;
  }
  const selectionManager = terminal.selectionManager;
  if (!selectionManager) {
    terminal.select?.(range.startCol, start.row, Math.abs(end.row * terminal.cols + end.col - (start.row * terminal.cols + start.col)) + 1);
    return ghosttySelectionTextFromAbsoluteRange(terminal, range);
  }
  selectionManager.clearSelection?.();
  selectionManager.selectionStart = { col: range.startCol, absoluteRow: range.startAbsoluteRow };
  selectionManager.selectionEnd = { col: range.endCol, absoluteRow: range.endAbsoluteRow };
  selectionManager.markCurrentSelectionDirty?.();
  selectionManager.requestRender?.();
  terminal.requestRender?.();
  selectionManager.selectionChangedEmitter?.fire?.();
  return ghosttySelectionTextFromAbsoluteRange(terminal, range);
}

function syncGhosttyCanvasFiller(terminal: InstanceType<typeof Terminal>): void {
  const element = terminal.element;
  const hideFiller = () => {
    const filler = element?.querySelector<HTMLDivElement>(".terminal-host-grid-filler");
    if (!filler) {
      return;
    }
    filler.style.width = "0px";
    filler.hidden = true;
  };
  const metrics = ghosttyPatchedRenderer(terminal)?.getMetrics?.();
  if (!element || !metrics || metrics.width <= 0) {
    hideFiller();
    return;
  }
  const available = ghosttyElementAvailableSize(element);
  if (!available) {
    hideFiller();
    return;
  }
  const gridWidth = terminal.cols * metrics.width;
  const leftoverWidth = available.width - gridWidth;
  const fillerWidth = gridWidth > 0 && leftoverWidth > 0 && leftoverWidth < metrics.width ? leftoverWidth : 0;
  let filler = element.querySelector<HTMLDivElement>(".terminal-host-grid-filler");
  if (!filler) {
    filler = document.createElement("div");
    filler.className = "terminal-host-grid-filler";
    filler.setAttribute("aria-hidden", "true");
    filler.setAttribute("contenteditable", "false");
    element.append(filler);
  }
  // 中文注释：不要拉伸 Ghostty 的 canvas；不足一列的右侧余量只用背景块遮住，
  // 保持字形栅格、点击坐标和选择坐标全部沿用 ghostty-web 的原生像素尺度。
  filler.style.width = `${fillerWidth}px`;
  filler.hidden = fillerWidth <= 0;
}

function patchGhosttyRendererViewport(terminal: InstanceType<typeof Terminal>): void {
  const renderer = ghosttyPatchedRenderer(terminal);
  if (!renderer || patchedGhosttyRenderers.has(renderer)) {
    return;
  }
  const originalRender = renderer.render?.bind(renderer);
  if (!originalRender) {
    return;
  }
  renderer.render = (
    buffer: unknown,
    forceAll?: boolean,
    viewportY?: number,
    scrollbackProvider?: unknown,
    scrollbarOpacity?: number,
  ) => {
    // 中文注释：ghostty-web 的 canvas render 会用 viewportY 判断 scrollback/screen 边界，
    // 但只在索引时取 floor；平滑滚动的小数值会让边界行读错并残留旧像素。
    const integerViewportY = viewportY === undefined ? viewportY : Math.max(0, Math.floor(viewportY));
    // 中文注释：滚动条可见性继续交给 Ghostty 自己决定；这里仅修正 fractional viewport
    // 导致的历史内容串行和旧像素残留，不能再把原生滚动条压成透明。
    originalRender(buffer, forceAll, integerViewportY, scrollbackProvider, scrollbarOpacity);
  };
  patchedGhosttyRenderers.add(renderer);
}

function patchGhosttyRendererFontMetrics(terminal: InstanceType<typeof Terminal>): void {
  const renderer = ghosttyPatchedRenderer(terminal);
  if (!renderer || patchedGhosttyMetricRenderers.has(renderer)) {
    return;
  }
  const originalMeasureFont = renderer.measureFont?.bind(renderer);
  const ctx = renderer.ctx;
  if (!originalMeasureFont || !ctx) {
    return;
  }
  renderer.measureFont = () => {
    const original = originalMeasureFont();
    const fontSize = typeof renderer.fontSize === "number" && renderer.fontSize > 0
      ? renderer.fontSize
      : original.height;
    ctx.save();
    try {
      if (fontSize > 0 && typeof renderer.fontFamily === "string" && renderer.fontFamily.length > 0) {
        ctx.font = `${fontSize}px ${renderer.fontFamily}`;
      }
      // 中文注释：ghostty-web upstream 用 "M" 量字高。对 CJK fallback 字体来说，
      // 首行汉字的 actualBoundingBoxAscent 往往比拉丁字母更高，结果只有第 1 行会被
      // canvas 顶边裁掉。这里保留单格宽度仍按拉丁 monospace 计算，只把垂直 metrics
      // 提升到拉丁/CJK 两套样本里的更大值。
      const latin = ctx.measureText("M");
      const cjk = ctx.measureText("中");
      const latinAscent = latin.actualBoundingBoxAscent || fontSize * 0.8;
      const latinDescent = latin.actualBoundingBoxDescent || fontSize * 0.2;
      const cjkAscent = cjk.actualBoundingBoxAscent || latinAscent;
      const cjkDescent = cjk.actualBoundingBoxDescent || latinDescent;
      const extraAscent = Math.max(0, cjkAscent - latinAscent);
      const extraDescent = Math.max(0, cjkDescent - latinDescent);
      return {
        width: original.width,
        height: Math.max(original.height, original.height + Math.ceil(extraAscent + extraDescent)),
        baseline: Math.max(original.baseline, original.baseline + Math.ceil(extraAscent)),
      };
    } finally {
      ctx.restore();
    }
  };
  renderer.remeasureFont?.();
  patchedGhosttyMetricRenderers.add(renderer);
}

function ghosttyMaxViewportY(terminal: GhosttyRuntimeTerminal): number {
  return Math.max(
    0,
    terminal.getScrollbackLength?.() ??
      terminal.wasmTerm?.getScrollbackLength?.() ??
      terminal.buffer?.active?.baseY ??
      0,
  );
}

function ghosttyRendererLineToRawViewport(maxViewportY: number, rendererLine: number): number {
  return clampLine(maxViewportY - rendererLine, 0, maxViewportY);
}

function ghosttyRawViewportToRendererLine(maxViewportY: number, rawViewportFromBottom: number): number {
  return clampLine(maxViewportY - rawViewportFromBottom, 0, maxViewportY);
}

function ghosttyDocumentAnimationFrameUnsafe(): boolean {
  return typeof document !== "undefined" && document.visibilityState === "hidden";
}

function scheduleGhosttyWriteCallbackRescue(callback: (() => void) | undefined): (() => void) | undefined {
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
    // 中文注释：ghostty-web upstream 的 write callback 直接挂在 requestAnimationFrame 上。
    // 页面 hidden 或窗口 blur 之后，这个 callback 可能被浏览器冻结，导致上层 writer
    // 永远停在“有一个 write 在飞行中”。这里补一个 timer rescue，只兜底 callback
    // 的完成通知，不改变 Ghostty 自己的解析/渲染顺序。
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
  if (ghosttyDocumentAnimationFrameUnsafe()) {
    armRescueTimer();
  }
  return settle;
}

function adaptGhosttyTerminal(
  terminal: InstanceType<typeof Terminal>,
  cleanupDebugBufferMirror: () => void = () => undefined,
): TerminalRendererTerminal {
  const runtimeTerminal = terminal as GhosttyRuntimeTerminal;
  const debugTerminal = terminal as GhosttyDebugTerminal;
  const selectionTerminal = terminal as unknown as GhosttySelectionTerminal;
  const fitTerminal = terminal as GhosttyFitTerminal;
  const writeParsedListeners = new Set<() => void>();
  const write = terminal.write.bind(terminal);
  const scrollToLine = terminal.scrollToLine.bind(terminal);
  let cleanupDebugSelectionBridge: () => void = () => undefined;

  // 中文注释：这里必须返回 facade，而不是直接改写 ghostty-web 的 Terminal 实例。
  // Ghostty 自己的 canvas 滚动条会调用原始 scrollToLine（坐标是“距底部距离”）；
  // Termd 业务层使用的统一坐标则由 facade 单独转换，避免拖动方向被反转。
  const terminalFacade: TerminalRendererTerminal = {
    get cols() {
      return terminal.cols;
    },
    get rows() {
      return terminal.rows;
    },
    get element() {
      return terminal.element;
    },
    get textarea() {
      return terminal.textarea;
    },
    get buffer() {
      return terminal.buffer;
    },
    get options() {
      return terminal.options as unknown as Record<string, unknown>;
    },
    // 中文注释：ghostty-web 的 selection 文本在部分浏览器里可能为空或落在旧 viewport；
    // 业务层必须优先用 selectionPosition + 当前 buffer 重建，native 字符串只做兜底。
    getSelection: () => {
      return ghosttySelectionTextFromSelectionManager(selectionTerminal) ??
        ghosttySelectionTextFromPosition(selectionTerminal) ??
        terminal.getSelection() ??
        "";
    },
    getSelectionPosition: () => selectionTerminal.getSelectionPosition?.(),
    hasSelection: () => {
      if (terminal.hasSelection()) {
        return true;
      }
      const selection = selectionTerminal.getSelectionPosition?.();
      return Boolean(selection && (selection.start.x !== selection.end.x || selection.start.y !== selection.end.y));
    },
    select: (column, row, length) => {
      terminal.select(column, row, length);
    },
    selectViewportRange: (start, end) => ghosttySelectViewportRange(selectionTerminal, start, end),
    getViewportRangeText: (start, end) => ghosttyViewportRangeText(selectionTerminal, start, end),
    deselect: () => {
      selectionTerminal.deselect?.();
    },
    open: (parent) => {
      terminal.open(parent);
      patchGhosttyRendererFontMetrics(terminal);
      patchGhosttyRendererViewport(terminal);
      syncGhosttyCanvasFiller(terminal);
      cleanupDebugSelectionBridge();
      cleanupDebugSelectionBridge = installDebugSelectionBridge(terminalFacade, debugTerminal);
    },
    write: (data, callback) => {
      const maxViewportBeforeWrite = ghosttyMaxViewportY(runtimeTerminal);
      const rawViewportFromBottom = Math.max(0, runtimeTerminal.getViewportY?.() ?? runtimeTerminal.viewportY ?? 0);
      const rendererViewportLine = ghosttyRawViewportToRendererLine(maxViewportBeforeWrite, rawViewportFromBottom);
      const normalizedData =
        typeof data !== "string" && Boolean((terminal.options as GhosttyTerminalOptions).convertEol)
          ? normalizeGhosttyWriteBytes(data)
          : data;
      const restoreScrollbackPosition = () => {
        if (rawViewportFromBottom <= 0) {
          return;
        }
        // 中文注释：ghostty-web 当前 write 会在用户上滚时自动 scrollToBottom。
        // 这里恢复的是 renderer-neutral 的绝对行号，而不是旧的“距底部距离”；
        // 否则用户停在历史顶部时，新输出会把视口向后推一行。
        const maxViewportAfterWrite = ghosttyMaxViewportY(runtimeTerminal);
        scrollToLine(ghosttyRendererLineToRawViewport(maxViewportAfterWrite, rendererViewportLine));
      };
      const settleWriteCallback = scheduleGhosttyWriteCallbackRescue(() => {
        restoreScrollbackPosition();
        debugTerminal.__termdDebugBufferSync?.();
        writeParsedListeners.forEach((listener) => listener());
        callback?.();
      });
      write(normalizedData, () => {
        settleWriteCallback?.();
      });
      restoreScrollbackPosition();
    },
    resize: (cols, rows) => {
      terminal.resize(cols, rows);
      syncGhosttyCanvasFiller(terminal);
      debugTerminal.__termdDebugBufferSync?.();
    },
    reset: () => terminal.reset(),
    // 中文注释：ghostty-web 自己维护 canvas render loop，没有 Ghostty 的 refresh API。
    // 如果未来公开 requestRender 就调用它；当前版本保持 no-op，避免业务层依赖 renderer 私有方法。
    refresh: () => {
      syncGhosttyCanvasFiller(terminal);
      fitTerminal.requestRender?.();
    },
    focus: () => terminal.focus(),
    scrollToLine: (line) => {
      const maxViewportY = ghosttyMaxViewportY(runtimeTerminal);
      const terminalLine = clampLine(line, 0, maxViewportY);
      // 中文注释：TerminalPane 使用 renderer-neutral 坐标，baseY 表示底部；ghostty-web
      // 内部 viewportY=0 才是底部，所以写回时必须反向转换。
      scrollToLine(ghosttyRendererLineToRawViewport(maxViewportY, terminalLine));
      // 中文注释：snapshot reveal 后马上选择/复制时，画面、viewport dataset 和 selection
      // 必须已经指向同一行。Ghostty 原生 scroll 事件可能晚一帧，这里主动收敛。
      fitTerminal.requestRender?.();
      syncGhosttyCanvasFiller(terminal);
      debugTerminal.__termdDebugBufferSync?.();
    },
    onData: (listener) => terminal.onData(listener),
    onCursorMove: (listener) => terminal.onCursorMove(listener),
    onWriteParsed: (listener) => {
      writeParsedListeners.add(listener);
      return {
        dispose: () => {
          writeParsedListeners.delete(listener);
        },
      };
    },
    onScroll: (listener) => terminal.onScroll(listener),
    onSelectionChange: (listener) => terminal.onSelectionChange(listener),
    dispose: () => {
      cleanupDebugSelectionBridge();
      cleanupDebugBufferMirror();
      terminal.dispose();
    },
  };
  return terminalFacade;
}

function createTermdGhosttyFitAddon(
  terminal: InstanceType<typeof Terminal>,
  fallbackFit: TerminalRendererFitAddon,
): TerminalRendererFitAddon {
  let isResizing = false;
  let pendingFitAfterResize = false;
  let stableMetrics: GhosttyFontMetrics | undefined;
  let lastMetricsVerified = false;

  const proposeDimensionsFromMetrics = (requireVerifiedMetrics: boolean) => {
    const element = terminal.element;
    const metricsResult = stableGhosttyFontMetrics(
      terminal,
      ghosttyPatchedRenderer(terminal)?.getMetrics?.(),
      stableMetrics,
    );
    stableMetrics = metricsResult.stableMetrics;
    lastMetricsVerified = metricsResult.verified;
    const metrics = metricsResult.metrics;
    if (!element || !metrics || metrics.width <= 0 || metrics.height <= 0 || typeof element.clientWidth === "undefined") {
      return requireVerifiedMetrics ? undefined : fallbackFit.proposeDimensions();
    }
    if (requireVerifiedMetrics && !metricsResult.verified) {
      return undefined;
    }
    const available = ghosttyElementAvailableSize(element);
    if (!available) {
      return requireVerifiedMetrics ? undefined : fallbackFit.proposeDimensions();
    }
    // 中文注释：ghostty-web 自带 FitAddon 会为“原生滚动条”额外扣 15px；
    // 但当前 Ghostty 滚动条画在 canvas 内部，扣宽会让 canvas 右侧露出一条空白。
    return {
      cols: Math.max(2, Math.floor(available.width / metrics.width)),
      rows: Math.max(1, Math.floor(available.height / metrics.height)),
    };
  };

  const proposeDimensions = () => proposeDimensionsFromMetrics(false);
  const proposeStableDimensions = () => {
    const proposed = proposeDimensionsFromMetrics(true);
    if (!proposed || !lastMetricsVerified) {
      return undefined;
    }
    return proposed;
  };

  const fit = () => {
    if (isResizing) {
      // 中文注释：浏览器刷新/聚焦时布局可能在 Ghostty resize 锁期间继续变化；
      // 这里不能直接丢掉新 fit 请求，解锁后必须补跑一次拿到最终稳定尺寸。
      pendingFitAfterResize = true;
      syncGhosttyCanvasFiller(terminal);
      return;
    }
    const proposed = proposeDimensions();
    if (!proposed) {
      return;
    }
    if (proposed.cols === terminal.cols && proposed.rows === terminal.rows) {
      syncGhosttyCanvasFiller(terminal);
      return;
    }
    isResizing = true;
    try {
      terminal.resize(proposed.cols, proposed.rows);
      syncGhosttyCanvasFiller(terminal);
      (terminal as GhosttyDebugTerminal).__termdDebugBufferSync?.();
    } finally {
      window.setTimeout(() => {
        isResizing = false;
        if (!pendingFitAfterResize) {
          return;
        }
        pendingFitAfterResize = false;
        // 中文注释：补跑时重新读取 host/client metrics，而不是复用锁内的旧 proposed。
        // 这样最后一次真实布局尺寸不会被前一帧的 resize 锁吞掉。
        fit();
      }, 50);
    }
  };

  return {
    fit,
    proposeDimensions,
    proposeStableDimensions,
  };
}

function terminalDebugText(terminal: InstanceType<typeof Terminal>): string {
  const rawViewportY = Math.max(0, terminal.getViewportY?.() ?? terminal.viewportY ?? 0);
  if (rawViewportY > 0) {
    // 中文注释：Ghostty 在 scrollback 视图里暴露的“整缓冲区”文本并不稳定，
    // 会把可见 viewport 和底部最新屏幕拼在一起。dev/test 里用户真正关心的是
    // 当前眼前这一屏，所以离开底部后改为镜像 viewport 文本，避免诊断数据自相矛盾。
    return terminalDebugViewportText(terminal);
  }
  const activeBuffer = terminal.buffer?.active;
  if (!activeBuffer) {
    return "";
  }
  if (typeof activeBuffer.getLine !== "function") {
    return terminal.element?.dataset.buffer ?? "";
  }
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

function terminalDebugViewportText(terminal: InstanceType<typeof Terminal>): string {
  const selectionTerminal = terminal as unknown as GhosttySelectionTerminal;
  const rows = Math.max(0, terminal.rows);
  const cols = Math.max(0, terminal.cols);
  const lines: string[] = [];
  for (let row = 0; row < rows; row += 1) {
    lines.push(ghosttyViewportRangeText(selectionTerminal, { col: 0, row }, { col: Math.max(0, cols - 1), row }) ?? "");
  }
  return lines.join("\n");
}

function installDebugSelectionBridge(
  terminalFacade: TerminalRendererTerminal,
  debugTerminal: GhosttyDebugTerminal,
): () => void {
  if (!DEBUG_SELECTION_BRIDGE_ENABLED || typeof window === "undefined") {
    return () => undefined;
  }
  const scope = window as typeof window & {
    __TERMD_DEBUG_GHOSTTY__?: GhosttyWindowDebugBridge;
  };
  const bridge: GhosttyWindowDebugBridge = {
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
  };
  // 中文注释：这个桥只在 test/E2E debug 构建里暴露，给 Playwright 稳定建立
  // Ghostty 选区用；正常 dev/prod 页面都不挂到 window，避免额外公开终端控制面。
  scope.__TERMD_DEBUG_GHOSTTY__ = bridge;
  return () => {
    if (scope.__TERMD_DEBUG_GHOSTTY__ === bridge) {
      delete scope.__TERMD_DEBUG_GHOSTTY__;
    }
  };
}

function installDebugBufferMirror(terminal: InstanceType<typeof Terminal>): () => void {
  const debugTerminal = terminal as GhosttyDebugTerminal;
  let disposed = false;
  const sync = () => {
    if (disposed || !terminal.element) {
      return;
    }
    const rawViewportY = Math.max(
      0,
      terminal.getViewportY?.() ?? terminal.viewportY ?? 0,
    );
    const scrollbackLength = ghosttyMaxViewportY(terminal as GhosttyRuntimeTerminal);
    // 中文注释：该镜像只在 test/dev 或显式 E2E 构建中启用；正常 production build
    // 不把 E2EE 解密后的终端明文复制到 DOM 属性，避免扩大明文暴露面。
    terminal.element.dataset.termdBuffer = terminalDebugText(terminal);
    terminal.element.dataset.termdCols = String(terminal.cols);
    terminal.element.dataset.termdRows = String(terminal.rows);
    terminal.element.dataset.termdViewportYRaw = String(rawViewportY);
    terminal.element.dataset.termdScrollbackLength = String(scrollbackLength);
    if (DEBUG_VIEWPORT_MIRROR_ENABLED) {
      // 中文注释：只在 E2E 构建启用；它把当前 viewport 的文本坐标固定下来，
      // 方便浏览器测试确认“画面坐标”和“拖拽复制坐标”没有分裂。Vitest 高输出
      // 用例会写入超长单行，不能在每次 render 时重建这份 viewport 文本。
      terminal.element.dataset.termdViewportText = terminalDebugViewportText(terminal);
    }
    terminal.element.dataset.termdHasSelection = String(Boolean(terminal.hasSelection?.()));
    terminal.element.dataset.termdSelection = terminal.getSelection?.() ?? "";
    terminal.element.dataset.termdSelectionPosition = JSON.stringify(terminal.getSelectionPosition?.() ?? null);
  };
  debugTerminal.__termdDebugBufferSync = sync;
  const subscriptions: GhosttyDisposable[] = [
    terminal.onRender(sync),
    terminal.onCursorMove(sync),
    terminal.onScroll(sync),
    terminal.onSelectionChange(sync),
  ];
  return () => {
    disposed = true;
    for (const subscription of subscriptions) {
      subscription.dispose?.();
    }
    if (debugTerminal.__termdDebugBufferSync === sync) {
      delete debugTerminal.__termdDebugBufferSync;
    }
  };
}

function ensureGhosttyInitialized(): Promise<void> | undefined {
  if (ghosttyInitialized) {
    return undefined;
  }
  if (!ghosttyInitPromise) {
    const initResult = init() as Promise<void> | void;
    if (initResult && typeof initResult.then === "function") {
      ghosttyInitPromise = initResult.then(() => {
        ghosttyInitialized = true;
      });
    } else {
      // 中文注释：测试环境的 ghostty-web mock 不需要加载 WASM，可同步完成初始化；
      // 真实浏览器仍走上面的 Promise 路径。
      ghosttyInitialized = true;
    }
  }
  return ghosttyInitialized ? undefined : ghosttyInitPromise;
}

function buildGhosttyRenderer(options: CreateTerminalRendererOptions): TerminalRendererInstance {
  const terminal = new Terminal(options.terminalOptions);
  const upstreamFit = new FitAddon();
  terminal.loadAddon(upstreamFit);
  const fit = createTermdGhosttyFitAddon(terminal, upstreamFit);
  const cleanupDebugBufferMirror = DEBUG_BUFFER_MIRROR_ENABLED
    ? installDebugBufferMirror(terminal)
    : () => undefined;
  const adaptedTerminal = adaptGhosttyTerminal(terminal, cleanupDebugBufferMirror);

  return {
    kind: "ghostty",
    terminal: adaptedTerminal,
    fit,
    search: noopSearch(),
    getInputElement: (host) => terminal.textarea ?? host.querySelector<HTMLTextAreaElement>("textarea") ?? undefined,
    isActivationTarget: (target) => Boolean(target.closest("canvas") || target.closest(".terminal-frame")),
    setOptions: (nextOptions) => {
      const terminalOptions = terminal.options as unknown as Record<string, unknown>;
      for (const [key, value] of Object.entries(nextOptions)) {
        if (key === "theme") {
          // 中文注释：ghostty-web 的运行期 theme setter 不能重写 WASM buffer 里的默认颜色。
          // App/TerminalPane 已改为通过 outputResetVersion 重建 Ghostty；renderer contract
          // 这里保持安全 no-op，避免未来调用方误以为局部 setOptions 能完整换色。
          continue;
        }
        terminalOptions[key] = value;
      }
    },
    scrollState: () => {
      const runtimeTerminal = terminal as GhosttyRuntimeTerminal;
      const maxViewportY = ghosttyMaxViewportY(runtimeTerminal);
      const ghosttyViewportFromBottom = Math.max(0, runtimeTerminal.getViewportY?.() ?? runtimeTerminal.viewportY ?? 0);
      const viewportY = ghosttyRawViewportToRendererLine(maxViewportY, ghosttyViewportFromBottom);
      return {
        viewportY,
        baseY: maxViewportY,
        cursorBottomLine: maxViewportY,
        length: maxViewportY + terminal.rows,
      };
    },
    syncInputAnchor: ({ recordDiagnostic, reason }) => {
      // 中文注释：ghostty-web 通过 canvas 渲染，不暴露 Ghostty 的 rows DOM；
      // 输入锚点由 renderer 自己维护，TerminalPane 不再猜测私有结构。
      recordDiagnostic({ reason, mode: "ghostty-renderer-owned" });
    },
  };
}

export function createGhosttyRenderer(
  options: CreateTerminalRendererOptions,
): TerminalRendererInstance | Promise<TerminalRendererInstance> {
  const initResult = ensureGhosttyInitialized();
  if (initResult) {
    return initResult.then(() => buildGhosttyRenderer(options));
  }
  return buildGhosttyRenderer(options);
}
