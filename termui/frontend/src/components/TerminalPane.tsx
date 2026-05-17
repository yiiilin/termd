import { useEffect, useLayoutEffect, useRef, useState, type FormEvent, type MouseEvent, type PointerEvent as ReactPointerEvent } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon, type ISearchOptions } from "@xterm/addon-search";
import { ChevronDown, ChevronUp, ClipboardPaste, GripVertical, Maximize2, RotateCcw, Search, X, ZoomIn, ZoomOut } from "lucide-react";
import type { BrowserMobileShortcut, EffectiveTheme, SessionCursorPresence, SessionSearchResultPayload, TerminalSize } from "../protocol/types";
import { useI18n } from "../i18n";
import { terminalTheme } from "../theme";

const TERMINAL_FONT_SIZE = 13;
const MOBILE_TERMINAL_FONT_SIZE = 12;
const TERMINAL_PADDING_PX = 12;
const TERMINAL_FRAME_BORDER_PX = 1;
const TERMINAL_FRAME_CHROME_PX = TERMINAL_PADDING_PX * 2 + TERMINAL_FRAME_BORDER_PX * 2;
const TERMINAL_CELL_WIDTH_PX = 8.4;
const TERMINAL_LINE_HEIGHT = 1.45;
const MIN_FOCUSED_RESIZE_ROWS = 6;
const MIN_FOCUSED_RESIZE_COLS = 20;
const VIEWER_ZOOM_STEP = 0.1;
const VIEWER_MIN_ZOOM = 0.5;
const VIEWER_MAX_ZOOM = 1.4;
const CURSOR_REPORT_INTERVAL_MS = 120;
const MOBILE_SCROLL_REPORT_INTERVAL_MS = 120;
const FOCUS_OUT_SETTLE_MS = 120;
const MOBILE_DIRECTION_HOLD_MS = 1000;
const MOBILE_DIRECTION_DEAD_ZONE_PX = 24;
const MOBILE_DIRECTION_STEP_PX = 38;
const MOBILE_DIRECTION_REPEAT_MS = 500;
const MOBILE_DIRECTION_TIER_TWO_PX = 56;
const MOBILE_DIRECTION_TIER_THREE_PX = 84;
const MOBILE_DIRECTION_CANCEL_PX = 10;
const TERMINAL_SEARCH_OPTIONS: ISearchOptions = {
  caseSensitive: false,
  decorations: {
    matchBackground: "#a7c080",
    matchBorder: "#a7c080",
    matchOverviewRuler: "#a7c080",
    activeMatchBackground: "#e69875",
    activeMatchBorder: "#e69875",
    activeMatchColorOverviewRuler: "#e69875",
  },
};
type ResizeSource = "layout" | "focus" | "session" | "viewer";
type MobileDirection = "up" | "down" | "left" | "right";
type MobileDirectionTier = 1 | 2 | 3;

const MOBILE_SHORTCUT_KEYS = [
  { label: "Tab", ariaKey: "terminal.sendTab", data: "\t" },
  { label: "Esc", ariaKey: "terminal.sendEscape", data: "\x1b" },
  { label: "^C", ariaKey: "terminal.sendCtrlC", data: "\x03" },
  { label: "^Z", ariaKey: "terminal.sendCtrlZ", data: "\x1a" },
  { label: "^D", ariaKey: "terminal.sendCtrlD", data: "\x04" },
] as const;

function sameTerminalDimensions(
  a: { rows: number; cols: number } | undefined,
  b: { rows: number; cols: number } | undefined,
): boolean {
  return Boolean(a) && Boolean(b) && a!.rows === b!.rows && a!.cols === b!.cols;
}

interface TerminalPaneProps {
  attached: boolean;
  sessionSize?: TerminalSize;
  focusRequest?: number;
  mobileInputMode?: boolean;
  mobileKeyboardOpen?: boolean;
  theme?: EffectiveTheme;
  resizeEnabled?: boolean;
  outputResetVersion: number;
  takeOutput: () => Uint8Array[];
  registerOutputDrain: (drain: () => void) => () => void;
  mobileShortcuts?: BrowserMobileShortcut[];
  onSearch?: (query: string) => Promise<SessionSearchResultPayload>;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
  onCursorChange?: (presence: SessionCursorPresence) => void;
}

export function TerminalPane(props: TerminalPaneProps) {
  const { t } = useI18n();
  const hostRef = useRef<HTMLDivElement | null>(null);
  const scrollportRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const outputResetVersionRef = useRef(props.outputResetVersion);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);
  const onCursorChangeRef = useRef(props.onCursorChange);
  const takeOutputRef = useRef(props.takeOutput);
  const sessionSizeRef = useRef(props.sessionSize);
  const mobileInputModeRef = useRef(Boolean(props.mobileInputMode));
  const resizeEnabledRef = useRef(Boolean(props.resizeEnabled));
  const viewerScaleRef = useRef(1);
  const resizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const stabilizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const drainOutputRef = useRef<() => void>(() => undefined);
  const cursorFrameRef = useRef<number | undefined>(undefined);
  const cursorReportTimerRef = useRef<number | undefined>(undefined);
  const focusOutTimerRef = useRef<number | undefined>(undefined);
  const bottomScrollFrameRef = useRef<number | undefined>(undefined);
  const copyToastTimerRef = useRef<number | undefined>(undefined);
  const lastCursorReportAtRef = useRef(0);
  const mobileScrollFrameRef = useRef<number | undefined>(undefined);
  const mobileScrollTimerRef = useRef<number | undefined>(undefined);
  const lastMobileScrollReportAtRef = useRef(0);
  const mobileScrollDragRef = useRef<{
    pointerId: number;
    startY: number;
    startViewportY: number;
    trackHeight: number;
  } | undefined>(undefined);
  const mobileDirectionGestureRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    lastStepX: number;
    lastStepY: number;
    active: boolean;
    timer: number;
    repeatTimer?: number;
    repeatDirection?: MobileDirection;
    repeatCount: number;
  } | undefined>(undefined);
  const lastNativePasteRef = useRef<{ text: string; atMs: number } | undefined>(undefined);
  const focusedRef = useRef(false);
  const clientSizeRef = useRef<TerminalSize | undefined>(undefined);
  const viewerModeRef = useRef(false);
  const focusActivationArmedRef = useRef(false);
  const suppressPassiveFocusRef = useRef(false);
  const windowActiveRef = useRef(true);
  const viewerAutoFitRef = useRef(true);
  const currentFontSizeRef = useRef(TERMINAL_FONT_SIZE);
  const pendingWriteChunksRef = useRef<Uint8Array[]>([]);
  const pendingWriteBytesRef = useRef(0);
  const writeInFlightRef = useRef(false);
  const writeFrameRef = useRef<number | undefined>(undefined);
  const needsPostWriteRefreshRef = useRef(false);
  const [clientSize, setClientSize] = useState<TerminalSize | undefined>(undefined);
  const [focused, setFocused] = useState(false);
  const [viewerScale, setViewerScale] = useState(1);
  const [copyToastVisible, setCopyToastVisible] = useState(false);
  const [mobileScrollRatio, setMobileScrollRatio] = useState(1);
  const [mobileScrollAvailable, setMobileScrollAvailable] = useState(false);
  const [mobileScrollDragging, setMobileScrollDragging] = useState(false);
  const [viewerViewportSize, setViewerViewportSize] = useState<{ width: number; height: number } | undefined>(undefined);
  const [viewerContentSize, setViewerContentSize] = useState<{ width: number; height: number } | undefined>(undefined);
  const [mobileDirectionActive, setMobileDirectionActive] = useState(false);
  const [mobileDirection, setMobileDirection] = useState<MobileDirection | undefined>();
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchDraft, setSearchDraft] = useState("");
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | undefined>();
  const [searchResult, setSearchResult] = useState<SessionSearchResultPayload | undefined>();
  const [activeSearchIndex, setActiveSearchIndex] = useState(0);
  const scrollToBottom = () => {
    const terminal = terminalRef.current;
    const activeBuffer = terminal?.buffer?.active;
    if (terminal && activeBuffer) {
      terminal.scrollToLine(Math.max(0, activeBuffer.baseY));
    }
    const scrollport = scrollportRef.current;
    if (!scrollport) {
      return;
    }
    scrollport.scrollTop = Math.max(0, scrollport.scrollHeight - scrollport.clientHeight);
  };
  const scheduleScrollToBottom = () => {
    if (bottomScrollFrameRef.current !== undefined) {
      return;
    }
    bottomScrollFrameRef.current = window.requestAnimationFrame(() => {
      bottomScrollFrameRef.current = undefined;
      scrollToBottom();
      // resize 后浏览器会在下一帧才稳定 scrollHeight；再贴底一次避免停在顶部。
      bottomScrollFrameRef.current = window.requestAnimationFrame(() => {
        bottomScrollFrameRef.current = undefined;
        scrollToBottom();
      });
    });
  };
  const showCopyToast = () => {
    setCopyToastVisible(true);
    if (copyToastTimerRef.current !== undefined) {
      window.clearTimeout(copyToastTimerRef.current);
    }
    // 自动复制是瞬时反馈，短暂保留提示即可，避免长期遮挡终端内容。
    copyToastTimerRef.current = window.setTimeout(() => {
      copyToastTimerRef.current = undefined;
      setCopyToastVisible(false);
    }, 1400);
  };
  const fitViewerToScrollport = () => {
    viewerAutoFitRef.current = true;
    setViewerScale((current) => fitScaleForViewer(scrollportRef.current, frameRef.current, current));
    scheduleScrollToBottom();
  };
  const setManualViewerScale = (updater: (scale: number) => number) => {
    viewerAutoFitRef.current = false;
    setViewerScale((current) => updater(current));
  };
  const updateViewerViewportSize = () => {
    const scrollport = scrollportRef.current;
    if (!scrollport) {
      return;
    }
    const next = { width: scrollport.clientWidth, height: scrollport.clientHeight };
    setViewerViewportSize((current) =>
      current && current.width === next.width && current.height === next.height ? current : next,
    );
  };
  const updateViewerContentSize = () => {
    const host = hostRef.current;
    const screen = host?.querySelector<HTMLElement>(".xterm-screen");
    if (!host || !screen) {
      return;
    }
    const next = {
      // xterm 的真实画布宽高由本机字体度量决定，不能直接信任远端 pixel_width。
      width: Math.max(screen.scrollWidth, screen.clientWidth, host.scrollWidth, host.clientWidth),
      height: Math.max(screen.scrollHeight, screen.clientHeight, host.scrollHeight, host.clientHeight),
    };
    if (next.width <= 0 || next.height <= 0) {
      return;
    }
    setViewerContentSize((current) =>
      current && current.width === next.width && current.height === next.height ? current : next,
    );
  };
  const remoteRenderMode = props.attached && !props.resizeEnabled;
  const viewerCols = props.sessionSize?.cols ?? 0;
  const viewerRows = props.sessionSize?.rows ?? 0;
  const viewerPixelWidth = props.sessionSize?.pixel_width ?? 0;
  const viewerPixelHeight = props.sessionSize?.pixel_height ?? 0;
  const viewerContentWidth =
    viewerContentSize?.width ??
    (viewerPixelWidth > 0 ? Math.ceil(viewerPixelWidth) : Math.ceil(viewerCols * TERMINAL_CELL_WIDTH_PX));
  const viewerContentHeight =
    viewerContentSize?.height ??
    (viewerPixelHeight > 0
      ? Math.ceil(viewerPixelHeight)
      : Math.ceil(viewerRows * (props.mobileInputMode ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE) * TERMINAL_LINE_HEIGHT));
  // 只有当前客户端不是 resize owner 且 PTY 尺寸不匹配时，才展示 viewer 虚线框和缩放工具。
  // 独占 session 的客户端即使短暂失焦，也仍按 operator 视图展示，避免移动端出现多余虚线框。
  const resolutionMismatch =
    remoteRenderMode &&
    viewerCols > 0 &&
    viewerRows > 0 &&
    clientSize !== undefined &&
    (clientSize.cols !== viewerCols || clientSize.rows !== viewerRows);
  const effectiveViewerScale = resolutionMismatch ? viewerScale : 1;
  const viewerFrameStyle =
    resolutionMismatch && viewerCols > 0 && viewerRows > 0
      ? {
          // 优先使用聚焦端上报的像素尺寸；缺失时按默认 xterm 字体度量估算 PTY 画布。
          // 缩放交给外层 CSS transform，不改变 xterm fontSize，避免 xterm 内部 screen/viewport
          // 和外层虚线框出现不同步裁切。
          width: `${Math.ceil(viewerContentWidth * effectiveViewerScale) + TERMINAL_FRAME_CHROME_PX}px`,
          height: `${Math.ceil(viewerContentHeight * effectiveViewerScale) + TERMINAL_FRAME_CHROME_PX}px`,
          fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
        }
      : undefined;
  const terminalHostStyle =
    resolutionMismatch && viewerCols > 0 && viewerRows > 0
      ? {
          width: `${viewerContentWidth}px`,
          height: `${viewerContentHeight}px`,
          transform: `scale(${effectiveViewerScale})`,
          transformOrigin: "top left",
        }
      : undefined;

  useEffect(() => {
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onCursorChangeRef.current = props.onCursorChange;
    takeOutputRef.current = props.takeOutput;
    sessionSizeRef.current = props.sessionSize;
    mobileInputModeRef.current = Boolean(props.mobileInputMode);
    resizeEnabledRef.current = Boolean(props.resizeEnabled);
  }, [props.mobileInputMode, props.onCursorChange, props.onInput, props.onResize, props.resizeEnabled, props.sessionSize, props.takeOutput]);

  useEffect(() => props.registerOutputDrain(() => drainOutputRef.current()), [props.registerOutputDrain]);

  useEffect(() => {
    viewerScaleRef.current = viewerScale;
    if (!hasActiveTerminalFocus()) {
      resizeRef.current?.("viewer");
    }
  }, [viewerScale]);

  useEffect(() => {
    // 打开或切换 session 时重新启用自动 Fit，避免沿用上一个会话的手动缩放比例。
    viewerAutoFitRef.current = true;
    setViewerScale(1);
  }, [props.attached, props.sessionSize?.cols, props.sessionSize?.pixel_height, props.sessionSize?.pixel_width, props.sessionSize?.rows]);

  useEffect(() => {
    if (props.mobileInputMode) {
      return;
    }
    setMobileScrollRatio(1);
    setMobileScrollAvailable(false);
    setMobileScrollDragging(false);
  }, [props.mobileInputMode]);

  useEffect(() => {
    resizeRef.current?.(hasActiveTerminalFocus() ? "focus" : "viewer");
  }, [props.resizeEnabled]);

  useLayoutEffect(() => {
    if (!resolutionMismatch || !viewerAutoFitRef.current) {
      return;
    }
    // viewer 的默认语义是“完整看见远端 PTY”，不是按 100% 像素裁切。
    // 用户手动缩放后会关闭 auto-fit；点 Fit 会重新打开。
    setViewerScale((current) => {
      const next = fitScaleForViewer(scrollportRef.current, frameRef.current, current);
      return Math.abs(next - current) < 0.005 ? current : next;
    });
  }, [
    clientSize?.cols,
    clientSize?.pixel_height,
    clientSize?.pixel_width,
    clientSize?.rows,
    resolutionMismatch,
    viewerViewportSize?.height,
    viewerViewportSize?.width,
    viewerCols,
    viewerContentHeight,
    viewerContentWidth,
    viewerPixelHeight,
    viewerPixelWidth,
    viewerRows,
  ]);

  useLayoutEffect(() => {
    if (!resolutionMismatch) {
      return;
    }
    scheduleScrollToBottom();
  }, [effectiveViewerScale, resolutionMismatch, viewerContentHeight, viewerRows]);

  useLayoutEffect(() => {
    if (!props.mobileInputMode) {
      return;
    }
    // 移动端软键盘会改变 visual viewport，但 viewer 模式不会向 daemon 发送 resize。
    // 因此键盘开合时必须主动重测本地可视高度并重新 fit，否则远端 PTY 的底部输入行会停在键盘下方。
    updateViewerViewportSize();
    updateViewerContentSize();
    stabilizeRef.current?.(hasActiveTerminalFocus() ? "focus" : "viewer");
    scheduleScrollToBottom();
  }, [props.mobileInputMode, props.mobileKeyboardOpen]);

  useEffect(() => {
    resizeRef.current?.(focused ? "focus" : "layout");
  }, [focused]);

  useEffect(() => {
    sessionSizeRef.current = props.sessionSize;
    resizeRef.current?.(hasActiveTerminalFocus() ? "session" : "viewer");
  }, [props.sessionSize?.cols, props.sessionSize?.pixel_height, props.sessionSize?.pixel_width, props.sessionSize?.rows]);

  const requestCursorReportFrame = () => {
    if (cursorFrameRef.current !== undefined) {
      return;
    }
    cursorFrameRef.current = window.requestAnimationFrame(() => {
      cursorFrameRef.current = undefined;
      const terminal = terminalRef.current;
      if (!terminal || !onCursorChangeRef.current) {
        return;
      }
      lastCursorReportAtRef.current = nowForThrottle();

      // xterm 内部 cursorX/cursorY 是 0-based；协议用 1-based，便于顶部状态条直接展示。
      // jsdom 测试环境不会完整实现 xterm buffer，缺失时用 1:1 兜底，不影响浏览器真实值。
      const activeBuffer = terminal.buffer?.active;
      onCursorChangeRef.current({
        row: activeBuffer ? activeBuffer.cursorY + 1 : 1,
        col: activeBuffer ? activeBuffer.cursorX + 1 : 1,
        focused: focusedRef.current,
      });
    });
  };

  const queueCursorReport = (options: { immediate?: boolean } = {}) => {
    if (options.immediate) {
      if (cursorReportTimerRef.current !== undefined) {
        window.clearTimeout(cursorReportTimerRef.current);
        cursorReportTimerRef.current = undefined;
      }
      requestCursorReportFrame();
      return;
    }

    const elapsed = nowForThrottle() - lastCursorReportAtRef.current;
    if (elapsed >= CURSOR_REPORT_INTERVAL_MS) {
      requestCursorReportFrame();
      return;
    }
    if (cursorReportTimerRef.current !== undefined) {
      return;
    }
    cursorReportTimerRef.current = window.setTimeout(() => {
      cursorReportTimerRef.current = undefined;
      requestCursorReportFrame();
    }, CURSOR_REPORT_INTERVAL_MS - elapsed);
  };

  const requestMobileScrollFrame = () => {
    if (mobileScrollFrameRef.current !== undefined) {
      return;
    }
    mobileScrollFrameRef.current = window.requestAnimationFrame(() => {
      mobileScrollFrameRef.current = undefined;
      const activeBuffer = terminalRef.current?.buffer?.active;
      const maxViewportY = activeBuffer?.baseY ?? 0;
      const nextRatio = maxViewportY > 0 ? clampNumber((activeBuffer?.viewportY ?? 0) / maxViewportY, 0, 1) : 1;
      const nextAvailable = maxViewportY > 0;
      lastMobileScrollReportAtRef.current = nowForThrottle();
      setMobileScrollAvailable((current) => (current === nextAvailable ? current : nextAvailable));
      setMobileScrollRatio((current) => (Math.abs(current - nextRatio) < 0.003 ? current : nextRatio));
    });
  };

  const scheduleMobileScrollPosition = (options: { immediate?: boolean } = {}) => {
    if (!mobileInputModeRef.current) {
      return;
    }
    if (options.immediate) {
      if (mobileScrollTimerRef.current !== undefined) {
        window.clearTimeout(mobileScrollTimerRef.current);
        mobileScrollTimerRef.current = undefined;
      }
      requestMobileScrollFrame();
      return;
    }

    const elapsed = nowForThrottle() - lastMobileScrollReportAtRef.current;
    if (elapsed >= MOBILE_SCROLL_REPORT_INTERVAL_MS) {
      requestMobileScrollFrame();
      return;
    }
    if (mobileScrollTimerRef.current !== undefined) {
      return;
    }
    mobileScrollTimerRef.current = window.setTimeout(() => {
      mobileScrollTimerRef.current = undefined;
      requestMobileScrollFrame();
    }, MOBILE_SCROLL_REPORT_INTERVAL_MS - elapsed);
  };

  const handleMobileScrollPointerDown = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const activeBuffer = terminalRef.current?.buffer?.active;
    const maxViewportY = activeBuffer?.baseY ?? 0;
    if (!activeBuffer || maxViewportY <= 0) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    event.currentTarget.setPointerCapture(event.pointerId);
    mobileScrollDragRef.current = {
      pointerId: event.pointerId,
      startY: event.clientY,
      startViewportY: activeBuffer.viewportY,
      trackHeight: Math.max(1, scrollportRef.current?.clientHeight ?? event.currentTarget.clientHeight),
    };
    setMobileScrollDragging(true);
  };

  const handleMobileScrollPointerMove = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = mobileScrollDragRef.current;
    const terminal = terminalRef.current;
    const activeBuffer = terminal?.buffer?.active;
    if (!drag || drag.pointerId !== event.pointerId || !terminal || !activeBuffer) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const maxViewportY = activeBuffer.baseY;
    if (maxViewportY <= 0) {
      return;
    }
    // 拖动距离映射到 xterm scrollback 的绝对行号，移动端无需精准触摸浏览器原生滚动条。
    const deltaRatio = (event.clientY - drag.startY) / drag.trackHeight;
    terminal.scrollToLine(clampNumber(Math.round(drag.startViewportY + deltaRatio * maxViewportY), 0, maxViewportY));
    scheduleMobileScrollPosition({ immediate: true });
  };

  const finishMobileScrollDrag = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const drag = mobileScrollDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    mobileScrollDragRef.current = undefined;
    setMobileScrollDragging(false);
    scheduleMobileScrollPosition({ immediate: true });
  };

  const sendTerminalControl = (data: string) => {
    onInputRef.current(data);
    queueCursorReport({ immediate: true });
    if (mobileInputModeRef.current) {
      terminalRef.current?.focus();
    }
  };

  const runSearch = async (event?: FormEvent<HTMLFormElement>) => {
    event?.preventDefault();
    const query = searchDraft.trim();
    if (!query || !props.onSearch) {
      searchAddonRef.current?.clearDecorations();
      return;
    }
    setSearchLoading(true);
    setSearchError(undefined);
    try {
      const result = await props.onSearch(query);
      setSearchResult(result);
      setActiveSearchIndex(0);
      scrollToSearchMatch(result, 0);
      highlightSearchMatches(query, "next");
    } catch {
      setSearchResult(undefined);
      searchAddonRef.current?.clearDecorations();
      setSearchError(t("terminal.searchFailed"));
    } finally {
      setSearchLoading(false);
    }
  };

  const scrollToSearchMatch = (result: SessionSearchResultPayload | undefined, index: number) => {
    const terminal = terminalRef.current;
    const activeBuffer = terminal?.buffer?.active;
    const match = result?.matches[index];
    if (!terminal || !activeBuffer || !match || !result?.line_count) {
      return;
    }
    // daemon 返回的是本次 snapshot 内的行号；前端 xterm buffer 尾部与 snapshot 尾部对齐。
    const firstSnapshotLine = Math.max(0, activeBuffer.length - result.line_count);
    terminal.scrollToLine(clampNumber(firstSnapshotLine + match.line_index, 0, Math.max(0, activeBuffer.length - 1)));
    terminal.focus();
  };

  const stepSearchResult = (direction: 1 | -1) => {
    if (!searchResult || searchResult.matches.length === 0) {
      return;
    }
    const nextIndex = (activeSearchIndex + direction + searchResult.matches.length) % searchResult.matches.length;
    setActiveSearchIndex(nextIndex);
    scrollToSearchMatch(searchResult, nextIndex);
    highlightSearchMatches(searchResult.query, direction > 0 ? "next" : "previous");
  };

  const highlightSearchMatches = (query: string, direction: "next" | "previous") => {
    const trimmed = query.trim();
    if (!trimmed) {
      searchAddonRef.current?.clearDecorations();
      return;
    }
    // daemon 搜索负责跨 snapshot 的结果数量和目标行；xterm search addon 负责真实渲染层高亮。
    // 两者分开可以避免前端手写 xterm DOM 高亮，从而不绑定具体 renderer 结构。
    if (direction === "previous") {
      searchAddonRef.current?.findPrevious(trimmed, TERMINAL_SEARCH_OPTIONS);
      return;
    }
    searchAddonRef.current?.findNext(trimmed, TERMINAL_SEARCH_OPTIONS);
  };

  const keepMobileKeyboardFocused = (event: ReactPointerEvent<HTMLButtonElement>) => {
    // 快捷键按钮位于软键盘上方；阻止按钮抢焦点，尽量让移动端键盘保持打开。
    event.preventDefault();
    event.stopPropagation();
    focusActivationArmedRef.current = true;
    suppressPassiveFocusRef.current = false;
    terminalRef.current?.focus();
  };

  const sendNativePasteText = (text: string) => {
    if (!text) {
      return;
    }
    const now = Date.now();
    const lastPaste = lastNativePasteRef.current;
    if (lastPaste?.text === text && now - lastPaste.atMs < 120) {
      return;
    }
    // 移动端有些浏览器会连续触发 paste 和 beforeinput(insertFromPaste)；
    // 这里去重只覆盖同一次系统粘贴，不影响快捷栏按钮反复粘贴。
    lastNativePasteRef.current = { text, atMs: now };
    sendTerminalControl(text);
  };

  const handlePasteShortcut = async () => {
    try {
      const text = await navigator.clipboard?.readText?.();
      if (text) {
        sendTerminalControl(text);
      } else {
        terminalRef.current?.focus();
      }
    } catch {
      // 剪贴板读取可能被浏览器权限或非安全上下文拒绝；失败时只保持终端焦点。
      terminalRef.current?.focus();
    }
  };

  const sendMobileDirection = (direction: MobileDirection) => {
    const sequences: Record<MobileDirection, string> = {
      up: "\x1b[A",
      down: "\x1b[B",
      right: "\x1b[C",
      left: "\x1b[D",
    };
    sendTerminalControl(sequences[direction]);
    setMobileDirection(direction);
  };

  const directionFromDelta = (deltaX: number, deltaY: number): MobileDirection | undefined => {
    const absX = Math.abs(deltaX);
    const absY = Math.abs(deltaY);
    if (Math.max(absX, absY) < MOBILE_DIRECTION_DEAD_ZONE_PX) {
      return undefined;
    }
    if (absX > absY) {
      return deltaX > 0 ? "right" : "left";
    }
    return deltaY > 0 ? "down" : "up";
  };

  const directionTierFromDelta = (deltaX: number, deltaY: number): MobileDirectionTier | undefined => {
    const distance = Math.max(Math.abs(deltaX), Math.abs(deltaY));
    if (distance < MOBILE_DIRECTION_DEAD_ZONE_PX) {
      return undefined;
    }
    if (distance >= MOBILE_DIRECTION_TIER_THREE_PX) {
      return 3;
    }
    if (distance >= MOBILE_DIRECTION_TIER_TWO_PX) {
      return 2;
    }
    return 1;
  };

  const stopMobileDirectionRepeat = (
    gesture: NonNullable<typeof mobileDirectionGestureRef.current>,
  ) => {
    if (gesture.repeatTimer !== undefined) {
      window.clearInterval(gesture.repeatTimer);
      gesture.repeatTimer = undefined;
    }
    gesture.repeatDirection = undefined;
    gesture.repeatCount = 0;
  };

  const startMobileDirectionRepeat = (
    gesture: NonNullable<typeof mobileDirectionGestureRef.current>,
    direction: MobileDirection,
    repeatCount: 1 | 2,
  ) => {
    setMobileDirection(direction);
    if (gesture.repeatDirection === direction && gesture.repeatCount === repeatCount && gesture.repeatTimer !== undefined) {
      return;
    }
    stopMobileDirectionRepeat(gesture);
    gesture.repeatDirection = direction;
    gesture.repeatCount = repeatCount;
    gesture.repeatTimer = window.setInterval(() => {
      const current = mobileDirectionGestureRef.current;
      if (!current?.active || current.repeatDirection !== direction) {
        return;
      }
      // 一档/二档按固定节奏发送，避免 pointermove 频率直接决定终端光标移动速度。
      for (let index = 0; index < repeatCount; index += 1) {
        sendMobileDirection(direction);
      }
    }, MOBILE_DIRECTION_REPEAT_MS);
  };

  const clearMobileDirectionGesture = () => {
    const gesture = mobileDirectionGestureRef.current;
    if (!gesture) {
      return;
    }
    window.clearTimeout(gesture.timer);
    stopMobileDirectionRepeat(gesture);
    mobileDirectionGestureRef.current = undefined;
    setMobileDirectionActive(false);
    setMobileDirection(undefined);
  };

  const handleMobileDirectionPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!mobileInputModeRef.current || !props.attached || event.pointerType === "mouse") {
      return;
    }
    clearMobileDirectionGesture();
    const pointerId = event.pointerId;
    const startX = event.clientX;
    const startY = event.clientY;
    const timer = window.setTimeout(() => {
      const gesture = mobileDirectionGestureRef.current;
      if (!gesture || gesture.pointerId !== pointerId) {
        return;
      }
      gesture.active = true;
      gesture.lastStepX = startX;
      gesture.lastStepY = startY;
      setMobileDirectionActive(true);
      setMobileDirection(undefined);
      terminalRef.current?.focus();
    }, MOBILE_DIRECTION_HOLD_MS);
    mobileDirectionGestureRef.current = {
      pointerId,
      startX,
      startY,
      lastStepX: startX,
      lastStepY: startY,
      active: false,
      timer,
      repeatCount: 0,
    };
  };

  const handleMobileDirectionPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const gesture = mobileDirectionGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    const deltaX = event.clientX - gesture.startX;
    const deltaY = event.clientY - gesture.startY;
    if (!gesture.active) {
      if (Math.hypot(deltaX, deltaY) > MOBILE_DIRECTION_CANCEL_PX) {
        clearMobileDirectionGesture();
      }
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const direction = directionFromDelta(deltaX, deltaY);
    const tier = directionTierFromDelta(deltaX, deltaY);
    if (!direction || !tier) {
      stopMobileDirectionRepeat(gesture);
      return;
    }
    if (tier === 1 || tier === 2) {
      startMobileDirectionRepeat(gesture, direction, tier);
      return;
    }
    stopMobileDirectionRepeat(gesture);
    const stepDeltaX = event.clientX - gesture.lastStepX;
    const stepDeltaY = event.clientY - gesture.lastStepY;
    if (direction === "left" || direction === "right") {
      if (Math.abs(stepDeltaX) < MOBILE_DIRECTION_STEP_PX) {
        return;
      }
      gesture.lastStepX = event.clientX;
      sendMobileDirection(direction);
      return;
    }
    if (Math.abs(stepDeltaY) < MOBILE_DIRECTION_STEP_PX) {
      return;
    }
    gesture.lastStepY = event.clientY;
    sendMobileDirection(direction);
  };

  const handleMobileDirectionPointerEnd = (event: ReactPointerEvent<HTMLDivElement>) => {
    const gesture = mobileDirectionGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    if (gesture.active) {
      event.preventDefault();
      event.stopPropagation();
      const direction = directionFromDelta(event.clientX - gesture.startX, event.clientY - gesture.startY);
      const tier = directionTierFromDelta(event.clientX - gesture.startX, event.clientY - gesture.startY);
      if (direction && tier === 3 && !mobileDirection) {
        sendMobileDirection(direction);
      }
    }
    clearMobileDirectionGesture();
  };

  const applyFontSize = (terminal: Terminal, fontSize: number) => {
    if (currentFontSizeRef.current === fontSize) {
      return;
    }
    currentFontSizeRef.current = fontSize;
    // xterm 的 cols/rows 属于构造期配置；运行期缩放只更新字体，避免把只读配置一起写回。
    terminal.options = { fontSize };
  };

  const currentTerminalFontSize = () => (mobileInputModeRef.current ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE);

  const isTerminalActivationTarget = (target: EventTarget | null) => {
    const element = target instanceof Element ? target : null;
    return Boolean(element?.closest(".xterm") || element?.closest(".terminal-viewer-frame"));
  };

  const hasActiveTerminalFocus = () => focusedRef.current && windowActiveRef.current;

  const reportTerminalFocus = (nextFocused: boolean) => {
    if (focusedRef.current === nextFocused) {
      return;
    }
    focusedRef.current = nextFocused;
    setFocused(nextFocused);
    if (!nextFocused) {
      suppressPassiveFocusRef.current = true;
    }
    queueCursorReport({ immediate: true });
  };

  const armFocusFromTerminalPointer = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!isTerminalActivationTarget(target)) {
      return;
    }
    // 只有用户明确点到终端渲染区域时，才允许从 viewer 状态重新接管 PTY 尺寸。
    // 缩放后命中目标可能是外层 PTY frame，而不是 xterm 内部节点。
    windowActiveRef.current = true;
    focusActivationArmedRef.current = true;
    suppressPassiveFocusRef.current = false;
  };

  const focusTerminalFromTerminalClick = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!isTerminalActivationTarget(target)) {
      return;
    }
    windowActiveRef.current = true;
    // 点击 viewer frame 是用户显式接管终端的动作；有些浏览器和 jsdom mock
    // 不会把外层 frame 点击稳定转成内部 textarea 的 focusin，因此这里先同步本地聚焦态。
    focusActivationArmedRef.current = false;
    suppressPassiveFocusRef.current = false;
    reportTerminalFocus(true);
    terminalRef.current?.focus();
    resizeRef.current?.("focus");
    // 从 viewer 回到 operator 时，xterm 和外层 scrollport 会连续重排；点击后立即贴底，
    // 避免浏览器把滚动位置恢复到顶部。
    scheduleScrollToBottom();
  };

  useEffect(() => {
    if (!props.attached || !hostRef.current || terminalRef.current) {
      return undefined;
    }

    const terminal = new Terminal({
      cursorBlink: true,
      cursorStyle: "block",
      cursorInactiveStyle: "outline",
      // MVP 只需要普通终端渲染；屏幕阅读模式会额外维持可访问性树，增加高输出场景的内存和 CPU 压力。
      screenReaderMode: false,
      scrollback: 2000,
      fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: props.mobileInputMode ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE,
      convertEol: true,
      theme: terminalTheme(props.theme ?? "dark"),
    });
    const fit = new FitAddon();
    const searchAddon = new SearchAddon({ highlightLimit: 1000 });
    terminal.loadAddon(fit);
    terminal.loadAddon(searchAddon);
    terminal.open(hostRef.current);
    const host = hostRef.current;
    let disposed = false;
    const scheduledFrames = new Set<number>();
    const requestTrackedFrame = (callback: () => void) => {
      const frame = window.requestAnimationFrame(() => {
        scheduledFrames.delete(frame);
        callback();
      });
      scheduledFrames.add(frame);
      return frame;
    };
    const dataSubscription = terminal.onData((data) => {
      onInputRef.current(data);
    });
    const helperTextarea = host.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
    const handleMobileBeforeInput = (event: InputEvent) => {
      if (!mobileInputModeRef.current || event.defaultPrevented) {
        return;
      }

      const text =
        event.inputType === "insertFromPaste" && event.data
          ? event.data
          : event.inputType === "insertText" && (event.data === " " || event.data === ",")
            ? event.data
            : undefined;
      if (!text) {
        return;
      }

      // iOS/Safari 软键盘有时只给 beforeinput，不走 xterm 的 keydown/keypress。
      // 对空格、逗号和粘贴文本做兜底，并阻止后续 input，避免同一份内容发送两次。
      event.preventDefault();
      event.stopPropagation();
      if (event.inputType === "insertFromPaste") {
        sendNativePasteText(text);
        return;
      }
      sendTerminalControl(text);
    };
    const handleMobilePaste = (event: ClipboardEvent) => {
      if (!mobileInputModeRef.current || event.defaultPrevented) {
        return;
      }
      const text = event.clipboardData?.getData("text");
      if (!text) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      sendNativePasteText(text);
    };
    helperTextarea?.addEventListener("beforeinput", handleMobileBeforeInput);
    helperTextarea?.addEventListener("paste", handleMobilePaste);
    const cursorMoveSubscription = terminal.onCursorMove(() => queueCursorReport());
    const writeParsedSubscription = terminal.onWriteParsed(() => queueCursorReport());
    const scrollSubscription = terminal.onScroll(() => scheduleMobileScrollPosition());
    const selectionSubscription = terminal.onSelectionChange(() => {
      if (!terminal.hasSelection()) {
        return;
      }
      const selection = terminal.getSelection();
      if (!selection) {
        return;
      }
      // xterm 原生选择完成后同步复制到系统剪贴板；复制失败时不打断终端交互。
      void navigator.clipboard?.writeText(selection).then(showCopyToast).catch(() => undefined);
    });
    // 本地 xterm 始终适配当前容器；只有聚焦客户端才把尺寸写回 shared PTY。
    // 未聚焦客户端按 session 的远端 rows/cols 渲染，外层 viewer panel 负责缩放与滚动。
    const resize = (source: ResizeSource = "layout") => {
      const terminalHost = hostRef.current;
      if (!terminalHost) {
        return;
      }
      let proposed = fit.proposeDimensions();
      let hostWidth = terminalHost.clientWidth;
      let hostHeight = terminalHost.clientHeight;
      const remoteSize = sessionSizeRef.current;
      const terminalHasActiveFocus = hasActiveTerminalFocus();
      const terminalOwnsResize = resizeEnabledRef.current;
      const hostIsRemoteViewerFrame =
        !terminalOwnsResize &&
        viewerModeRef.current &&
        Boolean(
          remoteSize &&
            proposed &&
            remoteSize.rows === proposed.rows &&
            remoteSize.cols === proposed.cols,
        );
      if (hostIsRemoteViewerFrame && clientSizeRef.current) {
        // viewer 模式下真实 xterm host 会被远端 PTY frame 框住；这时 FitAddon 测到的是
        // 远端画布尺寸，不是浏览器当前可容纳尺寸。继续使用上一轮本地测量值，避免
        // “测到远端尺寸 -> 关闭 viewer -> 又测到本地尺寸 -> 打开 viewer” 的振荡。
        proposed = { rows: clientSizeRef.current.rows, cols: clientSizeRef.current.cols };
        hostWidth = clientSizeRef.current.pixel_width;
        hostHeight = clientSizeRef.current.pixel_height;
      }
      if (proposed) {
        const nextClientSize = {
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostWidth,
          pixel_height: hostHeight,
        };
        clientSizeRef.current = nextClientSize;
        setClientSize((current) =>
          current &&
          current.cols === nextClientSize.cols &&
          current.rows === nextClientSize.rows &&
          current.pixel_width === nextClientSize.pixel_width &&
          current.pixel_height === nextClientSize.pixel_height
            ? current
            : nextClientSize,
        );
      }
      const mismatch = Boolean(
        remoteSize &&
          proposed &&
          (remoteSize.rows !== proposed.rows || remoteSize.cols !== proposed.cols),
      );
      viewerModeRef.current = !terminalOwnsResize && mismatch;
      // 非 resize owner 始终按 daemon 确认的远端尺寸渲染；它可以输入，但不能因为聚焦而接管 PTY 尺寸。
      if (!terminalOwnsResize || !terminalHasActiveFocus) {
        applyFontSize(terminal, currentTerminalFontSize());
        if (remoteSize) {
          if (sameTerminalDimensions(terminal, remoteSize)) {
            updateViewerContentSize();
            scheduleScrollToBottom();
            queueCursorReport({ immediate: true });
            return;
          }
          terminal.resize(remoteSize.cols, remoteSize.rows);
          updateViewerContentSize();
          scheduleScrollToBottom();
          queueCursorReport({ immediate: true });
          return;
        }
        if (!terminalOwnsResize) {
          return;
        }
      }
      applyFontSize(terminal, currentTerminalFontSize());
      // 移动端软键盘或外层 grid 短暂重排时可能把 xterm 容器压到 0 高。
      // 这种尺寸不能写回 shared PTY，否则其他客户端会被同步成一行终端。
      if (proposed && proposed.rows >= MIN_FOCUSED_RESIZE_ROWS && proposed.cols >= MIN_FOCUSED_RESIZE_COLS) {
        viewerModeRef.current = false;
        const approvedBySession =
          remoteSize?.rows === proposed.rows &&
          remoteSize?.cols === proposed.cols;
        if (approvedBySession) {
          if (source === "session" || !sameTerminalDimensions(terminal, proposed)) {
            fit.fit();
          }
          scheduleScrollToBottom();
          queueCursorReport({ immediate: true });
          return;
        }
        // 聚焦状态下只向 daemon 请求新尺寸；在收到 session_resized 并更新
        // sessionSize 之前，不主动调整本地 xterm，避免前端和 daemon 状态分叉。
        onResizeRef.current({
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostWidth,
          pixel_height: hostHeight,
        });
        queueCursorReport({ immediate: true });
      }
    };
    resizeRef.current = resize;
    const refreshTerminal = (source: ResizeSource = "layout") => {
      resize(source);
      terminal.refresh(0, Math.max(0, terminal.rows - 1));
    };
    const stabilizeTerminal = (source: ResizeSource = "layout") => {
      // xterm 在 CSS grid / 右侧文件 panel 同步变化时可能先按旧尺寸完成 open/write。
      // 连续两帧刷新可以等浏览器完成布局后再重算 viewport，避免用户必须额外点击才正常显示。
      requestTrackedFrame(() => {
        refreshTerminal(source);
        requestTrackedFrame(() => refreshTerminal(source));
      });
    };
    const takePendingWrite = () => {
      if (pendingWriteBytesRef.current <= 0) {
        return undefined;
      }
      const chunks = pendingWriteChunksRef.current;
      pendingWriteChunksRef.current = [];
      pendingWriteBytesRef.current = 0;
      return concatTerminalOutputChunks(chunks);
    };
    const afterTerminalWrite = () => {
      if (disposed) {
        return;
      }
      queueCursorReport();
      scheduleMobileScrollPosition();
      if (!needsPostWriteRefreshRef.current) {
        return;
      }
      needsPostWriteRefreshRef.current = false;
      // 首屏或清屏后的首个 write 需要一次轻量 refresh，避免 prompt 等到下一次输入才出现。
      // 持续输出路径不再反复 proposeDimensions/refresh，降低 layout 和绘制压力。
      requestTrackedFrame(() => terminal.refresh(0, Math.max(0, terminal.rows - 1)));
    };
    const flushPendingWrite = () => {
      if (writeInFlightRef.current) {
        return;
      }
      const output = takePendingWrite();
      if (!output || output.byteLength === 0) {
        return;
      }
      writeInFlightRef.current = true;
      terminal.write(output, () => {
        if (disposed) {
          return;
        }
        writeInFlightRef.current = false;
        afterTerminalWrite();
        if (pendingWriteBytesRef.current > 0) {
          schedulePendingWrite();
        }
      });
    };
    function schedulePendingWrite() {
      if (writeInFlightRef.current || writeFrameRef.current !== undefined) {
        return;
      }
      writeFrameRef.current = requestTrackedFrame(() => {
        if (disposed) {
          return;
        }
        writeFrameRef.current = undefined;
        flushPendingWrite();
      });
    }
    const drainOutput = () => {
      const chunks = takeOutputRef.current();
      if (chunks.length === 0) {
        return;
      }
      pendingWriteChunksRef.current.push(...chunks);
      pendingWriteBytesRef.current += chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
      schedulePendingWrite();
    };
    stabilizeRef.current = stabilizeTerminal;
    const clearPendingFocusOut = () => {
      if (focusOutTimerRef.current === undefined) {
        return;
      }
      window.clearTimeout(focusOutTimerRef.current);
      focusOutTimerRef.current = undefined;
    };
    const blurActiveTerminalElement = () => {
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && host.contains(activeElement)) {
        activeElement.blur();
      }
    };
    const handleFocusIn = () => {
      clearPendingFocusOut();
      if (!windowActiveRef.current) {
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        queueCursorReport({ immediate: true });
        return;
      }
      if (suppressPassiveFocusRef.current && !focusActivationArmedRef.current) {
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        queueCursorReport({ immediate: true });
        return;
      }
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = false;
      reportTerminalFocus(true);
      // 主动点击或程序 focus 回到终端时默认看最新输出，尤其覆盖 viewer resize 后的回聚焦路径。
      scheduleScrollToBottom();
    };
    const handleFocusOut = () => {
      focusActivationArmedRef.current = false;
      if (!focusedRef.current || focusOutTimerRef.current !== undefined) {
        return;
      }
      // 浏览器窗口 resize、移动端视觉视口变化和 xterm 内部重排都可能短暂触发
      // focusout -> focusin。延迟确认失焦，避免把这种瞬时 DOM 抖动上报成
      // operator 在 focused/blurred 之间来回切换。
      focusOutTimerRef.current = window.setTimeout(() => {
        focusOutTimerRef.current = undefined;
        reportTerminalFocus(false);
      }, FOCUS_OUT_SETTLE_MS);
    };
    const handleWindowBlur = () => {
      windowActiveRef.current = false;
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = true;
      clearPendingFocusOut();
      // 真实浏览器切到另一个窗口后，旧窗口的 textarea 可能仍留着 DOM focus。
      // 这里立即撤销 operator 聚焦态，避免旧窗口继续按自己的布局上报 PTY resize。
      reportTerminalFocus(false);
      blurActiveTerminalElement();
      resize("viewer");
    };
    const handleWindowFocus = () => {
      windowActiveRef.current = true;
      focusActivationArmedRef.current = false;
      // 回到浏览器窗口不等于用户要接管 PTY；仍需点击终端区域重新激活。
      suppressPassiveFocusRef.current = true;
    };
    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        handleWindowBlur();
        return;
      }
      handleWindowFocus();
    };
    host.addEventListener("focusin", handleFocusIn);
    host.addEventListener("focusout", handleFocusOut);
    window.addEventListener("blur", handleWindowBlur);
    window.addEventListener("focus", handleWindowFocus);
    document.addEventListener("visibilitychange", handleVisibilityChange);
    terminalRef.current = terminal;
    fitRef.current = fit;
    searchAddonRef.current = searchAddon;
    outputResetVersionRef.current = props.outputResetVersion;
    needsPostWriteRefreshRef.current = true;
    // attach 输出可能早于 xterm 初始化到达；创建实例时先取走待写队列，避免首屏输出丢失。
    drainOutputRef.current = drainOutput;
    drainOutput();
    queueCursorReport({ immediate: true });
    scheduleMobileScrollPosition({ immediate: true });
    updateViewerViewportSize();
    updateViewerContentSize();

    // 初次 attach 只做本地 fit；用户聚焦该终端时才接管 shared PTY 的远端尺寸。
    stabilizeTerminal();
    const handleWindowResize = () => resize("layout");
    window.addEventListener("resize", handleWindowResize);
    const resizeObserver =
      typeof ResizeObserver === "undefined"
        ? undefined
        : new ResizeObserver(() => {
          updateViewerViewportSize();
          updateViewerContentSize();
          stabilizeTerminal("layout");
        });
    resizeObserver?.observe(host);
    if (scrollportRef.current) {
      resizeObserver?.observe(scrollportRef.current);
    }

    return () => {
      disposed = true;
      for (const frame of scheduledFrames) {
        window.cancelAnimationFrame(frame);
      }
      scheduledFrames.clear();
      if (cursorFrameRef.current !== undefined) {
        window.cancelAnimationFrame(cursorFrameRef.current);
        cursorFrameRef.current = undefined;
      }
      if (cursorReportTimerRef.current !== undefined) {
        window.clearTimeout(cursorReportTimerRef.current);
        cursorReportTimerRef.current = undefined;
      }
      if (focusOutTimerRef.current !== undefined) {
        window.clearTimeout(focusOutTimerRef.current);
        focusOutTimerRef.current = undefined;
      }
      if (copyToastTimerRef.current !== undefined) {
        window.clearTimeout(copyToastTimerRef.current);
        copyToastTimerRef.current = undefined;
      }
      if (bottomScrollFrameRef.current !== undefined) {
        window.cancelAnimationFrame(bottomScrollFrameRef.current);
        bottomScrollFrameRef.current = undefined;
      }
      lastCursorReportAtRef.current = 0;
      if (mobileScrollFrameRef.current !== undefined) {
        window.cancelAnimationFrame(mobileScrollFrameRef.current);
        mobileScrollFrameRef.current = undefined;
      }
      if (mobileScrollTimerRef.current !== undefined) {
        window.clearTimeout(mobileScrollTimerRef.current);
        mobileScrollTimerRef.current = undefined;
      }
      clearMobileDirectionGesture();
      lastMobileScrollReportAtRef.current = 0;
      window.removeEventListener("resize", handleWindowResize);
      window.removeEventListener("blur", handleWindowBlur);
      window.removeEventListener("focus", handleWindowFocus);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      resizeObserver?.disconnect();
      host.removeEventListener("focusin", handleFocusIn);
      host.removeEventListener("focusout", handleFocusOut);
      helperTextarea?.removeEventListener("beforeinput", handleMobileBeforeInput);
      helperTextarea?.removeEventListener("paste", handleMobilePaste);
      dataSubscription.dispose();
      cursorMoveSubscription.dispose();
      writeParsedSubscription.dispose();
      scrollSubscription.dispose();
      selectionSubscription.dispose();
      terminal.dispose();
      // 清理 host 里的旧 xterm DOM，避免切换 session 后旧终端明文或隐藏 textarea 残留。
      host.replaceChildren();
      terminalRef.current = null;
      fitRef.current = null;
      searchAddonRef.current = null;
      resizeRef.current = undefined;
      stabilizeRef.current = undefined;
      drainOutputRef.current = () => undefined;
      pendingWriteChunksRef.current = [];
      pendingWriteBytesRef.current = 0;
      writeInFlightRef.current = false;
      writeFrameRef.current = undefined;
      needsPostWriteRefreshRef.current = false;
      focusedRef.current = false;
      clientSizeRef.current = undefined;
      viewerModeRef.current = false;
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = true;
      windowActiveRef.current = true;
      setFocused(false);
      setCopyToastVisible(false);
      setMobileScrollRatio(1);
      setMobileScrollAvailable(false);
      setMobileScrollDragging(false);
      setMobileDirectionActive(false);
      setMobileDirection(undefined);
      setViewerViewportSize(undefined);
      setViewerContentSize(undefined);
    };
  }, [props.attached]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    terminal.options = { theme: terminalTheme(props.theme ?? "dark") };
    terminal.refresh(0, Math.max(0, terminal.rows - 1));
  }, [props.theme]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    if (outputResetVersionRef.current === props.outputResetVersion) {
      return;
    }
    outputResetVersionRef.current = props.outputResetVersion;
    pendingWriteChunksRef.current = [];
    pendingWriteBytesRef.current = 0;
    if (writeFrameRef.current !== undefined) {
      window.cancelAnimationFrame(writeFrameRef.current);
      writeFrameRef.current = undefined;
    }
    needsPostWriteRefreshRef.current = true;
    // session 切换时 UI 会重置输出队列；同步清屏，避免旧 session 明文留在终端实例中。
    terminal.clear();
  }, [props.outputResetVersion]);

  useEffect(() => {
    if (!props.attached || !props.focusRequest || !terminalRef.current) {
      return undefined;
    }

    // 新建 session 后要直接进入可输入状态；等一帧可以确保 xterm 已完成 open/fit，
    // focusin 事件随后会关闭 viewer 虚线框，并由聚焦客户端上报真实 PTY 尺寸。
    const frame = window.requestAnimationFrame(() => {
      const activeElement = document.activeElement;
      const terminalHost = hostRef.current;
      if (
        activeElement instanceof HTMLElement &&
        terminalHost &&
        !terminalHost.contains(activeElement) &&
        Boolean(activeElement.closest(".toolbar, .mobile-menu-popover, .mobile-panel, .files-panel"))
      ) {
        // 延迟 focusRequest 不能抢走用户刚聚焦的工作台工具栏、菜单、文件面板等控件；
        // 否则移动端键盘常驻会破坏顶部工具按钮的键盘/辅助技术操作。
        return;
      }
      focusActivationArmedRef.current = true;
      suppressPassiveFocusRef.current = false;
      terminalRef.current?.focus();
      stabilizeRef.current?.("focus");
    });
    return () => window.cancelAnimationFrame(frame);
  }, [props.attached, props.focusRequest]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    applyFontSize(terminal, props.mobileInputMode ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE);
    stabilizeRef.current?.(hasActiveTerminalFocus() ? "focus" : "viewer");
  }, [props.mobileInputMode]);

  return (
    <section
      className={resolutionMismatch ? "terminal-pane terminal-pane-viewer" : "terminal-pane"}
      data-viewer-mode={resolutionMismatch ? "true" : "false"}
      data-testid="terminal-pane"
    >
      {resolutionMismatch ? (
        <div
          className="terminal-viewer-toolbar"
          aria-label={t("terminal.viewerControls")}
          onClick={(event) => event.stopPropagation()}
          onMouseDown={(event) => event.preventDefault()}
        >
          <span className="terminal-viewer-size">{viewerCols && viewerRows ? `${viewerCols}x${viewerRows}` : t("terminal.viewer")}</span>
          <button
            type="button"
            className="icon-button"
            aria-label={t("terminal.zoomOut")}
            title={t("terminal.zoomOut")}
            onClick={() => setManualViewerScale((scale) => clampViewerScale(scale - VIEWER_ZOOM_STEP))}
          >
            <ZoomOut size={15} aria-hidden="true" />
          </button>
          <span className="terminal-viewer-scale">{Math.round(viewerScale * 100)}%</span>
          <button
            type="button"
            className="icon-button"
            aria-label={t("terminal.zoomIn")}
            title={t("terminal.zoomIn")}
            onClick={() => setManualViewerScale((scale) => clampViewerScale(scale + VIEWER_ZOOM_STEP))}
          >
            <ZoomIn size={15} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label={t("terminal.fit")}
            title={t("terminal.fit")}
            onClick={fitViewerToScrollport}
          >
            <Maximize2 size={14} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label={t("terminal.resetZoom")}
            title={t("terminal.resetZoom")}
            onClick={() => setManualViewerScale(() => 1)}
          >
            <RotateCcw size={14} aria-hidden="true" />
          </button>
        </div>
      ) : null}
      <div className="terminal-scrollport" ref={scrollportRef}>
        <div className="terminal-viewer-canvas" ref={canvasRef}>
          <div
            className="terminal-viewer-frame"
            ref={frameRef}
            style={viewerFrameStyle}
            onMouseDownCapture={armFocusFromTerminalPointer}
            onMouseDown={armFocusFromTerminalPointer}
            onClickCapture={focusTerminalFromTerminalClick}
            onPointerDown={handleMobileDirectionPointerDown}
            onPointerMove={handleMobileDirectionPointerMove}
            onPointerUp={handleMobileDirectionPointerEnd}
            onPointerCancel={handleMobileDirectionPointerEnd}
          >
            <div
              className="terminal-host"
              ref={hostRef}
              style={terminalHostStyle}
            />
          </div>
        </div>
      </div>
      {props.attached && props.onSearch ? (
        <div className="terminal-search-control" onClick={(event) => event.stopPropagation()}>
          {searchOpen ? (
            <form className="terminal-search-popover" onSubmit={runSearch}>
              <label>
                <span className="sr-only">{t("terminal.search")}</span>
                <input
                  value={searchDraft}
                  autoFocus
                  placeholder={t("terminal.searchPlaceholder")}
                  onChange={(event) => setSearchDraft(event.currentTarget.value)}
                />
              </label>
              <button type="submit" className="icon-button" aria-label={t("terminal.search")} disabled={searchLoading || !searchDraft.trim()}>
                <Search size={14} aria-hidden="true" />
              </button>
              <button type="button" className="icon-button" aria-label={t("terminal.previousMatch")} disabled={!searchResult?.matches.length} onClick={() => stepSearchResult(-1)}>
                <ChevronUp size={14} aria-hidden="true" />
              </button>
              <button type="button" className="icon-button" aria-label={t("terminal.nextMatch")} disabled={!searchResult?.matches.length} onClick={() => stepSearchResult(1)}>
                <ChevronDown size={14} aria-hidden="true" />
              </button>
              <span className="terminal-search-count" aria-live="polite">
                {searchError ?? (searchResult ? `${searchResult.matches.length ? activeSearchIndex + 1 : 0}/${searchResult.matches.length}${searchResult.truncated ? "+" : ""}` : "")}
              </span>
              <button
                type="button"
                className="icon-button"
                aria-label={t("terminal.closeSearch")}
                onClick={() => {
                  searchAddonRef.current?.clearDecorations();
                  setSearchOpen(false);
                }}
              >
                <X size={14} aria-hidden="true" />
              </button>
            </form>
          ) : (
            <button type="button" className="icon-button terminal-search-button" aria-label={t("terminal.search")} onClick={() => setSearchOpen(true)}>
              <Search size={15} aria-hidden="true" />
            </button>
          )}
        </div>
      ) : null}
      {props.attached && props.mobileInputMode && props.mobileKeyboardOpen ? (
        <div
          className="terminal-mobile-shortcuts"
          aria-label={t("terminal.mobileShortcuts")}
          onClick={(event) => event.stopPropagation()}
        >
          {[...MOBILE_SHORTCUT_KEYS, ...(props.mobileShortcuts ?? [])].map((shortcut) => (
            <button
              type="button"
              key={shortcut.label}
              className="terminal-mobile-shortcut-button"
              aria-label={"ariaKey" in shortcut ? t(shortcut.ariaKey) : shortcut.label}
              title={"ariaKey" in shortcut ? t(shortcut.ariaKey) : shortcut.label}
              onPointerDown={keepMobileKeyboardFocused}
              onClick={(event) => {
                event.preventDefault();
                event.stopPropagation();
                sendTerminalControl(shortcut.data);
              }}
            >
              {shortcut.label}
            </button>
          ))}
          <button
            type="button"
            className="terminal-mobile-shortcut-button terminal-mobile-paste-button"
            aria-label={t("terminal.paste")}
            title={t("terminal.paste")}
            onPointerDown={keepMobileKeyboardFocused}
            onClick={(event) => {
              event.preventDefault();
              event.stopPropagation();
              void handlePasteShortcut();
            }}
          >
            <ClipboardPaste size={13} aria-hidden="true" />
            <span>{t("terminal.paste")}</span>
          </button>
        </div>
      ) : null}
      {mobileDirectionActive ? (
        <div className="terminal-direction-pad" aria-label={t("terminal.mobileDirection")}>
          <span className={mobileDirection === "up" ? "active" : undefined}>↑</span>
          <span className={mobileDirection === "left" ? "active" : undefined}>←</span>
          <span className={mobileDirection === "down" ? "active" : undefined}>↓</span>
          <span className={mobileDirection === "right" ? "active" : undefined}>→</span>
        </div>
      ) : null}
      {copyToastVisible ? (
        <div className="terminal-copy-toast" role="status" aria-live="polite">
          {t("terminal.copied")}
        </div>
      ) : null}
      {props.attached && mobileScrollAvailable ? (
        <div className={mobileScrollDragging ? "terminal-mobile-scroll-track dragging" : "terminal-mobile-scroll-track"}>
          <button
            type="button"
            className="terminal-mobile-scroll-thumb"
            aria-label={t("terminal.scroll")}
            title={t("terminal.scroll")}
            style={{
              top: `${mobileScrollRatio * 100}%`,
              transform: `translateY(-${mobileScrollRatio * 100}%)`,
            }}
            onPointerDown={handleMobileScrollPointerDown}
            onPointerMove={handleMobileScrollPointerMove}
            onPointerUp={finishMobileScrollDrag}
            onPointerCancel={finishMobileScrollDrag}
          >
            <GripVertical size={18} aria-hidden="true" />
          </button>
        </div>
      ) : null}
      {!props.attached ? <div className="terminal-placeholder">{t("status.detached")}</div> : null}
    </section>
  );
}

function clampViewerScale(scale: number): number {
  return Math.min(VIEWER_MAX_ZOOM, Math.max(VIEWER_MIN_ZOOM, Number(scale.toFixed(2))));
}

function clampNumber(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function fitScaleForViewer(scrollport: HTMLElement | null, canvas: HTMLElement | null, currentScale: number): number {
  if (!scrollport || !canvas || scrollport.clientWidth <= 0 || scrollport.clientHeight <= 0 || canvas.offsetWidth <= 0 || canvas.offsetHeight <= 0) {
    return 1;
  }
  const widthScale = (scrollport.clientWidth / canvas.offsetWidth) * currentScale;
  const heightScale = (scrollport.clientHeight / canvas.offsetHeight) * currentScale;
  return clampViewerScale(Math.min(widthScale, heightScale));
}

function concatTerminalOutputChunks(chunks: Uint8Array[]): Uint8Array {
  if (chunks.length === 1) {
    return chunks[0];
  }
  const byteLength = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const output = new Uint8Array(byteLength);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

function nowForThrottle(): number {
  return typeof performance === "undefined" ? Date.now() : performance.now();
}
