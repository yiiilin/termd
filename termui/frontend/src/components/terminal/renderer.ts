import type { TerminalSize } from "../../protocol/types";
import { createXtermRenderer } from "./xterm-renderer";

export type TerminalRendererKind = "xterm";

export interface TerminalRendererDisposable {
  dispose(): void;
}

export interface TerminalRendererBufferState {
  baseY: number;
  cursorX: number;
  cursorY: number;
  viewportY: number;
  length: number;
}

export interface TerminalRendererTerminal {
  cols: number;
  rows: number;
  element?: HTMLElement;
  textarea?: HTMLTextAreaElement;
  buffer?: {
    active?: TerminalRendererBufferState;
  };
  options: Record<string, unknown>;
  open(parent: HTMLElement): void;
  write(data: string | Uint8Array, callback?: () => void): void;
  resize(cols: number, rows: number): void;
  reset(): void;
  refresh(start: number, end: number): void;
  focus(): void;
  scrollToLine(line: number): void;
  onData(listener: (data: string) => void): TerminalRendererDisposable;
  onCursorMove(listener: () => void): TerminalRendererDisposable;
  onWriteParsed(listener: () => void): TerminalRendererDisposable;
  onScroll(listener: () => void): TerminalRendererDisposable;
  onSelectionChange(listener: () => void): TerminalRendererDisposable;
  hasSelection(): boolean;
  getSelection(): string;
  getSelectionPosition?(): { start: { x: number; y: number }; end: { x: number; y: number } } | undefined;
  select(column: number, row: number, length: number): void;
  selectViewportRange(start: { col: number; row: number }, end: { col: number; row: number }): string | undefined;
  getViewportRangeText(start: { col: number; row: number }, end: { col: number; row: number }): string | undefined;
  deselect(): void;
  dispose(): void;
}

export interface TerminalRendererFitAddon {
  fit(): void;
  proposeDimensions(): { cols: number; rows: number } | undefined;
  proposeStableDimensions?(): { cols: number; rows: number } | undefined;
}

export type TerminalSearchOptions = unknown;

export interface TerminalRendererSearchAddon {
  clearDecorations(): void;
  findNext(query: string, options?: TerminalSearchOptions): void;
  findPrevious(query: string, options?: TerminalSearchOptions): void;
}

export interface TerminalRendererInputAnchorOptions {
  host: HTMLElement | null;
  reason: "scroll" | "refresh";
  forcedCursorBottom: boolean;
  bottomEpsilon: number;
  recordDiagnostic: (fields: Record<string, unknown>) => void;
}

export interface TerminalRendererScrollState {
  viewportY: number;
  baseY: number;
  cursorBottomLine: number;
  length: number;
}

export interface TerminalRendererInstance {
  kind: TerminalRendererKind;
  terminal: TerminalRendererTerminal;
  fit: TerminalRendererFitAddon;
  search: TerminalRendererSearchAddon;
  getInputElement(host: HTMLElement): HTMLTextAreaElement | undefined;
  isActivationTarget(target: Element): boolean;
  setOptions(options: Record<string, unknown>): void;
  scrollState(terminal?: TerminalRendererTerminal): TerminalRendererScrollState | undefined;
  syncInputAnchor(options: TerminalRendererInputAnchorOptions): void;
}

export interface CreateTerminalRendererOptions {
  terminalOptions: Record<string, unknown>;
  searchOptions: Record<string, unknown>;
}

export function createTerminalRendererInstance(
  options: CreateTerminalRendererOptions,
): TerminalRendererInstance | Promise<TerminalRendererInstance> {
  // 中文注释：Web 客户端现在只保留 xterm.js renderer，不再保留旧 renderer fallback
  // 或双栈分支，避免终端语义继续分裂。
  return createXtermRenderer(options);
}

export function sameTerminalDimensions(
  a: { rows: number; cols: number } | undefined,
  b: { rows: number; cols: number } | undefined,
): boolean {
  return Boolean(a) && Boolean(b) && a!.rows === b!.rows && a!.cols === b!.cols;
}

export function terminalSizeFromDimensions(
  dimensions: { rows: number; cols: number },
  host: HTMLElement,
): TerminalSize {
  return {
    rows: dimensions.rows,
    cols: dimensions.cols,
    pixel_width: host.clientWidth,
    pixel_height: host.clientHeight,
  };
}
