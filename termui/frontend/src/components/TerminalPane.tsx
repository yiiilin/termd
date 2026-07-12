import { useEffect, useLayoutEffect, useRef, useState, type FormEvent, type MouseEvent, type PointerEvent as ReactPointerEvent } from "react";
import { ChevronDown, ChevronUp, ClipboardPaste, Copy, Search, X } from "lucide-react";
import type { BrowserMobileShortcut, EffectiveTheme, SessionSearchResultPayload, TerminalSize } from "../protocol/types";
import { useI18n } from "../i18n";
import { terminalTheme } from "../theme";
import type { TerminalOutputItem, TerminalResyncOptions } from "./terminal/types";
import { useTerminalOutputWriter } from "./terminal/useTerminalOutputWriter";
import { useTerminalFocusResizeState } from "./terminal/useTerminalFocusResize";
import { recordTermdDiagnostic } from "../diagnostics";
import {
  createTerminalRendererInstance,
  sameTerminalDimensions,
  type TerminalRendererFitAddon,
  type TerminalRendererInstance,
  type TerminalRendererSearchAddon,
  type TerminalRendererTerminal,
  type TerminalSearchOptions,
} from "./terminal/renderer";

export type { TerminalOutputItem } from "./terminal/types";

const TERMINAL_FONT_SIZE = 13;
const MOBILE_TERMINAL_FONT_SIZE = 12;
const MIN_FOCUSED_RESIZE_ROWS = 6;
const MIN_FOCUSED_RESIZE_COLS = 20;
const CURSOR_REPORT_INTERVAL_MS = 120;
const TERMINAL_SCROLL_REPORT_INTERVAL_MS = 120;
const FOCUS_OUT_SETTLE_MS = 120;
const MOBILE_PASSIVE_FOCUS_BYPASS_SETTLE_MS = 180;
const MOBILE_DIRECTION_HOLD_MS = 1000;
const MOBILE_DIRECTION_DEAD_ZONE_PX = 24;
const MOBILE_DIRECTION_STEP_PX = 38;
const MOBILE_DIRECTION_REPEAT_MS = 500;
const MOBILE_DIRECTION_TIER_TWO_PX = 56;
const MOBILE_DIRECTION_TIER_THREE_PX = 84;
const MOBILE_DIRECTION_CANCEL_PX = 10;
const MOBILE_SELECTION_LONG_PRESS_MS = 600;
const MOBILE_SCROLL_DEAD_ZONE_PX = 12;
const MOBILE_SCROLL_STEP_DIVISOR = 1.2;
const MOBILE_COMPOSITION_SETTLE_MS = 80;
const MOBILE_KEYBOARD_RESIZE_SUPPRESS_MS = 700;
const TERMINAL_BOTTOM_EPSILON = 1;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_MIN_BYTES = 8 * 1024;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_USER_MIN_BYTES = 1024;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_COOLDOWN_MS = 5_000;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS = 1_000;
const TERMINAL_SELECTION_DRAG_THRESHOLD_PX = 4;
const TERMINAL_SURFACE_SELECTOR_FALLBACKS = [
  "canvas",
  ".xterm-screen",
  ".xterm-viewport",
  ".xterm",
] as const;
const TERMINAL_SEARCH_OPTIONS: TerminalSearchOptions = {
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
type ResizeSource = "layout" | "focus" | "session" | "snapshot" | "mobile-viewport";
const RESIZE_SOURCE_PRIORITY: Record<ResizeSource, number> = {
  snapshot: 0,
  session: 1,
  "mobile-viewport": 1,
  layout: 2,
  focus: 3,
};
type MobileDirection = "up" | "down" | "left" | "right";
type MobileDirectionTier = 1 | 2 | 3;

interface DeferredTerminalFrameHandle {
  id: number;
  cancel: () => void;
  rescueHidden: (force?: boolean) => void;
}

function resolveTerminalSurfaceElement(host: HTMLElement | null | undefined): HTMLElement | undefined {
  if (!host) {
    return undefined;
  }
  for (const selector of TERMINAL_SURFACE_SELECTOR_FALLBACKS) {
    const surface = host.querySelector<HTMLElement>(selector);
    if (surface) {
      return surface;
    }
  }
  return undefined;
}

function isDocumentAnimationFrameUnsafe(): boolean {
  return typeof document !== "undefined" && (
    document.visibilityState === "hidden" ||
    (typeof document.hasFocus === "function" && !document.hasFocus())
  );
}

function serverScrollableOutputBytes(bytes: Uint8Array): number {
  for (const byte of bytes) {
    if (byte === 10 || byte === 13 || byte === 27 || (byte >= 32 && byte !== 127)) {
      return bytes.byteLength;
    }
  }
  return 0;
}

const MOBILE_SHORTCUT_KEYS = [
  { label: "Tab", ariaKey: "terminal.sendTab", data: "\t" },
  { label: "Esc", ariaKey: "terminal.sendEscape", data: "\x1b" },
  { label: "^C", ariaKey: "terminal.sendCtrlC", data: "\x03" },
  { label: "^Z", ariaKey: "terminal.sendCtrlZ", data: "\x1a" },
  { label: "^D", ariaKey: "terminal.sendCtrlD", data: "\x04" },
] as const;

interface TerminalPaneProps {
  attached: boolean;
  sessionSize?: TerminalSize;
  focusRequest?: number;
  mobileInputMode?: boolean;
  mobileKeyboardOpen?: boolean;
  mobileViewportWidth?: number;
  mobileViewportHeight?: number;
  mobileViewportOffsetTop?: number;
  theme?: EffectiveTheme;
  outputResetVersion: number;
  takeOutput: () => TerminalOutputItem[];
  registerOutputDrain: (drain: () => void) => () => void;
  onOutputResetApplied?: (version: number) => void;
  onTerminalResync?: (lastTerminalSeq?: number, options?: TerminalResyncOptions) => void;
  onTerminalSeqRendered?: (terminalSeq: number) => void;
  onTerminalSizeRendered?: (size: TerminalSize) => void;
  mobileShortcuts?: BrowserMobileShortcut[];
  onSearch?: (query: string) => Promise<SessionSearchResultPayload>;
  onInput: (data: string) => void;
  onResize: (size: TerminalSize) => void;
  /** @deprecated Cursor is derived locally from snapshot and PTY output. */
  onCursorChange?: (presence: { row: number; col: number; focused: boolean }) => void;
}

interface FocusTerminalInputSinkOptions {
  force?: boolean;
}

export function TerminalPane(props: TerminalPaneProps) {
  const { t } = useI18n();
  const hostRef = useRef<HTMLDivElement | null>(null);
  const scrollportRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<TerminalRendererTerminal | null>(null);
  const fitRef = useRef<TerminalRendererFitAddon | null>(null);
  const searchAddonRef = useRef<TerminalRendererSearchAddon | null>(null);
  const rendererRef = useRef<TerminalRendererInstance | null>(null);
  const searchRequestSeqRef = useRef(0);
  const outputResetVersionRef = useRef(props.outputResetVersion);
  const attachedRef = useRef(props.attached);
  const onInputRef = useRef(props.onInput);
  const onResizeRef = useRef(props.onResize);
  const onTerminalResyncRef = useRef(props.onTerminalResync);
  const onTerminalSeqRenderedRef = useRef(props.onTerminalSeqRendered);
  const onTerminalSizeRenderedRef = useRef(props.onTerminalSizeRendered);
  const onOutputResetAppliedRef = useRef(props.onOutputResetApplied);
  const sessionSizeRef = useRef(props.sessionSize);
  const confirmedSessionSizeRef = useRef(props.sessionSize);
  const mobileInputModeRef = useRef(Boolean(props.mobileInputMode));
  const mobileKeyboardOpenRef = useRef(Boolean(props.mobileKeyboardOpen));
  const mobileCursorVisibleRowsRef = useRef<number | undefined>(undefined);
  const resizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const stabilizeRef = useRef<((source?: ResizeSource) => void) | undefined>(undefined);
  const pendingResizeAfterSnapshotRef = useRef<ResizeSource | undefined>(undefined);
  const drainOutputRef = useRef<() => void>(() => undefined);
  const cursorFrameRef = useRef<number | undefined>(undefined);
  const cursorReportTimerRef = useRef<number | undefined>(undefined);
  const bottomScrollFrameRef = useRef<number | undefined>(undefined);
  const copyToastTimerRef = useRef<number | undefined>(undefined);
  const lastCursorReportAtRef = useRef(0);
  const terminalScrollFrameRef = useRef<number | undefined>(undefined);
  const terminalScrollTimerRef = useRef<number | undefined>(undefined);
  const lastTerminalScrollReportAtRef = useRef(0);
  const terminalRenderedOutputBytesSinceSnapshotRef = useRef(0);
  const terminalObservedLiveOutputSinceSnapshotRef = useRef(false);
  const terminalOutputIdleRef = useRef(true);
  const terminalOutputIdleSinceRef = useRef<number | undefined>(undefined);
  const terminalServerScrollbackResyncPendingRef = useRef(false);
  const terminalServerScrollbackResyncIdleTimerRef = useRef<number | undefined>(undefined);
  const terminalLastServerScrollbackResyncAtRef = useRef(0);
  const terminalRevealHistoryAfterSnapshotRef = useRef(false);
  const terminalRevealHistorySuppressBottomUntilRef = useRef(0);
  const pendingFocusRequestRef = useRef<number | undefined>(undefined);
  const mobileDirectionGestureRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    lastStepX: number;
    lastStepY: number;
    ready: boolean;
    active: boolean;
    timer: number;
    repeatTimer?: number;
    repeatDirection?: MobileDirection;
    repeatCount: number;
  } | undefined>(undefined);
  const mobileScrollGestureRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    lastClientY: number;
    active: boolean;
  } | undefined>(undefined);
  const mobileSelectionLongPressRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    timer?: number;
    active: boolean;
    moved: boolean;
    startCell?: { col: number; row: number };
    lastCell?: { col: number; row: number };
  } | undefined>(undefined);
  const mobilePointerDownInputFocusRef = useRef(false);
  const mobileKeyboardResizeSuppressUntilRef = useRef(0);
  const passiveFocusBypassTimerRef = useRef<number | undefined>(undefined);
  const lastNativePasteRef = useRef<{ text: string; atMs: number } | undefined>(undefined);
  const pendingPasteShortcutRef = useRef<{
    id: number;
    nativePasteObserved: boolean;
    fallbackStarted: boolean;
  } | undefined>(undefined);
  const pendingPasteShortcutTimerRef = useRef<number | undefined>(undefined);
  const pasteShortcutSequenceRef = useRef(0);
  const mobileCompositionActiveRef = useRef(false);
  const lastMobileCompositionEndAtRef = useRef(0);
  const currentFontSizeRef = useRef(TERMINAL_FONT_SIZE);
  const bottomScrollPassesRef = useRef(0);
  const bottomScrollGenerationRef = useRef(0);
  const bottomScrollProgrammaticRef = useRef(false);
  const terminalSelectionCopyRef = useRef<{ text: string; atMs: number } | undefined>(undefined);
  const terminalSelectionCopyGenerationRef = useRef(0);
  const terminalNativeSelectionCopySuppressUntilRef = useRef(0);
  const terminalSelectionClickFocusSuppressUntilRef = useRef(0);
  const terminalClipboardSelectionOwnerRef = useRef(false);
  const terminalNativeCopyCommandInFlightRef = useRef(false);
  const terminalNativeCopyCommandHandledRef = useRef(false);
  const terminalSelectionFocusPendingRef = useRef(false);
  const terminalSelectionDragRef = useRef<{
    active: boolean;
    dragging: boolean;
    startCol: number;
    startRow: number;
    lastCol: number;
    lastRow: number;
    startClientX: number;
    startClientY: number;
  } | undefined>(undefined);
  const terminalSnapshotRedrawGenerationRef = useRef(0);
  const terminalResizeStabilizationTimerRef = useRef<number | undefined>(undefined);
  const terminalResizeRequestKeyRef = useRef<string | undefined>(undefined);
  const terminalResizeReportFrameRef = useRef<number | undefined>(undefined);
  const terminalResizeReportPassesRef = useRef(0);
  const terminalResizeReportSizeRef = useRef<TerminalSize | undefined>(undefined);
  const terminalResizeReportSourceRef = useRef<ResizeSource | undefined>(undefined);
  const mobileViewportLayoutSuppressRef = useRef(false);
  const previousMobileViewportMetricsRef = useRef<
    | {
        keyboardOpen: boolean;
        width?: number;
        height?: number;
        offsetTop?: number;
      }
    | undefined
  >(undefined);
  const pendingSnapshotHistoryRepairRef = useRef<
    | {
        snapshotRows: number;
        snapshotCols: number;
        createdAtMs: number;
      }
    | undefined
  >(undefined);
  const lastRenderedSnapshotSizeRef = useRef<
    | {
        rows: number;
        cols: number;
      }
    | undefined
  >(undefined);
  const pendingSnapshotHistoryRepairFrameRef = useRef<number | undefined>(undefined);
  const pendingSnapshotHistoryRepairTimerRef = useRef<number | undefined>(undefined);
  const terminalStabilizeSourceRef = useRef<ResizeSource | undefined>(undefined);
  const terminalStabilizeFrameRef = useRef<number | undefined>(undefined);
  const terminalStabilizePassesRef = useRef(0);
  const terminalWheelLineRemainderRef = useRef(0);
  const terminalWheelRemainderModeRef = useRef<number | undefined>(undefined);
  const terminalSelectionWindowListenersRef = useRef<
    | {
        mousemove: (event: globalThis.MouseEvent) => void;
        mouseup: (event: globalThis.MouseEvent) => void;
        blur: () => void;
      }
    | undefined
  >(undefined);
  const forcedCursorBottomModeRef = useRef(false);
  const {
    focused,
    setFocused,
    focusedRef,
    clientSizeRef,
    focusActivationArmedRef,
    passiveFocusBypassRef,
    passiveInputFocusRef,
    suppressPassiveFocusRef,
    windowActiveRef,
    focusOutTimerRef,
    clearPendingFocusOut,
    armMobileInputFocusRescue,
    cancelMobileInputFocusRecovery,
    mobileInputFocusRescueActive,
    installTerminalFocusResizeListeners,
  } = useTerminalFocusResizeState();
  const {
    snapshotRedrawInProgressRef,
    resetWriterState,
    markWriterNeedsRefreshAndScroll,
    createTerminalOutputDrain,
  } = useTerminalOutputWriter(
    () => props.takeOutput(),
    () => onTerminalResyncRef.current?.(undefined),
  );
  const [copyToastVisible, setCopyToastVisible] = useState(false);
  const [terminalSelectionAvailable, setTerminalSelectionAvailable] = useState(false);
  const [mobileDirectionActive, setMobileDirectionActive] = useState(false);
  const [mobileDirection, setMobileDirection] = useState<MobileDirection | undefined>();
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchDraft, setSearchDraft] = useState("");
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | undefined>();
  const [searchResult, setSearchResult] = useState<SessionSearchResultPayload | undefined>();
  const [activeSearchIndex, setActiveSearchIndex] = useState(0);
  const deferredFrameIdRef = useRef(0);
  const deferredFrameHandlesByIdRef = useRef<Map<number, DeferredTerminalFrameHandle>>(new Map());
  const deferredFrameHandlesRef = useRef<Set<DeferredTerminalFrameHandle>>(new Set());
  const confirmOutputResetApplied = (version: number) => {
    const confirm = () => onOutputResetAppliedRef.current?.(version);
    // 中文注释：测试桩需要精确卡住“reset 已完成但 receive loop 还不能继续”的窗口，
    // 因此 reset ack 无论来自新实例挂载还是原地清屏，都统一走这一条延迟确认钩子。
    const deferOutputResetApplied = (globalThis as {
      __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (deferredConfirm: () => void) => void;
    }).__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__;
    if (deferOutputResetApplied) {
      deferOutputResetApplied(confirm);
      return;
    }
    confirm();
  };

  const readCurrentMobileViewportMetrics = (keyboardOpenFallback = mobileKeyboardOpenRef.current) => {
    if (typeof window === "undefined") {
      return undefined;
    }
    const viewport = window.visualViewport;
    if (!viewport) {
      return undefined;
    }
    const width = Math.round(viewport?.width ?? window.innerWidth);
    const height = Math.round(viewport?.height ?? window.innerHeight);
    const offsetTop = Math.round(viewport?.offsetTop ?? 0);
    const keyboardInset = Math.max(0, Math.round(window.innerHeight - height - offsetTop));
    return {
      keyboardOpen: keyboardInset >= 80 || keyboardOpenFallback,
      width,
      height,
      offsetTop,
    };
  };

  const rememberMobileViewportMetrics = (
    nextViewportMetrics: { keyboardOpen: boolean; width?: number; height?: number; offsetTop?: number },
    options: { suppressLayout: boolean },
  ) => {
    const previousViewportMetrics = previousMobileViewportMetricsRef.current;
    previousMobileViewportMetricsRef.current = nextViewportMetrics;
    if (!options.suppressLayout || !previousViewportMetrics) {
      return;
    }
    const widthChanged =
      previousViewportMetrics.width !== undefined &&
      nextViewportMetrics.width !== undefined &&
      previousViewportMetrics.width !== nextViewportMetrics.width;
    if (widthChanged) {
      // 中文注释：横竖屏切换或真实移动端 layout 宽度变化必须继续上报 PTY resize。
      // 这类变化也会伴随 visualViewport 事件，但不能被当成软键盘 height-only 变化吞掉。
      mobileViewportLayoutSuppressRef.current = false;
      return;
    }
    if (
      previousViewportMetrics.keyboardOpen !== nextViewportMetrics.keyboardOpen ||
      previousViewportMetrics.height !== nextViewportMetrics.height ||
      previousViewportMetrics.offsetTop !== nextViewportMetrics.offsetTop
    ) {
      // 中文注释：真实移动浏览器常把同一次软键盘/visualViewport 变化同时派发成
      // visualViewport resize、window resize 和 ResizeObserver。后一类事件会以
      // "layout" 进入 resize；这里只 suppress 宽度不变的 height/offset/keyboard
      // 变化，把它们并入纯前端 viewport 刷新。不能只用短时间窗，否则慢一拍的
      // ResizeObserver 或 snapshot 收尾仍会把软键盘变化写回 shared PTY。
      mobileViewportLayoutSuppressRef.current = true;
    }
  };

  const refreshMobileViewportLayoutSuppressFromWindow = () => {
    if (!mobileInputModeRef.current) {
      return;
    }
    const nextViewportMetrics = readCurrentMobileViewportMetrics();
    if (!nextViewportMetrics) {
      return;
    }
    rememberMobileViewportMetrics(nextViewportMetrics, { suppressLayout: true });
  };

  const armMobileKeyboardResizeSuppress = () => {
    mobileKeyboardResizeSuppressUntilRef.current = Date.now() + MOBILE_KEYBOARD_RESIZE_SUPPRESS_MS;
  };

  const clearMobileKeyboardResizeSuppress = () => {
    mobileKeyboardResizeSuppressUntilRef.current = 0;
  };

  const mobileKeyboardResizeSuppressActive = () => (
    Date.now() <= mobileKeyboardResizeSuppressUntilRef.current
  );

  const mouseEventCameFromTouch = (event: globalThis.MouseEvent | MouseEvent<HTMLDivElement>) => {
    type TouchSourceMouseEvent = globalThis.MouseEvent & {
      sourceCapabilities?: { firesTouchEvents?: boolean };
    };
    const sourceCapabilities = "nativeEvent" in event
      ? (event.nativeEvent as TouchSourceMouseEvent).sourceCapabilities
      : (event as TouchSourceMouseEvent).sourceCapabilities;
    return Boolean(sourceCapabilities?.firesTouchEvents);
  };

  const cancelDeferredTerminalFrame = (frameId: number | undefined) => {
    if (frameId === undefined) {
      return;
    }
    const handle = deferredFrameHandlesByIdRef.current.get(frameId);
    handle?.cancel();
  };

  const scheduleDeferredTerminalFrame = (callback: () => void): number => {
    const pageUnsafeAtSchedule = isDocumentAnimationFrameUnsafe();
    deferredFrameIdRef.current += 1;
    const frameId = deferredFrameIdRef.current;
    let frame: number | undefined;
    let timer: number | undefined;
    let settled = false;
    const clearScheduled = () => {
      if (frame !== undefined) {
        window.cancelAnimationFrame(frame);
        frame = undefined;
      }
      if (timer !== undefined) {
        window.clearTimeout(timer);
        timer = undefined;
      }
    };
    const finish = () => {
      deferredFrameHandlesRef.current.delete(handle);
      deferredFrameHandlesByIdRef.current.delete(frameId);
    };
    const run = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearScheduled();
      finish();
      callback();
    };
    const armTimer = () => {
      if (settled || timer !== undefined) {
        return;
      }
      timer = window.setTimeout(run, 0);
    };
    const handle: DeferredTerminalFrameHandle = {
      id: frameId,
      cancel: () => {
        if (settled) {
          return;
        }
        settled = true;
        clearScheduled();
        finish();
      },
      rescueHidden: (force = false) => {
        if (settled || frame === undefined) {
          return;
        }
        if (!force && !isDocumentAnimationFrameUnsafe()) {
          return;
        }
        window.cancelAnimationFrame(frame);
        frame = undefined;
        armTimer();
      },
    };
    deferredFrameHandlesRef.current.add(handle);
    deferredFrameHandlesByIdRef.current.set(frameId, handle);
    if (pageUnsafeAtSchedule) {
      armTimer();
      return frameId;
    }
    frame = window.requestAnimationFrame(run);
    return frameId;
  };

  useEffect(() => {
    if (typeof document === "undefined") {
      return undefined;
    }
    const rescueDeferredFrames = () => {
      deferredFrameHandlesRef.current.forEach((handle) => handle.rescueHidden(true));
    };
    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        rescueDeferredFrames();
      }
    };
    document.addEventListener("visibilitychange", handleVisibilityChange);
    window.addEventListener("blur", rescueDeferredFrames);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      window.removeEventListener("blur", rescueDeferredFrames);
      deferredFrameHandlesRef.current.forEach((handle) => handle.cancel());
      deferredFrameHandlesRef.current.clear();
      deferredFrameHandlesByIdRef.current.clear();
    };
  }, []);

  const clearTerminalServerScrollbackResyncIdleTimer = () => {
    if (terminalServerScrollbackResyncIdleTimerRef.current !== undefined) {
      window.clearTimeout(terminalServerScrollbackResyncIdleTimerRef.current);
      terminalServerScrollbackResyncIdleTimerRef.current = undefined;
    }
  };

  const clearPendingSnapshotHistoryRepairSchedule = () => {
    if (pendingSnapshotHistoryRepairFrameRef.current !== undefined) {
      cancelDeferredTerminalFrame(pendingSnapshotHistoryRepairFrameRef.current);
      pendingSnapshotHistoryRepairFrameRef.current = undefined;
    }
    if (pendingSnapshotHistoryRepairTimerRef.current !== undefined) {
      window.clearTimeout(pendingSnapshotHistoryRepairTimerRef.current);
      pendingSnapshotHistoryRepairTimerRef.current = undefined;
    }
  };

  const clearPendingSnapshotHistoryRepair = () => {
    pendingSnapshotHistoryRepairRef.current = undefined;
    clearPendingSnapshotHistoryRepairSchedule();
  };

  const measurePreferredClientSize = (): TerminalSize | undefined => {
    const preferred = clientSizeRef.current;
    if (
      preferred &&
      preferred.rows >= MIN_FOCUSED_RESIZE_ROWS &&
      preferred.cols >= MIN_FOCUSED_RESIZE_COLS
    ) {
      return preferred;
    }
    const fit = fitRef.current;
    const host = hostRef.current;
    const proposed = fit?.proposeStableDimensions?.() ?? fit?.proposeDimensions?.();
    if (
      !host ||
      !proposed ||
      proposed.rows < MIN_FOCUSED_RESIZE_ROWS ||
      proposed.cols < MIN_FOCUSED_RESIZE_COLS
    ) {
      return undefined;
    }
    return {
      rows: proposed.rows,
      cols: proposed.cols,
      pixel_width: host.clientWidth,
      pixel_height: host.clientHeight,
    };
  };

  const evaluatePendingSnapshotHistoryRepair = () => {
    const pendingRepair = pendingSnapshotHistoryRepairRef.current;
    const sessionSize = confirmedSessionSizeRef.current;
    if (!pendingRepair || !sessionSize) {
      clearPendingSnapshotHistoryRepairSchedule();
      return;
    }
    const preferredSize = measurePreferredClientSize() ?? (() => {
      const terminal = terminalRef.current;
      const host = hostRef.current;
      if (!terminal || !host || !localTerminalOwnsResizeAuthority()) {
        return undefined;
      }
      return {
        rows: terminal.rows,
        cols: terminal.cols,
        pixel_width: host.clientWidth,
        pixel_height: host.clientHeight,
      };
    })();
    if (
      !preferredSize ||
      sessionSize.rows !== preferredSize.rows ||
      sessionSize.cols !== preferredSize.cols
    ) {
      clearPendingSnapshotHistoryRepairSchedule();
      return;
    }
    if (
      sessionSize.rows === pendingRepair.snapshotRows &&
      sessionSize.cols === pendingRepair.snapshotCols
    ) {
      clearPendingSnapshotHistoryRepairSchedule();
      return;
    }
    const repairAgeMs = nowForThrottle() - pendingRepair.createdAtMs;
    if (terminalObservedLiveOutputSinceSnapshotRef.current) {
      recordTermdDiagnostic("terminal_snapshot_history_repair_abandoned", {
        snapshotRows: pendingRepair.snapshotRows,
        snapshotCols: pendingRepair.snapshotCols,
        sessionRows: sessionSize.rows,
        sessionCols: sessionSize.cols,
        repairAgeMs,
        renderedBytesSinceSnapshot: terminalRenderedOutputBytesSinceSnapshotRef.current,
        liveOutputSinceSnapshot: terminalObservedLiveOutputSinceSnapshotRef.current,
      });
      // 中文注释：一旦旧尺寸 snapshot 之后已经开始渲染 live output，就不再自动发起
      // full snapshot repair。此时再重连重放会打断正在恢复的 relay/stdout 流，
      // 体感比“旧历史暂时未按新列宽重排”更差，尤其是公网冻结恢复场景。
      clearPendingSnapshotHistoryRepair();
      return;
    }
    const scheduleRepairFrame = () => {
      if (pendingSnapshotHistoryRepairFrameRef.current !== undefined) {
        return;
      }
      pendingSnapshotHistoryRepairFrameRef.current = scheduleDeferredTerminalFrame(() => {
        pendingSnapshotHistoryRepairFrameRef.current = undefined;
        if (!pendingSnapshotHistoryRepairRef.current) {
          return;
        }
        recordTermdDiagnostic("terminal_snapshot_history_repair_resync", {
          snapshotRows: pendingSnapshotHistoryRepairRef.current.snapshotRows,
          snapshotCols: pendingSnapshotHistoryRepairRef.current.snapshotCols,
          sessionRows: sessionSize.rows,
          sessionCols: sessionSize.cols,
          renderedBytesSinceSnapshot: terminalRenderedOutputBytesSinceSnapshotRef.current,
          liveOutputSinceSnapshot: terminalObservedLiveOutputSinceSnapshotRef.current,
        });
        pendingSnapshotHistoryRepairRef.current = undefined;
        onTerminalResyncRef.current?.(undefined);
      });
    };
    if (!terminalOutputIdleRef.current) {
      clearPendingSnapshotHistoryRepairSchedule();
      return;
    }
    const now = nowForThrottle();
    const idleSince = terminalOutputIdleSinceRef.current;
    const idleForMs = idleSince === undefined ? 0 : now - idleSince;
    if (idleSince === undefined || idleForMs < TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS) {
      if (pendingSnapshotHistoryRepairTimerRef.current === undefined) {
        // 中文注释：repair timer 只在“已经 idle，但 settle window 还没满”时挂起。
        // 一旦中途又来了 live output，noteTerminalOutputRendered 会取消这里的调度，
        // 等下一次真正 idle 后再重新计时，避免在 burst 中途打断 attach。
        pendingSnapshotHistoryRepairTimerRef.current = window.setTimeout(() => {
          pendingSnapshotHistoryRepairTimerRef.current = undefined;
          evaluatePendingSnapshotHistoryRepair();
        }, Math.max(0, TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS - idleForMs));
      }
      return;
    }
    if (pendingSnapshotHistoryRepairTimerRef.current !== undefined) {
      window.clearTimeout(pendingSnapshotHistoryRepairTimerRef.current);
      pendingSnapshotHistoryRepairTimerRef.current = undefined;
    }
    scheduleRepairFrame();
  };

  useEffect(() => {
    // 搜索结果绑定当前 terminal buffer；attach/reset 后旧请求即使返回也不能落到新 buffer。
    searchRequestSeqRef.current += 1;
    setSearchLoading(false);
    setSearchError(undefined);
    setSearchResult(undefined);
    searchAddonRef.current?.clearDecorations();
    terminalRenderedOutputBytesSinceSnapshotRef.current = 0;
    terminalObservedLiveOutputSinceSnapshotRef.current = false;
    terminalOutputIdleRef.current = true;
    terminalOutputIdleSinceRef.current = undefined;
    terminalServerScrollbackResyncPendingRef.current = false;
    clearTerminalServerScrollbackResyncIdleTimer();
    terminalResizeRequestKeyRef.current = undefined;
    if (terminalResizeReportFrameRef.current !== undefined) {
      cancelDeferredTerminalFrame(terminalResizeReportFrameRef.current);
      terminalResizeReportFrameRef.current = undefined;
    }
    terminalResizeReportPassesRef.current = 0;
    terminalResizeReportSizeRef.current = undefined;
    terminalResizeReportSourceRef.current = undefined;
    terminalSelectionCopyGenerationRef.current += 1;
    // 中文注释：用户上滚触发的 reveal-history 只属于当前本地终端 buffer。
    // attach/reset 会换掉 buffer，跨代保留这个本地标记会把下一次普通 snapshot 错当成历史查看。
    terminalRevealHistoryAfterSnapshotRef.current = false;
    terminalRevealHistorySuppressBottomUntilRef.current = 0;
    // 中文注释：scrollback resync 冷却只应该活在当前 session/重置周期里。
    // 如果沿用旧 session 的时间戳，新 session 首次上滚会被错误地挡在 cooldown 外，
    // 结果看起来像“滚上去不是历史内容”。
    terminalLastServerScrollbackResyncAtRef.current = 0;
    lastRenderedSnapshotSizeRef.current = undefined;
    clearPendingSnapshotHistoryRepair();
  }, [props.attached, props.outputResetVersion]);

  const isTerminalPinnedToBottom = (terminal = terminalRef.current) => {
    const scrollState = terminal ? rendererRef.current?.scrollState(terminal) : undefined;
    const rendererPinned =
      !scrollState ||
      scrollState.viewportY >= Math.max(0, scrollState.baseY - TERMINAL_BOTTOM_EPSILON) ||
      (forcedCursorBottomModeRef.current &&
        Math.abs(scrollState.viewportY - scrollState.cursorBottomLine) <= TERMINAL_BOTTOM_EPSILON);
    const scrollport = scrollportRef.current;
    const scrollportPinned =
      !scrollport ||
      scrollport.scrollHeight <= scrollport.clientHeight + TERMINAL_BOTTOM_EPSILON ||
      scrollport.scrollTop >= scrollport.scrollHeight - scrollport.clientHeight - TERMINAL_BOTTOM_EPSILON;
    return rendererPinned && scrollportPinned;
  };
  const syncTerminalInputAnchor = (terminal = terminalRef.current, reason: "scroll" | "refresh" = "refresh") => {
    if (!terminal) {
      return;
    }
    rendererRef.current?.syncInputAnchor({
      host: hostRef.current,
      reason,
      forcedCursorBottom: forcedCursorBottomModeRef.current,
      bottomEpsilon: TERMINAL_BOTTOM_EPSILON,
      recordDiagnostic: (fields) => recordTermdDiagnostic("terminal_pane_sync_input_anchor", fields),
    });
  };
  const beginBottomScrollFollow = () => {
    if (bottomScrollGenerationRef.current === 0) {
      bottomScrollGenerationRef.current = 1;
    }
    return bottomScrollGenerationRef.current;
  };
  const invalidateBottomScrollFollow = () => {
    bottomScrollGenerationRef.current += 1;
    bottomScrollPassesRef.current = 0;
    if (bottomScrollFrameRef.current !== undefined) {
      cancelDeferredTerminalFrame(bottomScrollFrameRef.current);
      bottomScrollFrameRef.current = undefined;
    }
  };
  const isBottomScrollFollowActive = (generation: number) => bottomScrollGenerationRef.current === generation;
  const scrollToBottom = (generation: number) => {
    if (!isBottomScrollFollowActive(generation)) {
      return;
    }
    bottomScrollProgrammaticRef.current = true;
    try {
      const terminal = terminalRef.current;
      const scrollState = terminal ? rendererRef.current?.scrollState(terminal) : undefined;
      if (terminal && scrollState) {
        forcedCursorBottomModeRef.current = scrollState.cursorBottomLine < scrollState.baseY - TERMINAL_BOTTOM_EPSILON;
        terminal.scrollToLine(scrollState.cursorBottomLine);
        syncTerminalInputAnchor(terminal, "scroll");
      }
      const scrollport = scrollportRef.current;
      if (!scrollport) {
        return;
      }
      scrollport.scrollTop = Math.max(0, scrollport.scrollHeight - scrollport.clientHeight);
    } finally {
      bottomScrollProgrammaticRef.current = false;
    }
  };
  const resetMobileCursorViewportWindow = () => {
    mobileCursorVisibleRowsRef.current = undefined;
    const scrollport = scrollportRef.current;
    if (scrollport) {
      scrollport.scrollTop = 0;
    }
    canvasRef.current?.style.removeProperty("height");
    canvasRef.current?.style.removeProperty("min-height");
    frameRef.current?.style.removeProperty("height");
    frameRef.current?.style.removeProperty("min-height");
  };
  const visibleRowsForMobileViewport = (terminal: TerminalRendererTerminal) => {
    const proposedRows = fitRef.current?.proposeDimensions?.()?.rows;
    if (
      proposedRows !== undefined &&
      Number.isFinite(proposedRows) &&
      proposedRows > 0 &&
      proposedRows < terminal.rows
    ) {
      // 中文注释：移动端软键盘打开时，我们故意不 resize xterm/session rows，
      // 但 fit proposal 仍能代表键盘上方真正可见的终端行数。本地滚动窗口应使用
      // 这个可视高度，而不是仍用完整 PTY 网格高度。
      const visibleRows = clampNumber(Math.floor(proposedRows), 1, Math.max(1, terminal.rows));
      mobileCursorVisibleRowsRef.current = visibleRows;
      return visibleRows;
    }
    if (mobileCursorVisibleRowsRef.current) {
      return mobileCursorVisibleRowsRef.current;
    }
    const visibleRows =
      proposedRows !== undefined && Number.isFinite(proposedRows) && proposedRows > 0
        ? clampNumber(Math.floor(proposedRows), 1, Math.max(1, terminal.rows))
        : Math.max(1, terminal.rows);
    mobileCursorVisibleRowsRef.current = visibleRows;
    return visibleRows;
  };
  const syncMobileBottomViewportWindow = (terminal: TerminalRendererTerminal, visibleRows: number) => {
    const scrollport = scrollportRef.current;
    const canvas = canvasRef.current;
    const frame = frameRef.current;
    const host = hostRef.current;
    if (!scrollport || !canvas || !frame || !host || visibleRows <= 0 || terminal.rows <= 0) {
      return;
    }
    const hostStyle = window.getComputedStyle(host);
    const insetTop = Number.parseFloat(hostStyle.top || "0") || 0;
    const insetBottom = Number.parseFloat(hostStyle.bottom || "0") || 0;
    const visibleWindowHeight = Math.max(
      0,
      scrollport.clientHeight - insetTop - insetBottom,
    );
    const rowHeight = visibleWindowHeight > 0 ? visibleWindowHeight / visibleRows : 0;
    if (!Number.isFinite(rowHeight) || rowHeight <= 0) {
      return;
    }
    const fullGridHeight = Math.ceil(rowHeight * terminal.rows + insetTop + insetBottom);
    frame.style.height = `${fullGridHeight}px`;
    frame.style.minHeight = `${fullGridHeight}px`;

    canvas.style.height = `${fullGridHeight}px`;
    canvas.style.minHeight = `${fullGridHeight}px`;

    // 中文注释：快捷键栏贴在可视 viewport 底部；这里始终把完整 PTY 网格的底边贴到
    // 快捷键栏上方，不再追加半屏 spacer 把输入点抬到键盘上方中线。
    const maxScrollTop = Math.max(
      (Math.max(scrollport.scrollHeight, fullGridHeight) || fullGridHeight) - scrollport.clientHeight,
      0,
    );
    scrollport.scrollTop = maxScrollTop;
  };
  const alignMobileViewportToTerminalBottom = (terminal = terminalRef.current) => {
    if (!terminal || !mobileInputModeRef.current || !mobileKeyboardOpenRef.current) {
      resetMobileCursorViewportWindow();
      return false;
    }
    const scrollState = rendererRef.current?.scrollState(terminal);
    if (!scrollState || terminal.rows <= 0) {
      return false;
    }
    // 中文注释：键盘打开后目标是“终端底部贴住快捷键上方”，不是追随隐藏
    // textarea/输入光标。内部 xterm viewport 保持在 buffer 底部，外层 scrollport
    // 再裁出最后一屏，避免输入焦点被推到键盘上方中线。
    const visibleRows = visibleRowsForMobileViewport(terminal);
    const targetViewportY = scrollState.baseY;
    if (Math.abs(scrollState.viewportY - targetViewportY) < TERMINAL_BOTTOM_EPSILON) {
      syncMobileBottomViewportWindow(terminal, visibleRows);
      return false;
    }
    terminal.scrollToLine(targetViewportY);
    syncTerminalInputAnchor(terminal, "scroll");
    syncMobileBottomViewportWindow(terminal, visibleRows);
    return true;
  };
  const scheduleScrollToBottom = (generation: number, passes = 2) => {
    if (!isBottomScrollFollowActive(generation)) {
      return;
    }
    bottomScrollPassesRef.current = Math.max(bottomScrollPassesRef.current, Math.max(1, passes));
    if (bottomScrollFrameRef.current !== undefined) {
      cancelDeferredTerminalFrame(bottomScrollFrameRef.current);
      bottomScrollFrameRef.current = undefined;
    }
    const runScrollPass = () => {
      bottomScrollFrameRef.current = undefined;
      if (!isBottomScrollFollowActive(generation)) {
        bottomScrollPassesRef.current = 0;
        return;
      }
      scrollToBottom(generation);
      bottomScrollPassesRef.current -= 1;
      if (bottomScrollPassesRef.current <= 0) {
        bottomScrollPassesRef.current = 0;
        return;
      }
      // resize / renderer / 移动端 visual viewport 可能分多帧稳定。
      // attach 后的贴底只在首屏执行，多补几帧不会放大持续输出路径压力。
      bottomScrollFrameRef.current = scheduleDeferredTerminalFrame(runScrollPass);
    };
    bottomScrollFrameRef.current = scheduleDeferredTerminalFrame(runScrollPass);
  };
  const scheduleScrollToBottomIfPinned = (wasPinnedToBottom = isTerminalPinnedToBottom(), passes = 2) => {
    if (terminalRevealHistorySuppressBottomUntilRef.current > nowForThrottle()) {
      return;
    }
    if (mobileInputModeRef.current && mobileKeyboardOpenRef.current) {
      alignMobileViewportToTerminalBottom();
      return;
    }
    if (wasPinnedToBottom) {
      const generation = beginBottomScrollFollow();
      scheduleScrollToBottom(generation, passes);
    }
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
  const runExecCommandCopy = (
    text: string,
    options: {
      parent?: HTMLElement | null;
      trackTerminalCopyEvent?: boolean;
    } = {},
  ): { copied: boolean; handledByCopyEvent: boolean } => {
    if (!text) {
      return { copied: false, handledByCopyEvent: false };
    }
    const previousActiveElement = document.activeElement;
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.readOnly = true;
    textarea.setAttribute("aria-hidden", "true");
    textarea.tabIndex = -1;
    textarea.style.position = "fixed";
    textarea.style.left = "-9999px";
    textarea.style.top = "0";
    textarea.style.width = "1px";
    textarea.style.height = "1px";
    textarea.style.opacity = "0";
    textarea.style.pointerEvents = "none";
    const parent = options.parent ?? document.body;
    parent?.append(textarea);
    let copied = false;
    let handledByCopyEvent = false;
    const shouldTrackTerminalCopyEvent = Boolean(options.trackTerminalCopyEvent);
    if (shouldTrackTerminalCopyEvent) {
      terminalNativeCopyCommandHandledRef.current = false;
      terminalNativeCopyCommandInFlightRef.current = true;
    }
    try {
      textarea.focus();
      textarea.select();
      textarea.setSelectionRange(0, text.length);
      copied = typeof document.execCommand === "function" ? document.execCommand("copy") : false;
      handledByCopyEvent = shouldTrackTerminalCopyEvent && terminalNativeCopyCommandHandledRef.current;
    } catch {
      handledByCopyEvent = shouldTrackTerminalCopyEvent && terminalNativeCopyCommandHandledRef.current;
    } finally {
      if (shouldTrackTerminalCopyEvent) {
        terminalNativeCopyCommandInFlightRef.current = false;
        terminalNativeCopyCommandHandledRef.current = false;
      }
      textarea.remove();
      if (previousActiveElement instanceof HTMLElement) {
        previousActiveElement.focus();
      }
    }
    return { copied: copied || handledByCopyEvent, handledByCopyEvent };
  };
  const copyTextToClipboard = async (text: string): Promise<boolean> => {
    if (!text) {
      return false;
    }
    try {
      await navigator.clipboard?.writeText(text);
      return true;
    } catch {
      // 中文注释：浏览器 / Chromium 的剪贴板权限和 user activation 时序并不总是稳定；
      // async clipboard 失败时，退回到隐藏 textarea + execCommand，至少保证鼠标选区能复制出去。
    }
    return runExecCommandCopy(text).copied;
  };
  const updateTerminalSelectionDebug = (fields: Record<string, string | undefined>) => {
    const host = hostRef.current;
    if (!host || host.dataset.termdBuffer === undefined) {
      return;
    }
    for (const [key, value] of Object.entries(fields)) {
      const datasetKey = `termd${key[0].toUpperCase()}${key.slice(1)}`;
      if (value === undefined) {
        delete host.dataset[datasetKey as keyof DOMStringMap];
        continue;
      }
      host.dataset[datasetKey as keyof DOMStringMap] = value;
    }
  };
  const clearTerminalSelectionDragDebug = () => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    for (const key of [
      "termdSelectionNativeMouseDownTarget",
      "termdSelectionNativeMouseDownStarted",
      "termdSelectionNativeMouseDownTerminal",
      "termdSelectionNativeMouseDownRect",
      "termdSelectionNativeMouseDownSnapshotScrollback",
      "termdSelectionNativeMouseDownSnapshotLineCount",
      "termdSelectionNativeMouseDownScrollbarGutter",
      "termdSelectionNativeMouseMoveSeen",
      "termdSelectionNativeMouseUpSeen",
      "termdSelectionNativeMouseUpDragging",
      "termdSelectionDragActive",
      "termdSelectionDragDragging",
      "termdSelectionDragStart",
      "termdSelectionDragLast",
    ] as const) {
      delete host.dataset[key];
    }
  };
  const clearTerminalSelectionDebug = () => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    for (const key of [
      "termdSelectionCopy",
      "termdSelectionDragActive",
      "termdSelectionDragDragging",
      "termdSelectionDragStart",
      "termdSelectionDragLast",
      "termdSelectionNativeMouseDownTarget",
      "termdSelectionNativeMouseDownStarted",
      "termdSelectionNativeMouseDownTerminal",
      "termdSelectionNativeMouseDownRect",
      "termdSelectionNativeMouseDownSnapshotScrollback",
      "termdSelectionNativeMouseDownSnapshotLineCount",
      "termdSelectionNativeMouseDownScrollbarGutter",
      "termdSelectionNativeMouseMoveSeen",
      "termdSelectionNativeMouseUpSeen",
      "termdSelectionNativeMouseUpDragging",
      "termdSelectionPosition",
    ] as const) {
      delete host.dataset[key];
    }
  };
  const terminalCellFromClientPoint = (clientX: number, clientY: number): { col: number; row: number } | undefined => {
    const terminal = terminalRef.current;
    const surface = resolveTerminalSurfaceElement(hostRef.current);
    if (!terminal || !surface || terminal.cols <= 0 || terminal.rows <= 0) {
      return undefined;
    }
    const rect = surface.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) {
      return undefined;
    }
    const cellWidth = rect.width / terminal.cols;
    const cellHeight = rect.height / terminal.rows;
    if (cellWidth <= 0 || cellHeight <= 0) {
      return undefined;
    }
    return {
      col: clampNumber(Math.floor((clientX - rect.left) / cellWidth), 0, terminal.cols - 1),
      row: clampNumber(Math.floor((clientY - rect.top) / cellHeight), 0, terminal.rows - 1),
    };
  };
  const selectTerminalRange = (start: { col: number; row: number }, end: { col: number; row: number }) => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return undefined;
    }
    const selection = terminal.selectViewportRange(start, end);
    setTerminalSelectionAvailable(Boolean(selection));
    return selection;
  };
  const currentTerminalSelectionText = (terminal: TerminalRendererTerminal, selectionOverride?: string): string | undefined => {
    if (selectionOverride !== undefined) {
      return selectionOverride || undefined;
    }
    return terminal.getSelection() || undefined;
  };
  const shouldSkipDuplicateSelectionCopy = (selection: string, force = false) => {
    if (force) {
      return false;
    }
    const now = nowForThrottle();
    const lastSelectionCopy = terminalSelectionCopyRef.current;
    return lastSelectionCopy?.text === selection && now - lastSelectionCopy.atMs < 120;
  };
  const noteTerminalSelectionCopy = (selection: string) => {
    terminalSelectionCopyRef.current = { text: selection, atMs: nowForThrottle() };
    updateTerminalSelectionDebug({ selectionCopy: selection });
  };
  const markTerminalClipboardSelectionOwner = () => {
    terminalClipboardSelectionOwnerRef.current = true;
  };
  const clearTerminalClipboardSelectionOwner = () => {
    terminalClipboardSelectionOwnerRef.current = false;
  };
  const clearCurrentTerminalSelection = () => {
    const terminal = terminalRef.current;
    if (!terminal?.hasSelection()) {
      return false;
    }
    terminalSelectionCopyGenerationRef.current += 1;
    terminal.deselect();
    setTerminalSelectionAvailable(false);
    clearTerminalClipboardSelectionOwner();
    updateTerminalSelectionDebug({ selectionCopy: undefined });
    return true;
  };
  const copyVisibleTerminalSelection = () => {
    const terminal = terminalRef.current;
    if (!terminal) {
      setTerminalSelectionAvailable(false);
      return;
    }
    const selection = currentTerminalSelectionText(terminal);
    if (!selection) {
      setTerminalSelectionAvailable(false);
      return;
    }
    markTerminalClipboardSelectionOwner();
    copyCurrentTerminalSelection({ force: true, selectionOverride: selection });
  };
  const copyCurrentTerminalSelection = (
    options: {
      selectionOverride?: string;
      force?: boolean;
      clipboardData?: DataTransfer | null;
      allowProgrammaticClipboardFallback?: boolean;
    } = {},
  ) => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return false;
    }
    const selection = currentTerminalSelectionText(terminal, options.selectionOverride);
    if (!selection) {
      return false;
    }
    if (shouldSkipDuplicateSelectionCopy(selection, options.force)) {
      return false;
    }
    if (options.clipboardData) {
      noteTerminalSelectionCopy(selection);
      // 中文注释：浏览器菜单复制/快捷键 copy event 会提供 clipboardData；
      // 直接写入这份 payload，能绕过终端渲染层本身没有 DOM 文本选区的问题。
      options.clipboardData.setData("text/plain", selection);
      showCopyToast();
      return true;
    }
    if (options.allowProgrammaticClipboardFallback === false) {
      return false;
    }
    noteTerminalSelectionCopy(selection);
    void copyTextToClipboard(selection).then((copied) => {
      if (copied) {
        showCopyToast();
      }
    });
    return true;
  };
  const requestNativeTerminalCopy = (selection: string) => {
    // 中文注释：快捷键复制优先走浏览器原生 copy 事务，而不是先写 async clipboard。
    // 这样在 http/局域网地址这类 non-secure context 下仍能复用浏览器自己的复制链路。
    const nativeCopy = runExecCommandCopy(selection, {
      parent: hostRef.current ?? document.body,
      trackTerminalCopyEvent: true,
    });
    if (nativeCopy.handledByCopyEvent) {
      return true;
    }
    if (nativeCopy.copied) {
      noteTerminalSelectionCopy(selection);
      showCopyToast();
      return true;
    }
    return false;
  };
  const isTerminalCopyShortcut = (event: KeyboardEvent) => {
    const key = event.key.toLowerCase();
    if (event.altKey) {
      return false;
    }
    if (key === "c" && (event.metaKey || event.ctrlKey)) {
      return true;
    }
    return key === "insert" && event.ctrlKey && !event.metaKey;
  };
  const isTerminalPasteShortcut = (event: KeyboardEvent) => {
    const key = event.key.toLowerCase();
    if (event.altKey || event.ctrlKey || event.metaKey) {
      return false;
    }
    return key === "insert" && event.shiftKey;
  };
  const terminalShouldHandleClipboardEventTarget = (eventTarget: EventTarget | null) => {
    const host = hostRef.current;
    if (!host) {
      return false;
    }
    if (eventTarget instanceof Node && host.contains(eventTarget)) {
      return true;
    }
    // 中文注释：自定义拖拽选区结束后，浏览器焦点可能暂时落回 body/document。
    // 但一旦用户已经点到终端外，就不能再让旧终端选区劫持页面里的普通复制。
    return (
      terminalClipboardSelectionOwnerRef.current &&
      (eventTarget === document ||
        eventTarget === window ||
        eventTarget === document.body ||
        eventTarget === document.documentElement)
    );
  };
  const clearTerminalSelectionDrag = () => {
    const listeners = terminalSelectionWindowListenersRef.current;
    if (listeners) {
      window.removeEventListener("mousemove", listeners.mousemove, true);
      window.removeEventListener("mouseup", listeners.mouseup, true);
      window.removeEventListener("blur", listeners.blur, true);
      terminalSelectionWindowListenersRef.current = undefined;
    }
    terminalSelectionDragRef.current = undefined;
    terminalSelectionFocusPendingRef.current = false;
    clearTerminalSelectionDragDebug();
  };
  const handleTerminalSelectionMouseMove = (event: globalThis.MouseEvent) => {
    const drag = terminalSelectionDragRef.current;
    if (!drag?.active) {
      return;
    }
    updateTerminalSelectionDebug({
      selectionNativeMouseMoveSeen: "true",
    });
    event.preventDefault();
    event.stopPropagation();
    updateTerminalSelectionDrag(event.clientX, event.clientY);
  };
  const handleTerminalSelectionMouseUp = (event: globalThis.MouseEvent) => {
    const drag = terminalSelectionDragRef.current;
    if (!drag?.active) {
      clearTerminalSelectionDrag();
      return;
    }
    const wasDragging = drag.dragging;
    const shouldRestoreFocus = terminalSelectionFocusPendingRef.current;
    updateTerminalSelectionDebug({
      selectionNativeMouseUpSeen: "true",
      selectionNativeMouseUpDragging: String(drag.dragging),
    });
    event.preventDefault();
    event.stopPropagation();
    finishTerminalSelectionDrag(event.clientX, event.clientY);
    if (shouldRestoreFocus && !wasDragging) {
      terminalSelectionFocusPendingRef.current = false;
      window.requestAnimationFrame(() => {
        focusTerminalInputSink();
      });
    }
  };
  const handleTerminalSelectionWindowBlur = () => {
    terminalSelectionCopyGenerationRef.current += 1;
    clearTerminalSelectionDrag();
  };
  const installTerminalSelectionDragListeners = () => {
    if (terminalSelectionWindowListenersRef.current) {
      return;
    }
    const listeners = {
      mousemove: handleTerminalSelectionMouseMove,
      mouseup: handleTerminalSelectionMouseUp,
      blur: handleTerminalSelectionWindowBlur,
    };
    terminalSelectionWindowListenersRef.current = listeners;
    window.addEventListener("mousemove", listeners.mousemove, true);
    window.addEventListener("mouseup", listeners.mouseup, true);
    window.addEventListener("blur", listeners.blur, true);
  };
  const startTerminalSelectionDrag = (clientX: number, clientY: number) => {
    terminalSelectionCopyGenerationRef.current += 1;
    const cell = terminalCellFromClientPoint(clientX, clientY);
    if (!cell) {
      return false;
    }
    const terminal = terminalRef.current;
    const host = terminal?.element;
    const surfaceRect = resolveTerminalSurfaceElement(host)?.getBoundingClientRect();
    const snapshotBufferLineCount = (host?.dataset.termdBuffer ?? "").split("\n").length;
    updateTerminalSelectionDebug({
      selectionNativeMouseDownTerminal: terminal ? JSON.stringify({ cols: terminal.cols, rows: terminal.rows }) : undefined,
      selectionNativeMouseDownRect: surfaceRect ? JSON.stringify({ width: surfaceRect.width, height: surfaceRect.height }) : undefined,
      selectionNativeMouseDownSnapshotScrollback: String(Number.parseFloat(host?.dataset.termdScrollbackLength ?? "0") || 0),
      selectionNativeMouseDownSnapshotLineCount: String(snapshotBufferLineCount),
    });
    terminal?.deselect?.();
    terminalSelectionDragRef.current = {
      active: true,
      dragging: false,
      startCol: cell.col,
      startRow: cell.row,
      lastCol: cell.col,
      lastRow: cell.row,
      startClientX: clientX,
      startClientY: clientY,
    };
    updateTerminalSelectionDebug({
      selectionDragActive: "true",
      selectionDragDragging: "false",
      selectionDragStart: JSON.stringify(cell),
      selectionDragLast: JSON.stringify(cell),
      selectionCopy: undefined,
    });
    installTerminalSelectionDragListeners();
    return true;
  };
  const updateTerminalSelectionDrag = (clientX: number, clientY: number) => {
    const drag = terminalSelectionDragRef.current;
    if (!drag?.active) {
      return;
    }
    const cell = terminalCellFromClientPoint(clientX, clientY);
    if (!cell) {
      return;
    }
    const movement = Math.max(Math.abs(clientX - drag.startClientX), Math.abs(clientY - drag.startClientY));
    if (!drag.dragging) {
      if (movement < TERMINAL_SELECTION_DRAG_THRESHOLD_PX) {
        return;
      }
      drag.dragging = true;
    }
    if (cell.col === drag.lastCol && cell.row === drag.lastRow) {
      return;
    }
    drag.lastCol = cell.col;
    drag.lastRow = cell.row;
    updateTerminalSelectionDebug({
      selectionDragDragging: String(drag.dragging),
      selectionDragLast: JSON.stringify(cell),
    });
    selectTerminalRange(
      { col: drag.startCol, row: drag.startRow },
      { col: cell.col, row: cell.row },
    );
  };
  const finishTerminalSelectionDrag = (clientX: number, clientY: number) => {
    const drag = terminalSelectionDragRef.current;
    if (!drag?.active) {
      clearTerminalSelectionDrag();
      return;
    }
    const cell = terminalCellFromClientPoint(clientX, clientY);
    const endCell = cell ?? { col: drag.lastCol, row: drag.lastRow };
    const shouldCopy = drag.dragging;
    if (shouldCopy) {
      // 中文注释：拖拽选区结束后浏览器通常还会补一个 trailing click；
      // 这个 click 仍属于刚完成的拖拽手势，不能立刻把焦点抢回隐藏 textarea。
      terminalSelectionClickFocusSuppressUntilRef.current = nowForThrottle() + 250;
      terminalNativeSelectionCopySuppressUntilRef.current = nowForThrottle() + 750;
      const dragSelection = selectTerminalRange({ col: drag.startCol, row: drag.startRow }, endCell);
      updateTerminalSelectionDebug({
        selectionDragDragging: "true",
        selectionDragLast: JSON.stringify(endCell),
      });
      const copyGeneration = terminalSelectionCopyGenerationRef.current;
      const retryCopy = (attempt: number) => {
        if (terminalSelectionCopyGenerationRef.current !== copyGeneration) {
          return;
        }
        if (dragSelection !== undefined) {
          copyCurrentTerminalSelection({ selectionOverride: dragSelection });
          return;
        }
        const terminal = terminalRef.current;
        const currentSelection = terminal ? currentTerminalSelectionText(terminal) : undefined;
        if (currentSelection) {
          copyCurrentTerminalSelection({ selectionOverride: currentSelection });
          return;
        }
        if (attempt >= 3) {
          return;
        }
        window.requestAnimationFrame(() => retryCopy(attempt + 1));
      };
      window.requestAnimationFrame(() => {
        window.requestAnimationFrame(() => {
          retryCopy(0);
        });
      });
    }
    clearTerminalSelectionDrag();
  };
  const clearMobileSelectionLongPress = () => {
    const pending = mobileSelectionLongPressRef.current;
    if (!pending) {
      return;
    }
    if (pending.timer !== undefined) {
      window.clearTimeout(pending.timer);
    }
    if (pending.active) {
      // 中文注释：移动端长按选择结束后，浏览器通常会补发兼容 mouse/click。
      // 这些事件仍属于同一轮选择手势，不能被当成新的输入点击而弹出软键盘。
      terminalSelectionClickFocusSuppressUntilRef.current = nowForThrottle() + 900;
    }
    mobileSelectionLongPressRef.current = undefined;
  };
  const suppressMobileSelectionTrailingFocus = () => {
    terminalSelectionClickFocusSuppressUntilRef.current = Math.max(
      terminalSelectionClickFocusSuppressUntilRef.current,
      nowForThrottle() + 900,
    );
  };
  const startMobileSelectionRange = (pointerId: number, clientX: number, clientY: number) => {
    const pending = mobileSelectionLongPressRef.current;
    const terminal = terminalRef.current;
    const cell = terminalCellFromClientPoint(clientX, clientY);
    if (!pending || pending.pointerId !== pointerId || !terminal || !cell) {
      return false;
    }
    if (!terminal.getViewportRangeText(cell, cell)) {
      // 中文注释：只有按在实际终端字符上才进入选择；空白处继续留给旧的方向长按手势。
      return false;
    }
    if (pending.timer !== undefined) {
      window.clearTimeout(pending.timer);
      pending.timer = undefined;
    }
    terminalSelectionCopyGenerationRef.current += 1;
    terminal.deselect();
    markTerminalClipboardSelectionOwner();
    suppressMobileSelectionTrailingFocus();
    clearMobileDirectionGesture();
    cancelMobilePointerDownInputFocus();
    clearPassiveFocusBypassIfInactive();
    pending.active = true;
    pending.moved = false;
    pending.startCell = cell;
    pending.lastCell = cell;
    // 中文注释：长按只落下一个 cell 起点，让用户继续拖动扩展到几个字符或跨行；
    // 不再整行选择，否则移动端无法做精细复制。
    selectTerminalRange(cell, cell);
    updateTerminalSelectionDebug({
      selectionDragActive: "true",
      selectionDragDragging: "false",
      selectionDragStart: JSON.stringify(cell),
      selectionDragLast: JSON.stringify(cell),
    });
    return true;
  };
  const updateMobileSelectionRange = (pointerId: number, clientX: number, clientY: number) => {
    const pending = mobileSelectionLongPressRef.current;
    if (!pending?.active || pending.pointerId !== pointerId || !pending.startCell) {
      return false;
    }
    const cell = terminalCellFromClientPoint(clientX, clientY);
    if (!cell) {
      return true;
    }
    pending.moved =
      pending.moved ||
      cell.col !== pending.startCell.col ||
      cell.row !== pending.startCell.row;
    pending.lastCell = cell;
    selectTerminalRange(pending.startCell, cell);
    updateTerminalSelectionDebug({
      selectionDragDragging: String(pending.moved),
      selectionDragLast: JSON.stringify(cell),
    });
    return true;
  };
  const finishMobileSelectionRange = (pointerId: number, clientX: number, clientY: number) => {
    const pending = mobileSelectionLongPressRef.current;
    if (!pending || pending.pointerId !== pointerId) {
      return false;
    }
    if (!pending.active) {
      clearMobileSelectionLongPress();
      return false;
    }
    updateMobileSelectionRange(pointerId, clientX, clientY);
    suppressMobileSelectionTrailingFocus();
    const terminal = terminalRef.current;
    const selection = terminal ? currentTerminalSelectionText(terminal) : undefined;
    if (selection) {
      copyCurrentTerminalSelection({ selectionOverride: selection });
    }
    clearMobileDirectionGesture();
    cancelMobilePointerDownInputFocus();
    clearPassiveFocusBypassIfInactive();
    mobileSelectionLongPressRef.current = undefined;
    return true;
  };
  const scheduleMobileSelectionLongPress = (
    pointerId: number,
    clientX: number,
    clientY: number,
  ) => {
    clearMobileSelectionLongPress();
    const timer = window.setTimeout(() => {
      const pending = mobileSelectionLongPressRef.current;
      if (!pending || pending.pointerId !== pointerId) {
        return;
      }
      // 中文注释：终端是 canvas/xterm 自绘文本，移动浏览器原生长按无法稳定产生 DOM 文本选区。
      // 因此长按只进入 termd 自己的 cell range selection，后续拖动可以精确到字符并跨行。
      if (!startMobileSelectionRange(pointerId, clientX, clientY)) {
        mobileSelectionLongPressRef.current = undefined;
      }
    }, MOBILE_SELECTION_LONG_PRESS_MS);
    mobileSelectionLongPressRef.current = {
      pointerId,
      startX: clientX,
      startY: clientY,
      timer,
      active: false,
      moved: false,
    };
  };
  const noteTerminalOutputRendered = (item: TerminalOutputItem) => {
    if (item.kind === "sync") {
      return;
    }
    if (item.kind === "snapshot") {
      terminalRenderedOutputBytesSinceSnapshotRef.current = 0;
      terminalObservedLiveOutputSinceSnapshotRef.current = false;
      terminalOutputIdleRef.current = true;
      terminalOutputIdleSinceRef.current = undefined;
      terminalServerScrollbackResyncPendingRef.current = false;
      clearTerminalServerScrollbackResyncIdleTimer();
      clearPendingSnapshotHistoryRepairSchedule();
      terminalResizeRequestKeyRef.current = undefined;
      return;
    }
    if (item.kind === "data" || item.kind === "output") {
      terminalOutputIdleRef.current = false;
      terminalOutputIdleSinceRef.current = undefined;
      clearTerminalServerScrollbackResyncIdleTimer();
      clearPendingSnapshotHistoryRepairSchedule();
      // 中文注释：history repair 只关心“这个 snapshot 之后是否已经见过 live output”，
      // 不能再和 scrollback 预取共用同一个字节计数器；后者会在本地已有历史时被清零。
      if (item.bytes.byteLength > 0) {
        terminalObservedLiveOutputSinceSnapshotRef.current = true;
      }
      terminalRenderedOutputBytesSinceSnapshotRef.current += serverScrollableOutputBytes(item.bytes);
    }
  };
  const noteTerminalOutputIdle = () => {
    terminalOutputIdleRef.current = true;
    terminalOutputIdleSinceRef.current = nowForThrottle();
    evaluatePendingSnapshotHistoryRepair();
  };
  const maybeRequestServerScrollbackResync = (
    maxViewportY: number,
    reason: "auto" | "user-scroll" = "auto",
  ) => {
    if (maxViewportY > 0) {
      terminalRenderedOutputBytesSinceSnapshotRef.current = 0;
      terminalServerScrollbackResyncPendingRef.current = false;
      clearTerminalServerScrollbackResyncIdleTimer();
      return;
    }
    const renderedBytes = terminalRenderedOutputBytesSinceSnapshotRef.current;
    const minBytes =
      reason === "user-scroll"
        ? TERMINAL_SERVER_SCROLLBACK_RESYNC_USER_MIN_BYTES
        : TERMINAL_SERVER_SCROLLBACK_RESYNC_MIN_BYTES;
    if (reason === "user-scroll" && terminalServerScrollbackResyncPendingRef.current) {
      if (terminalRevealHistoryAfterSnapshotRef.current) {
        return;
      }
      // 中文注释：自动 scrollback 预取可能已经先启动 full snapshot 重连。
      // 用户随后滚轮向上时，不能因为 pending 就丢掉“我要看历史”的意图；
      // 这里把已在路上的 snapshot 升级成 reveal-history snapshot。
      terminalRevealHistoryAfterSnapshotRef.current = true;
      recordTermdDiagnostic("terminal_server_scrollback_resync_upgrade", {
        reason,
        renderedBytes,
      });
      onTerminalResyncRef.current?.(undefined, { revealHistory: true });
      return;
    }
    if (
      !attachedRef.current ||
      !terminalOutputIdleRef.current ||
      snapshotRedrawInProgressRef.current ||
      terminalServerScrollbackResyncPendingRef.current ||
      renderedBytes < minBytes ||
      !onTerminalResyncRef.current
    ) {
      return;
    }
    const now = nowForThrottle();
    if (reason === "auto") {
      const idleSince = terminalOutputIdleSinceRef.current;
      const idleForMs = idleSince === undefined ? 0 : now - idleSince;
      if (idleSince === undefined || idleForMs < TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS) {
        if (terminalServerScrollbackResyncIdleTimerRef.current === undefined) {
          const remainingMs = Math.max(
            0,
            TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS - idleForMs,
          );
          // 中文注释：自动 scrollback 预取只应该发生在输出真正稳定之后。
          // 刚 attach/reconnect/relay 恢复完成时，终端 renderer 很容易短暂处于
          // “baseY=0 但画面还在追平”的中间态；这里等待一个 idle settle window，
          // 避免把暂态误判成全屏程序重绘，从而过早触发 full snapshot 重连。
          terminalServerScrollbackResyncIdleTimerRef.current = window.setTimeout(() => {
            terminalServerScrollbackResyncIdleTimerRef.current = undefined;
            const retryScrollState = rendererRef.current?.scrollState(terminalRef.current ?? undefined);
            maybeRequestServerScrollbackResync(retryScrollState?.baseY ?? 0, "auto");
          }, remainingMs);
        }
        return;
      }
    }
    if (
      terminalLastServerScrollbackResyncAtRef.current > 0 &&
      now - terminalLastServerScrollbackResyncAtRef.current < TERMINAL_SERVER_SCROLLBACK_RESYNC_COOLDOWN_MS
    ) {
      return;
    }
    // 中文注释：supervisor attach 流是全屏 renderer，实时输出可能只重绘可见屏幕，
    // 不会让本地终端自身生成 scrollback。此时主动拉一次 daemon/supervisor snapshot，
    // 让浏览器获得可滚动历史；snapshot 完成后计数会被 noteTerminalOutputRendered 清零。
    terminalServerScrollbackResyncPendingRef.current = true;
    terminalLastServerScrollbackResyncAtRef.current = now;
    recordTermdDiagnostic("terminal_server_scrollback_resync", {
      reason,
      renderedBytes,
      minBytes,
    });
    if (reason === "user-scroll") {
      terminalRevealHistoryAfterSnapshotRef.current = true;
      onTerminalResyncRef.current(undefined, { revealHistory: true });
      return;
    }
    onTerminalResyncRef.current(undefined);
  };
  const handleTerminalWheel = (event: WheelEvent) => {
    const deltaLines = terminalDeltaLinesFromWheel(event);
    if (deltaLines === undefined) {
      return;
    }
    const consumed = applyTerminalScrollDelta(deltaLines);
    if (!consumed) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
  };
  const terminalDeltaLinesFromWheel = (event: WheelEvent): number | undefined => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return undefined;
    }
    const surfaceHeight = resolveTerminalSurfaceElement(hostRef.current)?.getBoundingClientRect().height ?? 0;
    const cellHeight = surfaceHeight > 0 && terminal.rows > 0 ? surfaceHeight / terminal.rows : 16;
    if (event.deltaMode === 0) {
      // 中文注释：高精度触控板经常连续发出小于 1 行的像素滚动；
      // 这里按 cellHeight 折算并跨事件累积，避免“轻滚完全不动”。
      const shouldResetRemainder =
        terminalWheelRemainderModeRef.current !== event.deltaMode ||
        Math.sign(terminalWheelLineRemainderRef.current) !== Math.sign(event.deltaY);
      if (shouldResetRemainder) {
        terminalWheelLineRemainderRef.current = 0;
      }
      const deltaLinesFloat =
        terminalWheelLineRemainderRef.current + (event.deltaY / Math.max(1, cellHeight));
      const deltaLines = Math.trunc(deltaLinesFloat);
      terminalWheelLineRemainderRef.current = deltaLinesFloat - deltaLines;
      terminalWheelRemainderModeRef.current = event.deltaMode;
      return deltaLines;
    }
    terminalWheelLineRemainderRef.current = 0;
    terminalWheelRemainderModeRef.current = event.deltaMode;
    return Math.trunc(event.deltaY * (event.deltaMode === 2 ? terminal.rows : 1));
  };
  const applyTerminalScrollDelta = (deltaLines: number) => {
    const terminal = terminalRef.current;
    const scrollState = rendererRef.current?.scrollState(terminal ?? undefined);
    if (!terminal || !scrollState || deltaLines === 0) {
      return false;
    }
    if (scrollState.baseY > 0) {
      const nextViewportY = clampNumber(
        scrollState.viewportY + deltaLines,
        0,
        scrollState.baseY,
      );
      if (Math.abs(nextViewportY - scrollState.viewportY) < TERMINAL_BOTTOM_EPSILON) {
        return false;
      }
      terminal.scrollToLine(nextViewportY);
      syncTerminalInputAnchor(terminal, "scroll");
      scheduleTerminalScrollPosition({ immediate: true });
      return true;
    }
    if (deltaLines < 0) {
      // 中文注释：supervisor attach 的实时输出可能只是全屏 repaint，本地终端 baseY 仍为 0；
      // 用户向上查看历史就是明确的“我要历史”信号，此时主动拉一次 daemon/supervisor snapshot。
      maybeRequestServerScrollbackResync(scrollState.baseY, "user-scroll");
      return true;
    }
    return false;
  };
  const handleMobileTerminalScrollPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!mobileInputModeRef.current || !props.attached || event.pointerType === "mouse") {
      return;
    }
    if (!hitTerminalSurfaceAtPoint(event.target, event.clientX, event.clientY)) {
      mobileScrollGestureRef.current = undefined;
      return;
    }
    if (passiveFocusBypassTimerRef.current !== undefined) {
      window.clearTimeout(passiveFocusBypassTimerRef.current);
      passiveFocusBypassTimerRef.current = undefined;
    }
    cancelMobileInputFocusRecovery();
    windowActiveRef.current = true;
    suppressPassiveFocusRef.current = true;
    // 中文注释：移动端触摸落下可能只是想滚动 scrollback，不能在 pointerdown 阶段
    // 提前聚焦 helper textarea。否则软键盘会先于滚动手势弹起，用户几乎无法上下拖动。
    // 真正允许键盘恢复的边界统一后移到明确的 tap/click 或显式 focusRequest 路径。
    passiveFocusBypassRef.current = false;
    mobilePointerDownInputFocusRef.current = false;
    mobileScrollGestureRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      lastClientY: event.clientY,
      active: false,
    };
    scheduleMobileSelectionLongPress(event.pointerId, event.clientX, event.clientY);
  };
  const handleMobileTerminalScrollPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const gesture = mobileScrollGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    if (updateMobileSelectionRange(event.pointerId, event.clientX, event.clientY)) {
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    const directionGesture = mobileDirectionGestureRef.current;
    if (
      directionGesture &&
      directionGesture.pointerId === event.pointerId &&
      (directionGesture.ready || directionGesture.active)
    ) {
      clearMobileSelectionLongPress();
      mobileScrollGestureRef.current = undefined;
      return;
    }
    const deltaX = event.clientX - gesture.startX;
    const deltaY = event.clientY - gesture.startY;
    if (Math.hypot(deltaX, deltaY) > MOBILE_DIRECTION_CANCEL_PX) {
      clearMobileSelectionLongPress();
    }
    if (!gesture.active) {
      if (Math.abs(deltaY) < MOBILE_SCROLL_DEAD_ZONE_PX || Math.abs(deltaY) <= Math.abs(deltaX)) {
        return;
      }
      // 中文注释：只有“明显的纵向拖动”才接管成 scroll；横向/微小手势继续留给原路径。
      gesture.active = true;
      // 中文注释：scroll 手势会在 capture 阶段消费后续 pointermove/pointerup，
      // bubble 阶段的方向手势清理不一定能跑到；这里必须主动收回 pointerdown 给
      // helper textarea 的临时 focus 许可，避免滚动后迟到 focus 被误放行。
      cancelMobilePointerDownInputFocus();
      clearMobileDirectionGesture();
      clearPassiveFocusBypassIfInactive();
    }
    const terminal = terminalRef.current;
    if (!terminal || terminal.rows <= 0) {
      return;
    }
    const surfaceHeight = resolveTerminalSurfaceElement(hostRef.current)?.getBoundingClientRect().height ?? 0;
    const cellHeight = surfaceHeight > 0 ? surfaceHeight / terminal.rows : 16;
    // 中文注释：移动端滚动遵循原生触摸直觉：内容跟随手指移动。
    // 因此手指向下拖应查看更旧历史（viewport 向上），手指向上拖应回到更新内容。
    const deltaPixels = gesture.lastClientY - event.clientY;
    const deltaLines = Math.trunc(deltaPixels / Math.max(1, cellHeight * MOBILE_SCROLL_STEP_DIVISOR));
    if (deltaLines === 0) {
      return;
    }
    gesture.lastClientY = event.clientY;
    const consumed = applyTerminalScrollDelta(deltaLines);
    if (!consumed) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
  };
  const handleMobileTerminalScrollPointerEnd = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.type === "pointercancel") {
      const pendingSelection = mobileSelectionLongPressRef.current;
      const cancelledActiveSelection =
        Boolean(pendingSelection?.active && pendingSelection.pointerId === event.pointerId);
      clearMobileSelectionLongPress();
      if (cancelledActiveSelection) {
        event.preventDefault();
        event.stopPropagation();
        mobileScrollGestureRef.current = undefined;
        return;
      }
    } else if (finishMobileSelectionRange(event.pointerId, event.clientX, event.clientY)) {
      event.preventDefault();
      event.stopPropagation();
      mobileScrollGestureRef.current = undefined;
      return;
    }
    const gesture = mobileScrollGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    clearMobileSelectionLongPress();
    const directionGesture = mobileDirectionGestureRef.current;
    if (
      gesture.active &&
      !(
        directionGesture &&
        directionGesture.pointerId === event.pointerId &&
        (directionGesture.ready || directionGesture.active)
      )
    ) {
      event.preventDefault();
      event.stopPropagation();
    }
    mobileScrollGestureRef.current = undefined;
  };
  useEffect(() => {
    attachedRef.current = props.attached;
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onTerminalResyncRef.current = props.onTerminalResync;
    onTerminalSeqRenderedRef.current = props.onTerminalSeqRendered;
    onTerminalSizeRenderedRef.current = props.onTerminalSizeRendered;
    onOutputResetAppliedRef.current = props.onOutputResetApplied;
    sessionSizeRef.current = props.sessionSize;
    confirmedSessionSizeRef.current = props.sessionSize;
    mobileInputModeRef.current = Boolean(props.mobileInputMode);
    mobileKeyboardOpenRef.current = Boolean(props.mobileKeyboardOpen);
  }, [props.attached, props.mobileInputMode, props.mobileKeyboardOpen, props.onInput, props.onOutputResetApplied, props.onResize, props.onTerminalResync, props.onTerminalSeqRendered, props.onTerminalSizeRendered, props.sessionSize, props.takeOutput]);

  useEffect(() => props.registerOutputDrain(() => drainOutputRef.current()), [props.registerOutputDrain]);

  useLayoutEffect(() => {
    resetMobileCursorViewportWindow();
    if (!props.mobileInputMode) {
      return;
    }
    // 移动端软键盘会改变 visual viewport；只看 keyboardOpen 布尔值不够，
    // 因为部分浏览器会让 innerHeight 跟着缩放，导致键盘开关前后布尔值都为 false。
    const wasPinnedToBottom = isTerminalPinnedToBottom();
    rememberMobileViewportMetrics(
      {
        keyboardOpen: Boolean(props.mobileKeyboardOpen),
        width: props.mobileViewportWidth,
        height: props.mobileViewportHeight,
        offsetTop: props.mobileViewportOffsetTop,
      },
      { suppressLayout: true },
    );
    stabilizeRef.current?.("mobile-viewport");
    scheduleScrollToBottomIfPinned(wasPinnedToBottom);
  }, [props.mobileInputMode, props.mobileKeyboardOpen, props.mobileViewportWidth, props.mobileViewportHeight, props.mobileViewportOffsetTop]);

  useEffect(() => {
    if (terminalSelectionDragRef.current?.active || terminalSelectionFocusPendingRef.current) {
      return;
    }
    resizeRef.current?.(focused ? "focus" : "layout");
  }, [focused]);

  useEffect(() => {
    sessionSizeRef.current = props.sessionSize;
    confirmedSessionSizeRef.current = props.sessionSize;
    terminalResizeRequestKeyRef.current = undefined;
    terminalResizeReportSizeRef.current = undefined;
    terminalResizeReportPassesRef.current = 0;
    resizeRef.current?.("session");
  }, [props.sessionSize?.cols, props.sessionSize?.rows]);

  useEffect(() => {
    // 中文注释：旧尺寸 snapshot 的历史修复需要同时满足两件事：
    // 1) daemon 已确认当前 rows/cols 就是浏览器想要的尺寸；
    // 2) live output 已经稳定到足够安全，可以重新 full snapshot。
    // 这里每次 size ack 变化都重评估一次；如果旧 snapshot 反而晚到，也会由
    // snapshot render / output idle 路径主动再次评估，不依赖下一次 props 变化。
    evaluatePendingSnapshotHistoryRepair();
  }, [props.sessionSize?.cols, props.sessionSize?.rows]);

  const requestCursorReportFrame = () => {
    if (cursorFrameRef.current !== undefined) {
      return;
    }
    cursorFrameRef.current = scheduleDeferredTerminalFrame(() => {
      cursorFrameRef.current = undefined;
      const terminal = terminalRef.current;
      if (!terminal) {
        return;
      }
      lastCursorReportAtRef.current = nowForThrottle();
      alignMobileViewportToTerminalBottom(terminal);
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

  const requestTerminalScrollFrame = () => {
    if (terminalScrollFrameRef.current !== undefined) {
      return;
    }
    terminalScrollFrameRef.current = scheduleDeferredTerminalFrame(() => {
      terminalScrollFrameRef.current = undefined;
      const scrollState = rendererRef.current?.scrollState(terminalRef.current ?? undefined);
      const maxViewportY = scrollState?.baseY ?? 0;
      maybeRequestServerScrollbackResync(maxViewportY);
      lastTerminalScrollReportAtRef.current = nowForThrottle();
    });
  };

  const scheduleTerminalScrollPosition = (options: { immediate?: boolean } = {}) => {
    if (options.immediate) {
      if (terminalScrollTimerRef.current !== undefined) {
        window.clearTimeout(terminalScrollTimerRef.current);
        terminalScrollTimerRef.current = undefined;
      }
      requestTerminalScrollFrame();
      return;
    }

    const elapsed = nowForThrottle() - lastTerminalScrollReportAtRef.current;
    if (elapsed >= TERMINAL_SCROLL_REPORT_INTERVAL_MS) {
      requestTerminalScrollFrame();
      return;
    }
    if (terminalScrollTimerRef.current !== undefined) {
      return;
    }
    terminalScrollTimerRef.current = window.setTimeout(() => {
      terminalScrollTimerRef.current = undefined;
      requestTerminalScrollFrame();
    }, TERMINAL_SCROLL_REPORT_INTERVAL_MS - elapsed);
  };

  const sendTerminalControl = (data: string) => {
    recordTermdDiagnostic("terminal_pane_send_terminal_control", {
      chunkLength: data.length,
      mobileInputMode: mobileInputModeRef.current,
      passiveInputFocus: passiveInputFocusRef.current,
      terminalInputHasDomFocus: terminalInputHasDomFocus(),
      focused: focusedRef.current,
    });
    onInputRef.current(data);
    queueCursorReport({ immediate: true });
    if (mobileInputModeRef.current) {
      // 中文注释：移动端 beforeinput / paste / 方向手势不一定运行在持有局部 terminal
      // 变量的闭包里，因此这里必须从 ref 取当前实例，避免输入热路径抛 ReferenceError。
      focusTerminalInputSink(terminalRef.current, {
        force: !mobileKeyboardOpenRef.current,
      });
    }
  };

  const runSearch = async (event?: FormEvent<HTMLFormElement>) => {
    event?.preventDefault();
    const requestId = searchRequestSeqRef.current + 1;
    searchRequestSeqRef.current = requestId;
    const query = searchDraft.trim();
    if (!query || !props.onSearch) {
      setSearchLoading(false);
      setSearchError(undefined);
      setSearchResult(undefined);
      searchAddonRef.current?.clearDecorations();
      return;
    }
    setSearchLoading(true);
    setSearchError(undefined);
    try {
      const result = await props.onSearch(query);
      if (searchRequestSeqRef.current !== requestId) {
        return;
      }
      setSearchResult(result);
      setActiveSearchIndex(0);
      scrollToSearchMatch(result, 0);
      highlightSearchMatches(query, "next");
    } catch {
      if (searchRequestSeqRef.current !== requestId) {
        return;
      }
      setSearchResult(undefined);
      searchAddonRef.current?.clearDecorations();
      setSearchError(t("terminal.searchFailed"));
    } finally {
      if (searchRequestSeqRef.current === requestId) {
        setSearchLoading(false);
      }
    }
  };

  const scrollToSearchMatch = (result: SessionSearchResultPayload | undefined, index: number) => {
    const terminal = terminalRef.current;
    const scrollState = terminal ? rendererRef.current?.scrollState(terminal) : undefined;
    const match = result?.matches[index];
    if (!terminal || !scrollState || !match || !result?.line_count) {
      return;
    }
    // daemon 返回的是本次 snapshot 内的行号；renderer buffer 尾部与 snapshot 尾部对齐。
    const firstSnapshotLine = Math.max(0, scrollState.length - result.line_count);
    terminal.scrollToLine(clampNumber(firstSnapshotLine + match.line_index, 0, Math.max(0, scrollState.length - 1)));
    focusTerminalInputSink(terminal);
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
    // daemon 搜索负责跨 snapshot 的结果数量和目标行；终端渲染层本身不暴露文本
    // decoration API，因此可见反馈由 React 层的搜索结果浮层承担。renderer search hook
    // 只保留为可选扩展点，当前 renderer adapter 是 no-op。
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
    if (mobileInputModeRef.current) {
      armMobileInputFocusRescue();
    }
    focusTerminalInputSink();
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

  const notePendingPasteShortcutNativePaste = () => {
    const pendingShortcut = pendingPasteShortcutRef.current;
    if (!pendingShortcut) {
      return;
    }
    pendingShortcut.nativePasteObserved = true;
    if (pendingPasteShortcutTimerRef.current !== undefined) {
      window.clearTimeout(pendingPasteShortcutTimerRef.current);
      pendingPasteShortcutTimerRef.current = undefined;
    }
    if (!pendingShortcut.fallbackStarted) {
      pendingPasteShortcutRef.current = undefined;
    }
  };

  const handlePasteShortcut = async (shortcutId?: number) => {
    if (shortcutId !== undefined) {
      const pendingShortcut = pendingPasteShortcutRef.current;
      if (
        !pendingShortcut ||
        pendingShortcut.id !== shortcutId ||
        pendingShortcut.nativePasteObserved
      ) {
        return;
      }
      pendingShortcut.fallbackStarted = true;
    }
    try {
      const text = await navigator.clipboard?.readText?.();
      if (
        shortcutId !== undefined &&
        (
          !pendingPasteShortcutRef.current ||
          pendingPasteShortcutRef.current.id !== shortcutId ||
          pendingPasteShortcutRef.current.nativePasteObserved
        )
      ) {
        return;
      }
      if (text) {
        sendNativePasteText(text);
      } else {
        focusTerminalInputSink();
      }
    } catch {
      // 剪贴板读取可能被浏览器权限或非安全上下文拒绝；失败时只保持终端焦点。
      focusTerminalInputSink(terminalRef.current, {
        force: mobileInputModeRef.current && !mobileKeyboardOpenRef.current,
      });
    } finally {
      if (shortcutId !== undefined && pendingPasteShortcutRef.current?.id === shortcutId) {
        pendingPasteShortcutRef.current = undefined;
      }
    }
  };

  const schedulePasteShortcutFallback = (shortcutId: number) => {
    if (pendingPasteShortcutTimerRef.current !== undefined) {
      window.clearTimeout(pendingPasteShortcutTimerRef.current);
    }
    pendingPasteShortcutTimerRef.current = window.setTimeout(() => {
      pendingPasteShortcutTimerRef.current = undefined;
      void handlePasteShortcut(shortcutId);
    }, 0);
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

  const clearPassiveFocusBypassTimer = () => {
    if (passiveFocusBypassTimerRef.current !== undefined) {
      window.clearTimeout(passiveFocusBypassTimerRef.current);
      passiveFocusBypassTimerRef.current = undefined;
    }
  };

  const clearPassiveFocusBypassIfInactive = () => {
    if (!terminalInputHasDomFocus() && !focusedRef.current) {
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
    }
  };

  const clearPassiveInputFocus = () => {
    clearPassiveFocusBypassTimer();
    if (!passiveInputFocusRef.current || focusedRef.current) {
      clearPassiveFocusBypassIfInactive();
      return;
    }
    // 中文注释：移动端软键盘刚打开或刚收起时，浏览器经常把 focusout / pointerup / resize
    // 事件打成一串。这里如果主动把当前 activeElement blur 掉，很多浏览器会直接收回
    // 系统键盘，造成“点开就自己关闭”的观感。收口时只清内部临时状态，不再反向 blur。
    passiveInputFocusRef.current = false;
    passiveFocusBypassRef.current = false;
  };

  const shouldPreserveMobileKeyboardFocus = () => (
    mobileInputModeRef.current &&
    (mobileKeyboardOpenRef.current || mobileInputFocusRescueActive())
  );

  const cancelMobilePointerDownInputFocus = () => {
    if (!mobilePointerDownInputFocusRef.current) {
      clearPassiveFocusBypassTimer();
      return;
    }
    clearPassiveInputFocus();
    mobilePointerDownInputFocusRef.current = false;
    if (mobileInputModeRef.current) {
      // 中文注释：取消/滚动手势后，helper textarea 可以继续保留 DOM focus 以稳定软键盘，
      // 但这只是被动输入 sink，不能被 terminalDomHasActiveFocus 误当成 PTY resize 权限。
      if (focusedRef.current) {
        reportTerminalFocus(false);
      }
      passiveInputFocusRef.current = true;
      passiveFocusBypassRef.current = false;
      suppressPassiveFocusRef.current = true;
    }
    if (shouldPreserveMobileKeyboardFocus()) {
      // 中文注释：软键盘弹出链路里可能收到 pointercancel / scroll 误判。
      // rescue 窗口内绝不主动 blur helper textarea，否则系统键盘会刚弹出就收回。
      return;
    }
    // 中文注释：移动端系统键盘由 helper textarea 的 DOM focus 维持。
    // pointercancel、滚动和 contextmenu 在真机上可能晚于键盘动画到达；这里只能降级
    // 内部 resize/input 权限，不能反向 blur 输入 sink，否则键盘会“弹出后立刻收起”。
  };

  const cancelMobilePointerDownGestureOnly = () => {
    if (!mobilePointerDownInputFocusRef.current) {
      clearPassiveFocusBypassTimer();
      return;
    }
    clearPassiveFocusBypassTimer();
    mobilePointerDownInputFocusRef.current = false;
    if (!mobileInputModeRef.current) {
      return;
    }
    // 中文注释：迟到的 contextmenu 是系统长按菜单/键盘链路的一部分，不代表用户要取消输入。
    // 这里只收掉 pointerdown 的临时标记，保留 helper textarea 的 beforeinput 通道。
    if (focusedRef.current) {
      reportTerminalFocus(false);
    }
    passiveFocusBypassRef.current = false;
    suppressPassiveFocusRef.current = true;
    passiveInputFocusRef.current = terminalInputHasDomFocus();
  };

  const handleMobileDirectionPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!mobileInputModeRef.current || !props.attached || event.pointerType === "mouse") {
      return;
    }
    clearMobileDirectionGesture();
    clearPassiveFocusBypassTimer();
    cancelMobileInputFocusRecovery();
    windowActiveRef.current = true;
    suppressPassiveFocusRef.current = true;
    // 中文注释：方向手势同样始于触摸，不能在长按/拖动开始阶段就允许 helper textarea
    // 提前拿到焦点。否则用户只是摸一下准备滚动/长按，软键盘也会被误触发。
    passiveFocusBypassRef.current = false;
    const pointerId = event.pointerId;
    const startX = event.clientX;
    const startY = event.clientY;
    const timer = window.setTimeout(() => {
      const gesture = mobileDirectionGestureRef.current;
      if (!gesture || gesture.pointerId !== pointerId) {
        return;
      }
      // 中文注释：静止长按要留给系统复制/粘贴菜单。这里只标记“已长按”，
      // 等用户继续拖动超过死区后才真正进入方向手势。
      gesture.ready = true;
    }, MOBILE_DIRECTION_HOLD_MS);
    mobileDirectionGestureRef.current = {
      pointerId,
      startX,
      startY,
      lastStepX: startX,
      lastStepY: startY,
      ready: false,
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
      const distance = Math.hypot(deltaX, deltaY);
      if (!gesture.ready) {
        if (distance > MOBILE_DIRECTION_CANCEL_PX) {
          cancelMobilePointerDownInputFocus();
          clearMobileDirectionGesture();
          clearPassiveFocusBypassIfInactive();
        }
        return;
      }
      if (distance < MOBILE_DIRECTION_DEAD_ZONE_PX) {
        return;
      }
      gesture.active = true;
      gesture.lastStepX = gesture.startX;
      gesture.lastStepY = gesture.startY;
      setMobileDirectionActive(true);
      setMobileDirection(undefined);
      focusTerminalInputSink();
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
    // 中文注释：pointerdown 只允许 click 前的早到 focusin 通过。若整轮触摸结束都没有
    // 真正聚焦终端，就把这次临时放行收回，避免后续 layout/snapshot 误判本地已接管。
    if (event.type === "pointercancel") {
      cancelMobilePointerDownInputFocus();
      clearPassiveFocusBypassIfInactive();
    } else if (!terminalInputHasDomFocus() && !focusedRef.current && !passiveInputFocusRef.current) {
      clearPassiveFocusBypassTimer();
      passiveFocusBypassTimerRef.current = window.setTimeout(() => {
        passiveFocusBypassTimerRef.current = undefined;
        clearPassiveFocusBypassIfInactive();
      }, MOBILE_PASSIVE_FOCUS_BYPASS_SETTLE_MS);
    } else {
      clearPassiveFocusBypassTimer();
      clearPassiveFocusBypassIfInactive();
    }
    clearMobileDirectionGesture();
  };

  const handleTerminalContextMenu = () => {
    clearMobileSelectionLongPress();
    cancelMobilePointerDownGestureOnly();
    clearMobileDirectionGesture();
    clearPassiveFocusBypassIfInactive();
    clearTerminalSelectionDrag();
  };

  const applyFontSize = (terminal: TerminalRendererTerminal, fontSize: number) => {
    if (currentFontSizeRef.current === fontSize) {
      return;
    }
    currentFontSizeRef.current = fontSize;
    // 中文注释：renderer 的 cols/rows 属于运行期状态；字体调整只更新 fontSize，
    // 避免把尺寸配置一起写回造成不同 renderer 的行为差异。
    rendererRef.current?.setOptions({ fontSize });
  };

  const currentTerminalFontSize = () => (mobileInputModeRef.current ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE);

  const cancelLocalResizeReport = () => {
    if (terminalResizeReportFrameRef.current !== undefined) {
      window.cancelAnimationFrame(terminalResizeReportFrameRef.current);
      terminalResizeReportFrameRef.current = undefined;
    }
    terminalResizeReportPassesRef.current = 0;
    terminalResizeReportSizeRef.current = undefined;
    terminalResizeReportSourceRef.current = undefined;
  };

  const beginSnapshotRedrawMask = () => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    cancelLocalResizeReport();
    terminalResizeRequestKeyRef.current = undefined;
    terminalSnapshotRedrawGenerationRef.current += 1;
    host.dataset.termdSnapshotRedraw = "true";
  };

  const beginTerminalResizeStabilizationMask = () => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    host.dataset.termdResizeStabilizing = "true";
    if (terminalResizeStabilizationTimerRef.current !== undefined) {
      window.clearTimeout(terminalResizeStabilizationTimerRef.current);
    }
    // 中文注释：本地 resize 的临时过渡期通常只有几帧；只要没有新的 resize 事件，
    // 就把终端可见表面重新露出来。若过程中又来了一次 focus/layout/mobile 变化，就继续延长遮罩。
    terminalResizeStabilizationTimerRef.current = window.setTimeout(() => {
      terminalResizeStabilizationTimerRef.current = undefined;
      const currentHost = hostRef.current;
      if (currentHost) {
        delete currentHost.dataset.termdResizeStabilizing;
      }
    }, 180);
  };

  const maybeBeginTerminalResizeStabilizationMask = (
    terminal: TerminalRendererTerminal,
    nextSize: { rows: number; cols: number } | undefined,
  ) => {
    if (!nextSize || sameTerminalDimensions(terminal, nextSize)) {
      return;
    }
    beginTerminalResizeStabilizationMask();
  };

  const clearSnapshotRedrawMask = () => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    const generation = terminalSnapshotRedrawGenerationRef.current + 1;
    terminalSnapshotRedrawGenerationRef.current = generation;
    // 中文注释：snapshot 刚写完时终端还会补一到两帧 repaint/fit；等稳定帧后再露出可见表面，
    // 避免刷新时肉眼看到旧 80x24 画面闪一下。
    scheduleDeferredTerminalFrame(() => {
      scheduleDeferredTerminalFrame(() => {
        if (terminalSnapshotRedrawGenerationRef.current === generation) {
          delete host.dataset.termdSnapshotRedraw;
        }
      });
    });
  };

  const isTerminalActivationTarget = (target: EventTarget | null) => {
    const element = target instanceof Element ? target : null;
    return Boolean(element && rendererRef.current?.isActivationTarget(element));
  };
  const hitTerminalSurfaceAtPoint = (
    target: EventTarget | null,
    clientX: number,
    clientY: number,
  ) => {
    const element = target instanceof Element ? target : null;
    if (
      element?.closest("canvas") ||
      element?.closest(".xterm-screen") ||
      element?.closest(".xterm-viewport")
    ) {
      return true;
    }
    const host = hostRef.current;
    const surface = resolveTerminalSurfaceElement(host);
    if (!host || !surface) {
      return false;
    }
    const rect = surface.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) {
      // 中文注释：jsdom / 测试桩里终端表面可能没有真实布局尺寸；此时只要事件目标还在
      // terminal host 内，就把它当成命中了终端文字层，保持与真实浏览器一致的聚焦语义。
      return Boolean(element && host.contains(element));
    }
    return (
      clientX >= rect.left &&
      clientX <= rect.right &&
      clientY >= rect.top &&
      clientY <= rect.bottom
    );
  };
  const hasActiveTerminalFocus = () => focusedRef.current && windowActiveRef.current;
  const terminalInputHasDomFocus = () => {
    const terminalHost = hostRef.current;
    const activeElement = document.activeElement;
    return Boolean(
      windowActiveRef.current &&
      terminalHost &&
      activeElement instanceof HTMLElement &&
      terminalHost.contains(activeElement),
    );
  };
  const terminalDomHasActiveFocus = () =>
    terminalInputHasDomFocus() && !passiveInputFocusRef.current;
  const localTerminalOwnsResizeAuthority = () =>
    hasActiveTerminalFocus() ||
    terminalDomHasActiveFocus() ||
    (windowActiveRef.current && focusActivationArmedRef.current);
  const localTerminalWillOwnResizeAuthority = () =>
    localTerminalOwnsResizeAuthority() || pendingFocusRequestRef.current !== undefined;

  const reportTerminalFocus = (nextFocused: boolean) => {
    if (focusedRef.current === nextFocused) {
      return;
    }
    focusedRef.current = nextFocused;
    setFocused(nextFocused);
    if (nextFocused) {
      passiveInputFocusRef.current = false;
    }
    if (!nextFocused) {
      passiveInputFocusRef.current = false;
      cancelLocalResizeReport();
      suppressPassiveFocusRef.current = true;
    }
    queueCursorReport({ immediate: true });
  };

  const resolveTerminalInputElement = (host: HTMLElement | null = hostRef.current) => {
    if (!host) {
      return undefined;
    }
    return rendererRef.current?.getInputElement(host);
  };

  const focusTerminalInputSink = (
    terminal: TerminalRendererTerminal | null = terminalRef.current,
    options: FocusTerminalInputSinkOptions = {},
  ) => {
    const input = resolveTerminalInputElement();
    if (!input) {
      terminal?.focus();
      return;
    }
    const forceRefocus = Boolean(options.force && mobileInputModeRef.current);
    if (
      mobileInputModeRef.current &&
      (focusedRef.current || focusActivationArmedRef.current || mobileKeyboardOpenRef.current || forceRefocus)
    ) {
      armMobileInputFocusRescue();
    }
    if (document.activeElement === input && !forceRefocus) {
      return;
    }
    terminal?.focus();
    // 中文注释：桌面键盘/IME 输入最终要落到隐藏 textarea；
    // host 只保留给可访问性、selection 和 resize 状态，不作为桌面输入终点。
    // 移动端重新激活软键盘时也只重复 focus；主动 blur 会让真机系统键盘弹出后立刻收起。
    try {
      input.focus({ preventScroll: true });
    } catch {
      input.focus();
    }
  };

  const promotePassiveInputFocusToActive = (source: ResizeSource = "focus") => {
    if (!passiveInputFocusRef.current || !terminalInputHasDomFocus()) {
      return false;
    }
    // 中文注释：显式接管路径（focusRequest / click / shortcut 等）可能发生在
    // helper textarea 已经被动聚焦之后。此时不会再有新的 focusin 可用来升级状态，
    // 需要同步把 passive helper focus 提升成真正 active focus。
    passiveFocusBypassRef.current = false;
    passiveInputFocusRef.current = false;
    focusActivationArmedRef.current = false;
    suppressPassiveFocusRef.current = false;
    mobileViewportLayoutSuppressRef.current = false;
    mobilePointerDownInputFocusRef.current = false;
    reportTerminalFocus(true);
    resizeRef.current?.(source);
    return true;
  };

  const focusTerminalFromTerminalClick = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    const hitSurface = hitTerminalSurfaceAtPoint(target, event.clientX, event.clientY);
    const hitActivationTarget = hitSurface || isTerminalActivationTarget(target);
    if (!hitActivationTarget) {
      return;
    }
    if (terminalSelectionDragRef.current?.active || terminalSelectionFocusPendingRef.current) {
      return;
    }
    if (nowForThrottle() < terminalSelectionClickFocusSuppressUntilRef.current) {
      return;
    }
    const clearedSelection = clearCurrentTerminalSelection();
    const shouldForceMobileKeyboardReactivate = mobileInputModeRef.current && !mobileKeyboardOpenRef.current;
    if (!shouldForceMobileKeyboardReactivate && (hasActiveTerminalFocus() || terminalDomHasActiveFocus())) {
      // 中文注释：终端已经有焦点时，普通 click 只需要清掉旧选区，不需要再补一轮 focus。
      if (clearedSelection || hitSurface) {
        return;
      }
    }
    const wasPinnedToBottom = isTerminalPinnedToBottom();
    windowActiveRef.current = true;
    // 点击终端 frame 是用户显式接管终端的动作；有些浏览器和 jsdom mock
    // 不会把外层 frame 点击稳定转成内部 textarea 的 focusin，因此这里先同步本地聚焦态。
    focusActivationArmedRef.current = false;
    suppressPassiveFocusRef.current = false;
    mobileViewportLayoutSuppressRef.current = false;
    if (mobileInputModeRef.current) {
      armMobileInputFocusRescue();
      armMobileKeyboardResizeSuppress();
    }
    reportTerminalFocus(true);
    mobilePointerDownInputFocusRef.current = false;
    focusTerminalInputSink(terminalRef.current, {
      force: mobileInputModeRef.current && !mobileKeyboardOpenRef.current,
    });
    resizeRef.current?.("focus");
    // 当前客户端接管 PTY 尺寸时，只在用户本来就在底部时继续贴底。
    // 用户已经上滚查看历史时，点击空白处应该只聚焦终端，不能强行跳到最新输出。
    scheduleScrollToBottomIfPinned(wasPinnedToBottom);
  };

  const handleTerminalMouseDownCapture = (event: MouseEvent<HTMLDivElement>) => {
    if (!mobileInputModeRef.current || mouseEventCameFromTouch(event)) {
      return;
    }
    // 中文注释：桌面鼠标点击是明确接管终端的动作，可以结束移动端键盘防抖期。
    // 真机触摸合成的 mouse 事件会带 sourceCapabilities.firesTouchEvents，不能在这里清掉。
    clearMobileKeyboardResizeSuppress();
  };

  const applyTerminalFocusRequest = (requestId?: number) => {
    const activeElement = document.activeElement;
    const terminalHost = hostRef.current;
    if (
      activeElement instanceof HTMLElement &&
      terminalHost &&
      !terminalHost.contains(activeElement) &&
      (Boolean(activeElement.closest(".toolbar, .mobile-menu-popover, .mobile-panel, .files-panel")) ||
        activeElement.isContentEditable ||
        activeElement instanceof HTMLInputElement ||
        activeElement instanceof HTMLTextAreaElement ||
        activeElement instanceof HTMLSelectElement)
    ) {
      // 延迟 focusRequest 不能抢走用户刚聚焦的工作台工具栏、菜单、文件面板等控件；
      // session/daemon 重命名、路径输入、连接表单也属于显式文本编辑，同样不能被终端抢焦点。
      // 否则移动端键盘常驻会破坏顶部工具按钮的键盘/辅助技术操作。
      if (requestId !== undefined && pendingFocusRequestRef.current === requestId) {
        pendingFocusRequestRef.current = undefined;
      }
      return;
    }
    focusActivationArmedRef.current = true;
    suppressPassiveFocusRef.current = false;
    mobileViewportLayoutSuppressRef.current = false;
    clearMobileKeyboardResizeSuppress();
    if (mobileInputModeRef.current) {
      armMobileInputFocusRescue();
    }
    if (promotePassiveInputFocusToActive("focus")) {
      focusTerminalInputSink();
      stabilizeRef.current?.("focus");
      if (requestId !== undefined && pendingFocusRequestRef.current === requestId) {
        pendingFocusRequestRef.current = undefined;
      }
      return;
    }
    focusTerminalInputSink();
    stabilizeRef.current?.("focus");
    if (requestId !== undefined && pendingFocusRequestRef.current === requestId) {
      pendingFocusRequestRef.current = undefined;
    }
  };

  useEffect(() => {
    if (!props.attached || !hostRef.current || terminalRef.current) {
      return undefined;
    }
    recordTermdDiagnostic("terminal_pane_create", {
      outputResetVersion: props.outputResetVersion,
      attached: props.attached,
      sessionSize: props.sessionSize,
    });

    let disposed = false;
    let cleanupMountedRenderer: (() => void) | undefined;
    let cleanupTerminalSelectionNativeListeners: (() => void) | undefined;
    const trackedFrames = new Set<number>();
    const host = hostRef.current;
    const mountRenderer = (renderer: TerminalRendererInstance) => {
      if (disposed) {
        renderer.terminal.dispose();
        return;
      }
      if (!host) {
        renderer.terminal.dispose();
        return;
      }
      const { terminal, fit, search: searchAddon } = renderer;
      terminal.open(host);
      recordTermdDiagnostic("terminal_renderer_mounted", {
        kind: renderer.kind,
        outputResetVersion: props.outputResetVersion,
      });
      cleanupTerminalSelectionNativeListeners = (() => {
        const handleMouseDown = (event: globalThis.MouseEvent) => {
          const target = event.target instanceof Element ? event.target : null;
          updateTerminalSelectionDebug({
            selectionNativeMouseDownTarget: target?.tagName.toLowerCase() ?? undefined,
          });
          if (!hitTerminalSurfaceAtPoint(target, event.clientX, event.clientY)) {
            terminalSelectionCopyGenerationRef.current += 1;
            clearTerminalSelectionDrag();
            updateTerminalSelectionDebug({
              selectionNativeMouseDownStarted: "false",
            });
            return;
          }
          if (mobileInputModeRef.current && nowForThrottle() < terminalSelectionClickFocusSuppressUntilRef.current) {
            // 中文注释：长按选择后的兼容 mousedown 不是输入激活动作；只允许下一次独立 tap 打开键盘。
            event.preventDefault();
            event.stopPropagation();
            event.stopImmediatePropagation();
            return;
          }
          windowActiveRef.current = true;
          focusActivationArmedRef.current = true;
          suppressPassiveFocusRef.current = false;
          if (mobileInputModeRef.current && !mouseEventCameFromTouch(event)) {
            clearMobileKeyboardResizeSuppress();
          }
          markTerminalClipboardSelectionOwner();
          mobileViewportLayoutSuppressRef.current = false;
          if (mobileInputModeRef.current) {
            armMobileInputFocusRescue();
            // 中文注释：移动浏览器要求输入框 focus 发生在触摸/鼠标兼容事件的用户激活链内。
            // 下方自定义选区会拦截 xterm 默认 mousedown；因此这里先同步执行 xterm 原本
            // 会做的 helper textarea 聚焦，避免等 click/rAF 时软键盘已经被系统收回。
            focusTerminalInputSink(terminal);
          }
          const started = startTerminalSelectionDrag(event.clientX, event.clientY);
          updateTerminalSelectionDebug({
            selectionNativeMouseDownStarted: String(started),
          });
          if (!started) {
            focusTerminalInputSink(terminal);
            stabilizeRef.current?.("focus");
            return;
          }
          // 中文注释：这里使用 capture 抢在 renderer 默认鼠标选区前拦截，
          // 让自定义拖拽复制与业务选区语义保持一致。
          event.preventDefault();
          event.stopPropagation();
          event.stopImmediatePropagation();
          terminalSelectionFocusPendingRef.current = true;
        };
        const handleClick = (event: globalThis.MouseEvent) => {
          const target = event.target instanceof Element ? event.target : null;
          if (!hitTerminalSurfaceAtPoint(target, event.clientX, event.clientY)) {
            return;
          }
          if (mobileInputModeRef.current && nowForThrottle() < terminalSelectionClickFocusSuppressUntilRef.current) {
            // 中文注释：同一轮长按选择的 trailing click 不能抢回 helper textarea 焦点。
            event.preventDefault();
            event.stopPropagation();
            event.stopImmediatePropagation();
            return;
          }
          const drag = terminalSelectionDragRef.current;
          if (!drag?.active || drag.dragging || !terminalSelectionFocusPendingRef.current) {
            return;
          }
          // 中文注释：终端文字层可能拦截冒泡阶段 click；对于这种“按下后没有拖拽、
          // 但 click 仍然落在终端内容上”的情况，需要在 capture 阶段直接补回焦点。
          // 同时清掉待选区状态，避免测试桩或极端浏览器时序里遗漏 mouseup 后遗留脏监听。
          clearTerminalSelectionDrag();
          mobileViewportLayoutSuppressRef.current = false;
          focusTerminalInputSink(terminal);
          stabilizeRef.current?.("focus");
        };
        host.addEventListener("mousedown", handleMouseDown, true);
        host.addEventListener("click", handleClick, true);
        return () => {
          host.removeEventListener("mousedown", handleMouseDown, true);
          host.removeEventListener("click", handleClick, true);
        };
      })();
    const requestTrackedFrame = (callback: () => void) => {
      const frameId = scheduleDeferredTerminalFrame(() => {
        trackedFrames.delete(frameId);
        callback();
      });
      trackedFrames.add(frameId);
      return frameId;
    };
    const canUseArmedFocusResizeAuthority = (source: ResizeSource | undefined) => {
      if (source !== "focus") {
        return false;
      }
      // 中文注释：真实浏览器里 terminal.focus() 到 host/textarea 变成 activeElement
      // 之间可能还隔着一轮布局；focusRequest/显式点击触发的首轮 focus resize 不能因为
      // 这个短暂窗口被吞掉，否则共享 PTY 仍会维持旧 rows/cols，首屏就会按旧网格乱掉。
      return windowActiveRef.current && focusActivationArmedRef.current;
    };
    const shouldSuppressMobileInputResize = () => (
      mobileInputModeRef.current &&
      (
        mobileKeyboardOpenRef.current ||
        mobileKeyboardResizeSuppressActive()
      )
    );
    const canReportLocalResizeForSource = (source: ResizeSource | undefined) => {
      if (!source || source === "session" || source === "snapshot" || snapshotRedrawInProgressRef.current) {
        return false;
      }
      if (shouldSuppressMobileInputResize()) {
        return false;
      }
      return (
        hasActiveTerminalFocus() ||
        (windowActiveRef.current && terminalDomHasActiveFocus()) ||
        canUseArmedFocusResizeAuthority(source)
      );
    };
    const scheduleLocalResizeReport = (source: ResizeSource) => {
      const currentReportSource = terminalResizeReportSourceRef.current;
      if (
        !currentReportSource ||
        RESIZE_SOURCE_PRIORITY[source] >= RESIZE_SOURCE_PRIORITY[currentReportSource]
      ) {
        terminalResizeReportSourceRef.current = source;
      }
      if (terminalResizeReportFrameRef.current !== undefined) {
        // 中文注释：已有稳定 resize 检测正在跑时，新的 focus/layout 事件只更新优先级。
        // 如果每次 ResizeObserver/focusin 都重置等待帧数，reload 初期会永远等不到
        // “最终尺寸”，用户看到的就是终端分辨率反复跳动。
        return;
      }
      terminalResizeReportPassesRef.current = Math.max(terminalResizeReportPassesRef.current, 12);
      const runResizeReportPass = () => {
        terminalResizeReportFrameRef.current = undefined;
        const reportSource = terminalResizeReportSourceRef.current;
        if (!canReportLocalResizeForSource(reportSource)) {
          cancelLocalResizeReport();
          return;
        }
        const stableDimensions = fit.proposeStableDimensions?.();
        if (
          !stableDimensions ||
          stableDimensions.rows < MIN_FOCUSED_RESIZE_ROWS ||
          stableDimensions.cols < MIN_FOCUSED_RESIZE_COLS
        ) {
          terminalResizeReportSizeRef.current = undefined;
          terminalResizeReportPassesRef.current -= 1;
          if (terminalResizeReportPassesRef.current > 0) {
            terminalResizeReportFrameRef.current = requestTrackedFrame(runResizeReportPass);
            return;
          }
          terminalResizeReportSourceRef.current = undefined;
          return;
        }
        const terminalHost = hostRef.current;
        const nextSize = {
          rows: stableDimensions.rows,
          cols: stableDimensions.cols,
          pixel_width: terminalHost?.clientWidth ?? 0,
          pixel_height: terminalHost?.clientHeight ?? 0,
        };
        const previousStableSize = terminalResizeReportSizeRef.current;
        if (!previousStableSize || previousStableSize.rows !== nextSize.rows || previousStableSize.cols !== nextSize.cols) {
          // 中文注释：daemon resize 不能用刚出现的一帧 proposal；reload/focus 初期 metrics
          // 可能还在变，必须等同一个 rows/cols 连续稳定几帧后再上报。
          terminalResizeReportSizeRef.current = nextSize;
          terminalResizeReportPassesRef.current = 2;
          terminalResizeReportFrameRef.current = requestTrackedFrame(runResizeReportPass);
          return;
        }
        terminalResizeReportPassesRef.current -= 1;
        if (terminalResizeReportPassesRef.current > 0) {
          terminalResizeReportFrameRef.current = requestTrackedFrame(runResizeReportPass);
          return;
        }
        terminalResizeReportPassesRef.current = 0;
        terminalResizeReportSizeRef.current = undefined;
        terminalResizeReportSourceRef.current = undefined;
        if (!nextSize) {
          return;
        }
        const remoteSize = sessionSizeRef.current;
        if (remoteSize?.rows === nextSize.rows && remoteSize?.cols === nextSize.cols) {
          terminalResizeRequestKeyRef.current = undefined;
          return;
        }
        const nextResizeKey = `${nextSize.rows}:${nextSize.cols}`;
        if (terminalResizeRequestKeyRef.current === nextResizeKey) {
          return;
        }
        terminalResizeRequestKeyRef.current = nextResizeKey;
        onResizeRef.current(nextSize);
      };
      terminalResizeReportFrameRef.current = requestTrackedFrame(runResizeReportPass);
    };
    const dataSubscription = terminal.onData((data) => {
      recordTermdDiagnostic("terminal_pane_on_data", {
        chunkLength: data.length,
      });
      onInputRef.current(data);
    });
    const helperTextarea = renderer.getInputElement(host);
    if (helperTextarea) {
      // 中文注释：真实终端会同时让 host 和隐藏 textarea 具备输入能力；
      // 对用户和 Playwright role locator 来说，外层 host 才是唯一可见终端输入框。
      helperTextarea.setAttribute("aria-hidden", "true");
    }
    const handleHostFocusBridge = (event: FocusEvent) => {
      const target = event.target;
      if (!(target instanceof HTMLElement)) {
        return;
      }
      if (target !== host && target !== terminal.element) {
        return;
      }
      if (terminalSelectionDragRef.current?.active || terminalSelectionFocusPendingRef.current) {
        return;
      }
      // 中文注释：host 只负责把焦点态带进终端；真实的键盘/IME 输入 sink 仍是 textarea。
      if (!helperTextarea || document.activeElement === helperTextarea) {
        return;
      }
      try {
        helperTextarea.focus({ preventScroll: true });
      } catch {
        helperTextarea.focus();
      }
    };
    host.addEventListener("focusin", handleHostFocusBridge, true);
    const isMobileCompositionInput = (event: InputEvent) => {
      const inputType = event.inputType.toLowerCase();
      const recentlyEnded =
        lastMobileCompositionEndAtRef.current > 0 &&
        nowForThrottle() - lastMobileCompositionEndAtRef.current < MOBILE_COMPOSITION_SETTLE_MS;
      return event.isComposing || mobileCompositionActiveRef.current || inputType.includes("composition") || recentlyEnded;
    };
    const handleMobileCompositionStart = () => {
      mobileCompositionActiveRef.current = true;
    };
    const handleMobileCompositionEnd = () => {
      mobileCompositionActiveRef.current = false;
      lastMobileCompositionEndAtRef.current = nowForThrottle();
    };
    const handleMobileBeforeInput = (event: InputEvent) => {
      recordTermdDiagnostic("terminal_pane_beforeinput", {
        inputType: event.inputType,
        hasData: Boolean(event.data),
        defaultPrevented: event.defaultPrevented,
        mobileInputMode: mobileInputModeRef.current,
        passiveInputFocus: passiveInputFocusRef.current,
        terminalInputHasDomFocus: terminalInputHasDomFocus(),
      });
      if (event.defaultPrevented || isMobileCompositionInput(event)) {
        return;
      }
      if (event.inputType === "insertFromPaste" && event.data) {
        // 中文注释：桌面和移动端都可能把原生粘贴落成 beforeinput(insertFromPaste)；
        // 统一在这里接管，并和 readText fallback 共用 sendNativePasteText 去重。
        event.preventDefault();
        event.stopPropagation();
        event.stopImmediatePropagation();
        notePendingPasteShortcutNativePaste();
        sendNativePasteText(event.data);
        return;
      }
      if (!mobileInputModeRef.current || event.inputType !== "insertText" || !event.data) {
        return;
      }

      // iOS/Safari 软键盘有时只给 beforeinput，不走 renderer 的 keydown/keypress。
      // 对移动端非组合文本做兜底，并阻止后续 input，避免同一份内容发送两次。
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      sendTerminalControl(event.data);
    };
    const handleMobilePaste = (event: ClipboardEvent) => {
      if (event.defaultPrevented) {
        return;
      }
      const text = event.clipboardData?.getData("text");
      if (!text) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      notePendingPasteShortcutNativePaste();
      sendNativePasteText(text);
    };
    helperTextarea?.addEventListener("compositionstart", handleMobileCompositionStart, true);
    helperTextarea?.addEventListener("compositionend", handleMobileCompositionEnd, true);
    helperTextarea?.addEventListener("beforeinput", handleMobileBeforeInput, true);
    helperTextarea?.addEventListener("paste", handleMobilePaste, true);
    const cursorMoveSubscription = terminal.onCursorMove(() => queueCursorReport());
    const writeParsedSubscription = terminal.onWriteParsed(() => queueCursorReport());
    const scrollSubscription = terminal.onScroll(() => {
      const scrollState = rendererRef.current?.scrollState(terminal);
      if (forcedCursorBottomModeRef.current && scrollState) {
        if (Math.abs(scrollState.viewportY - scrollState.cursorBottomLine) > TERMINAL_BOTTOM_EPSILON) {
          forcedCursorBottomModeRef.current = false;
        }
      }
      if (
        scrollState &&
        !bottomScrollProgrammaticRef.current &&
        scrollState.viewportY < Math.max(0, scrollState.baseY - TERMINAL_BOTTOM_EPSILON)
      ) {
        invalidateBottomScrollFollow();
      }
      scheduleTerminalScrollPosition();
    });
    const terminalFrame = frameRef.current;
    terminalFrame?.addEventListener("wheel", handleTerminalWheel, { capture: true, passive: false });
    const handleTerminalCopyShortcut = (event: KeyboardEvent) => {
      if (event.defaultPrevented || !isTerminalCopyShortcut(event)) {
        return;
      }
      const terminal = terminalRef.current;
      if (!terminal?.hasSelection() || !terminalShouldHandleClipboardEventTarget(event.target)) {
        return;
      }
      const selection = currentTerminalSelectionText(terminal);
      if (!selection) {
        return;
      }
      const copied =
        requestNativeTerminalCopy(selection) ||
        copyCurrentTerminalSelection({ force: true, selectionOverride: selection });
      if (!copied) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
    };
    const handleTerminalPasteShortcut = (event: KeyboardEvent) => {
      if (event.defaultPrevented || !isTerminalPasteShortcut(event)) {
        return;
      }
      // 中文注释：Shift+Insert 只在当前终端真有焦点时接管，避免影响页面其他输入控件。
      if (!terminalDomHasActiveFocus()) {
        return;
      }
      // 中文注释：这里不阻止默认 paste。浏览器若能把原生粘贴送到隐藏 textarea，
      // 那条路径兼容性最好；只有当前事件循环结束后仍未看到原生 paste，
      // 才启动 readText() 兜底，避免 fallback 与原生链路抢跑。
      pasteShortcutSequenceRef.current += 1;
      const shortcutId = pasteShortcutSequenceRef.current;
      pendingPasteShortcutRef.current = {
        id: shortcutId,
        nativePasteObserved: false,
        fallbackStarted: false,
      };
      schedulePasteShortcutFallback(shortcutId);
    };
    const handleTerminalCopyEvent = (event: ClipboardEvent) => {
      if (event.defaultPrevented) {
        return;
      }
      const terminal = terminalRef.current;
      if (!terminal?.hasSelection() || !terminalShouldHandleClipboardEventTarget(event.target)) {
        return;
      }
      if (terminalNativeCopyCommandInFlightRef.current) {
        terminalNativeCopyCommandHandledRef.current = true;
      }
      const copied = copyCurrentTerminalSelection({
        force: true,
        clipboardData: event.clipboardData,
        allowProgrammaticClipboardFallback: false,
      });
      if (!copied) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
    };
    const updateTerminalClipboardSelectionOwnerFromTarget = (eventTarget: EventTarget | null) => {
      const host = hostRef.current;
      if (!host) {
        clearTerminalClipboardSelectionOwner();
        return;
      }
      if (eventTarget instanceof Node && host.contains(eventTarget)) {
        markTerminalClipboardSelectionOwner();
        return;
      }
      clearTerminalClipboardSelectionOwner();
    };
    const handleTerminalClipboardContextMouseDown = (event: globalThis.MouseEvent) => {
      updateTerminalClipboardSelectionOwnerFromTarget(event.target);
    };
    const handleTerminalClipboardContextFocusIn = (event: FocusEvent) => {
      updateTerminalClipboardSelectionOwnerFromTarget(event.target);
    };
    document.addEventListener("keydown", handleTerminalCopyShortcut, true);
    document.addEventListener("keydown", handleTerminalPasteShortcut, true);
    document.addEventListener("copy", handleTerminalCopyEvent, true);
    document.addEventListener("mousedown", handleTerminalClipboardContextMouseDown, true);
    document.addEventListener("focusin", handleTerminalClipboardContextFocusIn, true);
    const selectionSubscription = terminal.onSelectionChange(() => {
      setTerminalSelectionAvailable(terminal.hasSelection() && Boolean(terminal.getSelection()));
      if (terminalSelectionDragRef.current?.active) {
        return;
      }
      if (nowForThrottle() < terminalNativeSelectionCopySuppressUntilRef.current) {
        return;
      }
      if (!terminal.hasSelection()) {
        return;
      }
      const selection = terminal.getSelection();
      if (!selection) {
        return;
      }
      markTerminalClipboardSelectionOwner();
      // 终端原生选择完成后同步复制到系统剪贴板；复制失败时不打断终端交互。
      copyCurrentTerminalSelection({ selectionOverride: selection });
    });
    // 本地终端只有在当前浏览器窗口聚焦终端时才把尺寸写回 shared PTY。
    // 未聚焦客户端按 daemon 确认的 session rows/cols 渲染，不再做本地等比缩放。
    const resize = (source: ResizeSource = "layout") => {
      if (source === "layout") {
        refreshMobileViewportLayoutSuppressFromWindow();
      }
      const isSuppressedMobileViewportLayout = (proposed?: { rows: number; cols: number }, referenceCols?: number) => {
        if (source !== "layout" || !mobileInputModeRef.current || !mobileViewportLayoutSuppressRef.current) {
          return false;
        }
        if (proposed && referenceCols !== undefined && proposed.cols !== referenceCols) {
          // 中文注释：软键盘只改变高度；如果拟合列数已经变化，说明这是横竖屏、
          // 分屏或真实容器宽度变化，必须让 focused 客户端继续上报 PTY resize。
          mobileViewportLayoutSuppressRef.current = false;
          return false;
        }
        return true;
      };
      if (snapshotRedrawInProgressRef.current) {
        const proposed = fit.proposeDimensions();
        const referenceCols = sessionSizeRef.current?.cols ?? terminal.cols;
        const layoutFromMobileViewport =
          isSuppressedMobileViewportLayout(proposed, referenceCols);
        if (
          source === "focus" ||
          (source === "layout" && hasActiveTerminalFocus() && !layoutFromMobileViewport)
        ) {
          // 中文注释：snapshot 字节写入期间不能改变终端尺寸；但用户主动聚焦/窗口变化
          // 不能丢，等 snapshot 渲染完成后再补一次真实 resize 上报。
          pendingResizeAfterSnapshotRef.current = source;
        }
        // 中文注释：snapshot 字节按生成时的列宽解释；写入完成前禁止 layout/session resize
        // 把终端改回旧尺寸，否则宽行换行和光标位置会被错误解析。
        return;
      }
      const terminalHost = hostRef.current;
      if (!terminalHost) {
        return;
      }
      const proposed = fit.proposeDimensions();
      const hostWidth = terminalHost.clientWidth;
      const hostHeight = terminalHost.clientHeight;
      const remoteSize = sessionSizeRef.current;
      const terminalHasActiveFocus = hasActiveTerminalFocus();
      const layoutFromMobileViewport =
        isSuppressedMobileViewportLayout(proposed, remoteSize?.cols ?? terminal.cols);
      if (source === "mobile-viewport" || layoutFromMobileViewport || shouldSuppressMobileInputResize()) {
        // 中文注释：移动端软键盘只改变可视窗口，不代表 shared PTY 的 rows/cols
        // 需要变化。这里也不做本地 fit，避免 helper textarea 在键盘动画期被
        // xterm 重新锚定到新网格，导致移动浏览器认为输入目标失稳而收起键盘。
        applyFontSize(terminal, currentTerminalFontSize());
        terminal.refresh(0, Math.max(0, terminal.rows - 1));
        alignMobileViewportToTerminalBottom(terminal);
        syncTerminalInputAnchor(terminal, "refresh");
        queueCursorReport({ immediate: true });
        return;
      }
      const armedFocusResizeAuthority = canUseArmedFocusResizeAuthority(source);
      const terminalCanReportActiveFocus =
        terminalHasActiveFocus ||
        (windowActiveRef.current && terminalDomHasActiveFocus()) ||
        armedFocusResizeAuthority;
      const mobileKeyboardIsOpen = shouldSuppressMobileInputResize();
      const canReportLocalResize =
        source !== "session" &&
        source !== "snapshot" &&
        !mobileKeyboardIsOpen &&
        terminalCanReportActiveFocus;
      const canFitLocalAfterSnapshot =
        source === "snapshot" &&
        !mobileKeyboardIsOpen &&
        (terminalHasActiveFocus || terminalDomHasActiveFocus());
      const wasPinnedToBottom = isTerminalPinnedToBottom(terminal);
      if (proposed) {
        clientSizeRef.current = {
          rows: proposed.rows,
          cols: proposed.cols,
          pixel_width: hostWidth,
          pixel_height: hostHeight,
        };
        const snapshotSize = lastRenderedSnapshotSizeRef.current;
        if (
          canReportLocalResize &&
          snapshotSize &&
          (snapshotSize.rows !== proposed.rows || snapshotSize.cols !== proposed.cols) &&
          (
            pendingSnapshotHistoryRepairRef.current?.snapshotRows !== snapshotSize.rows ||
            pendingSnapshotHistoryRepairRef.current?.snapshotCols !== snapshotSize.cols
          )
        ) {
          // 中文注释：页面初开时如果先按 daemon 的旧 snapshot 画出来，用户随后第一次聚焦
          // 才把 PTY resize 到当前浏览器尺寸，那么现有 buffer 仍是旧网格折行出来的内容。
          // 这里先记下“需要用新尺寸再拉一次 full snapshot”，等 daemon 真正确认 rows/cols
          // 后统一重建历史和当前屏幕，避免 Codex 底部输入区相对正文抬高/压低一行。
          pendingSnapshotHistoryRepairRef.current = {
            snapshotRows: snapshotSize.rows,
            snapshotCols: snapshotSize.cols,
            createdAtMs: nowForThrottle(),
          };
          recordTermdDiagnostic("terminal_snapshot_history_repair_pending", {
            snapshotRows: snapshotSize.rows,
            snapshotCols: snapshotSize.cols,
            proposedRows: proposed.rows,
            proposedCols: proposed.cols,
          });
          // 中文注释：daemon 对新尺寸的 ack 可能已经先于用户聚焦到达。
          // 如果 repair intent 是在后续 focus/layout resize 中才首次建立，就不能再只等
          // 下一次 sessionSize 变化；这里要立刻重评估一次，避免 pending repair 永远挂住。
          evaluatePendingSnapshotHistoryRepair();
        }
      }
      if (!canReportLocalResize) {
        applyFontSize(terminal, currentTerminalFontSize());
        if (
          canFitLocalAfterSnapshot &&
          proposed &&
          proposed.rows >= MIN_FOCUSED_RESIZE_ROWS &&
          proposed.cols >= MIN_FOCUSED_RESIZE_COLS
        ) {
          // 中文注释：snapshot 后仍要把本地终端贴合当前容器，避免内容停在旧高度；
          // 但这是被动重绘，不能回写 daemon，否则不同分辨率客户端会形成 resize/snapshot 风暴。
          maybeBeginTerminalResizeStabilizationMask(terminal, proposed);
          if (!sameTerminalDimensions(terminal, proposed)) {
            fit.fit();
          }
          scheduleScrollToBottomIfPinned(wasPinnedToBottom);
          queueCursorReport({ immediate: true });
          return;
        }
        if (
          source === "session" &&
          remoteSize &&
          !localTerminalWillOwnResizeAuthority() &&
          !sameTerminalDimensions(terminal, remoteSize)
        ) {
          // 中文注释：未聚焦客户端虽然不能把自己的布局写回 shared PTY，但 daemon/supervisor
          // 已确认的新 rows/cols 仍然是权威尺寸。这里必须被动跟随远端 grid，否则后续
          // output/snapshot 仍会按旧列宽解释，vim/top 这类全屏界面会直接错位。
          terminal.resize(remoteSize.cols, remoteSize.rows);
        }
        // 中文注释：窗口失焦后的 layout/blur 抖动不能把本地终端立即缩回远端尺寸。
        // 只有 daemon/supervisor 真的确认了新的 sessionSize，未聚焦客户端才跟随权威 grid；
        // 否则回到页面时会先看到一次旧 grid/旧布局，再被 focus resize 拉回当前容器。
        scheduleScrollToBottomIfPinned(wasPinnedToBottom);
        queueCursorReport({ immediate: true });
        return;
      }
      applyFontSize(terminal, currentTerminalFontSize());
      // 移动端软键盘或外层 grid 短暂重排时可能把终端容器压到 0 高。
      // 这种尺寸不能写回 shared PTY，否则其他客户端会被同步成一行终端。
      if (proposed && proposed.rows >= MIN_FOCUSED_RESIZE_ROWS && proposed.cols >= MIN_FOCUSED_RESIZE_COLS) {
        const nextResizeKey = `${proposed.rows}:${proposed.cols}`;
        const approvedBySession =
          remoteSize?.rows === proposed.rows &&
          remoteSize?.cols === proposed.cols;
        if (approvedBySession) {
          if (terminalResizeRequestKeyRef.current === nextResizeKey) {
            terminalResizeRequestKeyRef.current = undefined;
          }
          maybeBeginTerminalResizeStabilizationMask(terminal, proposed);
          if (!sameTerminalDimensions(terminal, proposed)) {
            fit.fit();
          }
          scheduleScrollToBottomIfPinned(wasPinnedToBottom);
          queueCursorReport({ immediate: true });
          return;
        }
        if (terminalResizeRequestKeyRef.current === nextResizeKey) {
          maybeBeginTerminalResizeStabilizationMask(terminal, proposed);
          if (!sameTerminalDimensions(terminal, proposed)) {
            fit.fit();
          }
          scheduleScrollToBottomIfPinned(wasPinnedToBottom);
          queueCursorReport({ immediate: true });
          return;
        }
        // 只有拥有本地 resize 权限时才向 daemon 请求新尺寸。这里把本地 fit 也放进
        // 同一条稳定帧通道里，避免 reload/focus 期间把临时测量先写进终端。
        maybeBeginTerminalResizeStabilizationMask(terminal, proposed);
        if (!sameTerminalDimensions(terminal, proposed)) {
          fit.fit();
          scheduleScrollToBottomIfPinned(wasPinnedToBottom);
        }
        scheduleLocalResizeReport(source);
        queueCursorReport({ immediate: true });
      }
    };
    resizeRef.current = resize;
    const refreshTerminal = (source: ResizeSource = "layout") => {
      resize(source);
      terminal.refresh(0, Math.max(0, terminal.rows - 1));
      syncTerminalInputAnchor(terminal, "refresh");
    };
    const stabilizeTerminal = (source: ResizeSource = "layout") => {
      // 终端在 CSS grid / 右侧文件 panel 同步变化时可能先按旧尺寸完成 open/write。
      // 连续两帧刷新可以等浏览器完成布局后再重算 viewport；同一轮内的 focus、
      // ResizeObserver 和 snapshot 回调必须合并，否则会把刷新时的临时尺寸反复写回 PTY。
      const currentSource = terminalStabilizeSourceRef.current;
      if (!currentSource || RESIZE_SOURCE_PRIORITY[source] >= RESIZE_SOURCE_PRIORITY[currentSource]) {
        terminalStabilizeSourceRef.current = source;
      }
      terminalStabilizePassesRef.current = Math.max(terminalStabilizePassesRef.current, 2);
      if (terminalStabilizeFrameRef.current !== undefined) {
        return;
      }
      const runStabilizePass = () => {
        terminalStabilizeFrameRef.current = undefined;
        const nextSource = terminalStabilizeSourceRef.current ?? source;
        terminalStabilizeSourceRef.current = undefined;
        refreshTerminal(nextSource);
        terminalStabilizePassesRef.current -= 1;
        if (terminalStabilizePassesRef.current <= 0) {
          terminalStabilizePassesRef.current = 0;
          return;
        }
        terminalStabilizeFrameRef.current = requestTrackedFrame(runStabilizePass);
      };
      terminalStabilizeFrameRef.current = requestTrackedFrame(runStabilizePass);
    };
    const drainOutput = createTerminalOutputDrain({
      terminal,
      sessionSizeRef,
      isDisposed: () => disposed,
      requestTrackedFrame,
      cancelTrackedFrame: cancelDeferredTerminalFrame,
      sameTerminalDimensions,
      isTerminalPinnedToBottom,
      scrollToBottom,
      scheduleScrollToBottom,
      queueCursorReport,
      scheduleTerminalScrollPosition,
      beginBottomScrollFollow,
      isBottomScrollFollowActive,
      onTerminalResync: (lastTerminalSeq) => {
        pendingResizeAfterSnapshotRef.current = undefined;
        terminalResizeRequestKeyRef.current = undefined;
        onTerminalResyncRef.current?.(lastTerminalSeq);
      },
      onTerminalSeqRendered: (terminalSeq) => onTerminalSeqRenderedRef.current?.(terminalSeq),
      onTerminalSizeRendered: (size) => onTerminalSizeRenderedRef.current?.(size),
      onTerminalOutputRendered: noteTerminalOutputRendered,
      onTerminalOutputIdle: noteTerminalOutputIdle,
      onSnapshotRedrawBegin: beginSnapshotRedrawMask,
      hasTerminalInputFocus: terminalInputHasDomFocus,
      restoreTerminalInputFocus: () => focusTerminalInputSink(terminal),
      shouldScrollSnapshotToBottom: (item) => !(item.revealHistory || terminalRevealHistoryAfterSnapshotRef.current),
      // 中文注释：snapshot 会先按 daemon 生成时的尺寸重放历史屏幕；重放完成后必须
      // 再按当前浏览器容器 fit 一次，否则后续输出会沿旧 24 行滚动，最终悬在上半截。
      onSnapshotRendered: (item) => {
        lastRenderedSnapshotSizeRef.current = {
          rows: item.size.rows,
          cols: item.size.cols,
        };
        clearSnapshotRedrawMask();
        const revealHistory = item.revealHistory || terminalRevealHistoryAfterSnapshotRef.current;
        terminalRevealHistoryAfterSnapshotRef.current = false;
        if (revealHistory) {
          // 中文注释：用户滚轮触发的 full snapshot 是为了看历史，不是普通重连。
          // snapshot 重放后几帧内禁止 stabilize/refresh 把刚露出的历史又贴回底部。
          terminalRevealHistorySuppressBottomUntilRef.current = nowForThrottle() + 900;
          invalidateBottomScrollFollow();
        } else {
          terminalRevealHistorySuppressBottomUntilRef.current = 0;
        }
        const pendingResizeSource = pendingResizeAfterSnapshotRef.current;
        pendingResizeAfterSnapshotRef.current = undefined;
        const revealHistoryAfterFit = () => {
          if (!revealHistory) {
            return;
          }
          const revealHistoryGeneration = terminalSnapshotRedrawGenerationRef.current;
          let attempts = 0;
          const revealPass = () => {
            requestTrackedFrame(() => {
              if (terminalSnapshotRedrawGenerationRef.current !== revealHistoryGeneration) {
                return;
              }
              const currentTerminal = terminalRef.current;
              const scrollState = rendererRef.current?.scrollState(currentTerminal ?? undefined);
              if (!currentTerminal || !scrollState || scrollState.baseY <= 0) {
                attempts += 1;
                if (attempts < 10) {
                  revealPass();
                }
                return;
              }
              // 中文注释：这次 snapshot 是用户向上滚触发的历史拉取。历史回来后直接退回
              // 一屏左右，避免第一次滚轮只完成后台拉取却仍停在最新输出底部。
              currentTerminal.scrollToLine(Math.max(0, scrollState.baseY - currentTerminal.rows));
              syncTerminalInputAnchor(currentTerminal, "scroll");
              scheduleTerminalScrollPosition({ immediate: true });
              const nextState = rendererRef.current?.scrollState(currentTerminal);
              if ((nextState?.viewportY ?? scrollState.baseY) >= scrollState.baseY - TERMINAL_BOTTOM_EPSILON) {
                attempts += 1;
                if (attempts < 10) {
                  revealPass();
                }
              }
            });
          };
          revealPass();
        };
        const replayPendingResize = () => {
          if (!pendingResizeSource) {
            return;
          }
          requestTrackedFrame(() => resizeRef.current?.(pendingResizeSource));
        };
        const preferredSize = measurePreferredClientSize();
        const suppressSnapshotResizeForMobileViewport =
          mobileInputModeRef.current &&
          mobileViewportLayoutSuppressRef.current &&
          (
            preferredSize === undefined ||
            preferredSize.cols === item.size.cols
          );
        const shouldRepairSnapshotHistory =
          !revealHistory &&
          !suppressSnapshotResizeForMobileViewport &&
          localTerminalWillOwnResizeAuthority() &&
          preferredSize !== undefined &&
          (preferredSize.rows !== item.size.rows || preferredSize.cols !== item.size.cols);
        if (shouldRepairSnapshotHistory) {
          pendingSnapshotHistoryRepairRef.current = {
            snapshotRows: item.size.rows,
            snapshotCols: item.size.cols,
            createdAtMs: nowForThrottle(),
          };
          evaluatePendingSnapshotHistoryRepair();
        } else if (
          pendingSnapshotHistoryRepairRef.current?.snapshotRows === item.size.rows &&
          pendingSnapshotHistoryRepairRef.current?.snapshotCols === item.size.cols
        ) {
          clearPendingSnapshotHistoryRepair();
        }
        if (localTerminalOwnsResizeAuthority()) {
          refreshTerminal("snapshot");
          stabilizeTerminal("snapshot");
          if (suppressSnapshotResizeForMobileViewport && !pendingResizeSource) {
            revealHistoryAfterFit();
            return;
          }
          // 中文注释：reload/重连时 daemon snapshot 可能仍是旧的 80x24。若当前客户端
          // 已经重新聚焦终端，snapshot 重放完成后必须按 focus 路径把真实浏览器尺寸
          // 写回 daemon/supervisor；仅本地 fit 会让下一次 attach 继续拿到旧分辨率。
          requestTrackedFrame(() => resizeRef.current?.(pendingResizeSource ?? "focus"));
          revealHistoryAfterFit();
          return;
        }
        stabilizeTerminal("snapshot");
        replayPendingResize();
        revealHistoryAfterFit();
      },
    });
    stabilizeRef.current = stabilizeTerminal;
    const cleanupFocusResizeListeners = installTerminalFocusResizeListeners({
      host,
      focusOutSettleMs: FOCUS_OUT_SETTLE_MS,
      isPassiveInputTarget: (target) => target === helperTextarea,
      isMobileInputMode: () => mobileInputModeRef.current,
      isMobileKeyboardOpen: () => mobileKeyboardOpenRef.current,
      reportTerminalFocus,
      queueCursorReport,
      restoreTerminalInputFocus: () => focusTerminalInputSink(terminal),
      scheduleScrollToBottomIfPinned,
      resize,
    });
    terminalRef.current = terminal;
    fitRef.current = fit;
    searchAddonRef.current = searchAddon;
    rendererRef.current = renderer;
    outputResetVersionRef.current = props.outputResetVersion;
    confirmOutputResetApplied(props.outputResetVersion);
    markWriterNeedsRefreshAndScroll();
    // attach 输出可能早于终端初始化到达；创建实例时先取走待写队列，避免首屏输出丢失。
    drainOutputRef.current = drainOutput;
    drainOutput();
    queueCursorReport({ immediate: true });
    scheduleTerminalScrollPosition({ immediate: true });

    // 初次 attach 只做本地 fit；用户聚焦该终端时才接管 shared PTY 的远端尺寸。
    stabilizeTerminal();
    if (terminalDomHasActiveFocus()) {
      // 中文注释：reload 后用户或测试可能在终端表面 mount 前已经把焦点放到 host；
      // renderer 就绪后必须补一次 focus resize，否则 daemon/supervisor 会继续停在旧 snapshot 尺寸。
      reportTerminalFocus(true);
      stabilizeTerminal("focus");
      focusTerminalInputSink(terminal);
    }
    if (pendingFocusRequestRef.current !== undefined) {
      requestTrackedFrame(() => applyTerminalFocusRequest(pendingFocusRequestRef.current));
    }
    const handleWindowResize = () => resize("layout");
    window.addEventListener("resize", handleWindowResize);
    const resizeObserver =
      typeof ResizeObserver === "undefined"
        ? undefined
        : new ResizeObserver(() => {
          stabilizeTerminal("layout");
        });
    resizeObserver?.observe(host);
    if (scrollportRef.current) {
      resizeObserver?.observe(scrollportRef.current);
    }

      cleanupMountedRenderer = () => {
      recordTermdDiagnostic("terminal_pane_dispose", {
        outputResetVersion: outputResetVersionRef.current,
        attached: props.attached,
        rendererKind: renderer.kind,
      });
      disposed = true;
      for (const frameId of trackedFrames) {
        cancelDeferredTerminalFrame(frameId);
      }
      trackedFrames.clear();
      if (cursorFrameRef.current !== undefined) {
        cancelDeferredTerminalFrame(cursorFrameRef.current);
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
      clearPassiveFocusBypassTimer();
      mobilePointerDownInputFocusRef.current = false;
      clearMobileKeyboardResizeSuppress();
      if (terminalResizeStabilizationTimerRef.current !== undefined) {
        window.clearTimeout(terminalResizeStabilizationTimerRef.current);
        terminalResizeStabilizationTimerRef.current = undefined;
      }
      if (bottomScrollFrameRef.current !== undefined) {
        cancelDeferredTerminalFrame(bottomScrollFrameRef.current);
        bottomScrollFrameRef.current = undefined;
      }
      lastCursorReportAtRef.current = 0;
      bottomScrollPassesRef.current = 0;
      bottomScrollGenerationRef.current += 1;
      if (terminalScrollFrameRef.current !== undefined) {
        cancelDeferredTerminalFrame(terminalScrollFrameRef.current);
        terminalScrollFrameRef.current = undefined;
      }
      if (terminalScrollTimerRef.current !== undefined) {
        window.clearTimeout(terminalScrollTimerRef.current);
        terminalScrollTimerRef.current = undefined;
      }
      clearTerminalServerScrollbackResyncIdleTimer();
      clearMobileSelectionLongPress();
      clearMobileDirectionGesture();
      resetMobileCursorViewportWindow();
      lastTerminalScrollReportAtRef.current = 0;
      terminalLastServerScrollbackResyncAtRef.current = 0;
      terminalSelectionFocusPendingRef.current = false;
      terminalNativeSelectionCopySuppressUntilRef.current = 0;
      clearTerminalClipboardSelectionOwner();
      terminalNativeCopyCommandInFlightRef.current = false;
      terminalNativeCopyCommandHandledRef.current = false;
      terminalRevealHistoryAfterSnapshotRef.current = false;
      terminalRevealHistorySuppressBottomUntilRef.current = 0;
      terminalOutputIdleSinceRef.current = undefined;
      terminalSnapshotRedrawGenerationRef.current += 1;
      clearPendingSnapshotHistoryRepair();
      window.removeEventListener("resize", handleWindowResize);
      cleanupFocusResizeListeners();
      resizeObserver?.disconnect();
      helperTextarea?.removeEventListener("compositionstart", handleMobileCompositionStart, true);
      helperTextarea?.removeEventListener("compositionend", handleMobileCompositionEnd, true);
      helperTextarea?.removeEventListener("beforeinput", handleMobileBeforeInput, true);
      helperTextarea?.removeEventListener("paste", handleMobilePaste, true);
      host.removeEventListener("focusin", handleHostFocusBridge, true);
      terminalFrame?.removeEventListener("wheel", handleTerminalWheel, true);
      document.removeEventListener("keydown", handleTerminalCopyShortcut, true);
      document.removeEventListener("keydown", handleTerminalPasteShortcut, true);
      document.removeEventListener("copy", handleTerminalCopyEvent, true);
      document.removeEventListener("mousedown", handleTerminalClipboardContextMouseDown, true);
      document.removeEventListener("focusin", handleTerminalClipboardContextFocusIn, true);
      if (pendingPasteShortcutTimerRef.current !== undefined) {
        window.clearTimeout(pendingPasteShortcutTimerRef.current);
        pendingPasteShortcutTimerRef.current = undefined;
      }
      pendingPasteShortcutRef.current = undefined;
      clearTerminalSelectionDrag();
      dataSubscription.dispose();
      cursorMoveSubscription.dispose();
      writeParsedSubscription.dispose();
      scrollSubscription.dispose();
      selectionSubscription.dispose();
      cleanupTerminalSelectionNativeListeners?.();
      cleanupTerminalSelectionNativeListeners = undefined;
      terminal.dispose();
      // 清理 host 里的旧终端 DOM，避免切换 session 后旧终端明文或隐藏 textarea 残留。
      host.replaceChildren();
      // 中文注释：真实 renderer dispose 不会清理应用层 debug dataset；
      // 即使该镜像只在 dev/test/E2E 构建启用，detach/reset 后也不能残留旧终端明文。
      delete host.dataset.termdBuffer;
      delete host.dataset.termdCols;
      delete host.dataset.termdRows;
      delete host.dataset.termdActualCols;
      delete host.dataset.termdActualRows;
      delete host.dataset.termdViewportYRaw;
      delete host.dataset.termdScrollbackLength;
      delete host.dataset.termdViewportText;
      delete host.dataset.termdHasSelection;
      delete host.dataset.termdSelection;
      delete host.dataset.termdSelectionPosition;
      delete host.dataset.termdSnapshotRedraw;
      delete host.dataset.termdResizeStabilizing;
      clearTerminalSelectionDebug();
      delete host.dataset.buffer;
      terminalRef.current = null;
      fitRef.current = null;
      searchAddonRef.current = null;
      rendererRef.current = null;
      setTerminalSelectionAvailable(false);
      resizeRef.current = undefined;
      stabilizeRef.current = undefined;
      pendingResizeAfterSnapshotRef.current = undefined;
      terminalResizeRequestKeyRef.current = undefined;
      if (terminalResizeReportFrameRef.current !== undefined) {
        cancelDeferredTerminalFrame(terminalResizeReportFrameRef.current);
        terminalResizeReportFrameRef.current = undefined;
      }
      terminalResizeReportPassesRef.current = 0;
      terminalResizeReportSizeRef.current = undefined;
      terminalStabilizeSourceRef.current = undefined;
      if (terminalStabilizeFrameRef.current !== undefined) {
        cancelDeferredTerminalFrame(terminalStabilizeFrameRef.current);
      }
      terminalStabilizeFrameRef.current = undefined;
      terminalStabilizePassesRef.current = 0;
      mobileViewportLayoutSuppressRef.current = false;
      previousMobileViewportMetricsRef.current = undefined;
      terminalSelectionCopyGenerationRef.current += 1;
      pendingFocusRequestRef.current = undefined;
      drainOutputRef.current = () => undefined;
      resetWriterState();
      forcedCursorBottomModeRef.current = false;
      focusedRef.current = false;
      mobileCompositionActiveRef.current = false;
      lastMobileCompositionEndAtRef.current = 0;
      clientSizeRef.current = undefined;
      focusActivationArmedRef.current = false;
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      suppressPassiveFocusRef.current = true;
      windowActiveRef.current = true;
      setFocused(false);
      setCopyToastVisible(false);
      setMobileDirectionActive(false);
      setMobileDirection(undefined);
    };
    };
    const rendererResult = createTerminalRendererInstance({
      terminalOptions: {
        // 中文注释：同 session 的 full snapshot repair / reconnect 会重建 xterm 实例。
        // 如果这里回退到 xterm 默认 80x24，调试镜像和真实 DOM 都会短暂暴露错误网格，
        // 造成“108x35 -> 80x24 -> 108x35”的闪回。新实例应优先从当前权威 sessionSize 起步。
        cols: props.sessionSize?.cols ?? 80,
        rows: props.sessionSize?.rows ?? 24,
        cursorBlink: true,
        cursorStyle: "block",
        cursorInactiveStyle: "outline",
        // MVP 只需要普通终端渲染；屏幕阅读模式会额外维持可访问性树，增加高输出场景的内存和 CPU 压力。
        screenReaderMode: false,
        scrollback: 2000,
        smoothScrollDuration: 0,
        // 中文注释：xterm 会按 terminal cells 排版；如果主字体不覆盖 CJK，
        // 如果主字体本身不覆盖 CJK，浏览器 fallback 过来的非终端字形往往只占
        // 约 1 个 cell 的视觉宽度，画面就会变成“字字分家”。这里优先使用本机
        // 常见且真实存在的 CJK monospace 字体，保证中英混排在 fresh attach 时也能稳定对齐。
        fontFamily: '"Noto Sans Mono CJK SC", "WenQuanYi Zen Hei Mono", "IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
        fontSize: props.mobileInputMode ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE,
        convertEol: true,
        theme: terminalTheme(props.theme ?? "dark"),
      },
      searchOptions: { highlightLimit: 1000 },
    });
    if ("then" in rendererResult) {
      void rendererResult.then(mountRenderer);
    } else {
      mountRenderer(rendererResult);
    }
    return () => {
      disposed = true;
      cleanupMountedRenderer?.();
      cleanupMountedRenderer = undefined;
    };
  }, [props.attached]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    // 中文注释：xterm.js 支持运行期原地换 theme；这里直接更新 renderer options，
    // 不再依赖 outputResetVersion 触发整实例重建。
    rendererRef.current?.setOptions({ theme: terminalTheme(props.theme ?? "dark") });
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
    recordTermdDiagnostic("terminal_pane_output_reset", {
      previousResetVersion: outputResetVersionRef.current,
      outputResetVersion: props.outputResetVersion,
      terminalHasDomFocus: terminalInputHasDomFocus(),
      writerReset: true,
    });
    // 中文注释：同一 session 的 full snapshot / resync 不能再销毁整棵 xterm DOM。
    // 否则 helper textarea 会被替换，浏览器会把输入焦点直接打掉，表现成“闪一下再失焦”。
    // 这里原地 reset，并推进 writer generation 让旧 write callback 全部失效。
    const shouldRestoreTerminalFocusAfterReset = terminalInputHasDomFocus();
    resetWriterState();
    invalidateBottomScrollFollow();
    resetMobileCursorViewportWindow();
    terminal.reset();
    if (shouldRestoreTerminalFocusAfterReset) {
      // 中文注释：部分浏览器/renderer reset 会让同一个 helper textarea 短暂掉到 body；
      // 只有 reset 前焦点就在终端内时才恢复，避免抢走工具栏、文件面板或表单焦点。
      focusTerminalInputSink(terminal);
    }
    stabilizeRef.current?.(mobileInputModeRef.current ? "mobile-viewport" : "layout");
    outputResetVersionRef.current = props.outputResetVersion;
    confirmOutputResetApplied(props.outputResetVersion);
    markWriterNeedsRefreshAndScroll();
    drainOutputRef.current();
  }, [props.outputResetVersion]);

  useEffect(() => {
    if (!props.attached || !props.focusRequest || !terminalRef.current) {
      if (props.attached && props.focusRequest) {
        pendingFocusRequestRef.current = props.focusRequest;
      }
      return undefined;
    }

    // 新建 session 后要直接进入可输入状态；等一帧可以确保终端已完成 open/fit，
    // focusin 事件随后会由聚焦客户端上报真实 PTY 尺寸。
    pendingFocusRequestRef.current = props.focusRequest;
    const frame = scheduleDeferredTerminalFrame(() => {
      applyTerminalFocusRequest(props.focusRequest);
    });
    return () => cancelDeferredTerminalFrame(frame);
  }, [props.attached, props.focusRequest]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    applyFontSize(terminal, props.mobileInputMode ? MOBILE_TERMINAL_FONT_SIZE : TERMINAL_FONT_SIZE);
    stabilizeRef.current?.(hasActiveTerminalFocus() ? "focus" : "layout");
  }, [props.mobileInputMode]);

  const activeSearchMatch = searchResult?.matches[activeSearchIndex];

  return (
    <section
      className="terminal-pane"
      data-testid="terminal-pane"
    >
      <div className="terminal-scrollport" ref={scrollportRef}>
        <div className="terminal-canvas" ref={canvasRef}>
          <div
            className="terminal-frame"
            ref={frameRef}
            onMouseDownCapture={handleTerminalMouseDownCapture}
            onClickCapture={focusTerminalFromTerminalClick}
            onPointerDownCapture={handleMobileTerminalScrollPointerDown}
            onPointerMoveCapture={handleMobileTerminalScrollPointerMove}
            onPointerUpCapture={handleMobileTerminalScrollPointerEnd}
            onPointerCancelCapture={handleMobileTerminalScrollPointerEnd}
            onPointerDown={handleMobileDirectionPointerDown}
            onPointerMove={handleMobileDirectionPointerMove}
            onPointerUp={handleMobileDirectionPointerEnd}
            onPointerCancel={handleMobileDirectionPointerEnd}
            onContextMenuCapture={handleTerminalContextMenu}
          >
            <div
              className="terminal-host"
              ref={hostRef}
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
                  onChange={(event) => {
                    searchRequestSeqRef.current += 1;
                    setSearchLoading(false);
                    setSearchError(undefined);
                    setSearchDraft(event.currentTarget.value);
                  }}
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
                  searchRequestSeqRef.current += 1;
                  searchAddonRef.current?.clearDecorations();
                  setSearchLoading(false);
                  setSearchError(undefined);
                  setSearchResult(undefined);
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
          {searchOpen && searchResult?.matches.length ? (
            <div className="terminal-search-highlight" data-testid="terminal-search-highlight">
              {activeSearchMatch?.line_text ?? searchResult.query}
            </div>
          ) : null}
        </div>
      ) : null}
      {props.attached && props.mobileInputMode && terminalSelectionAvailable ? (
        <div
          className={`terminal-mobile-selection-toolbar${props.mobileKeyboardOpen ? " keyboard-open" : ""}`}
          onPointerDown={(event) => {
            // 中文注释：复制选区不是输入激活动作；阻止 pointerdown 把焦点推进隐藏 textarea。
            event.preventDefault();
            event.stopPropagation();
          }}
          onClick={(event) => event.stopPropagation()}
        >
          <button
            type="button"
            className="terminal-mobile-selection-copy-button"
            aria-label={t("terminal.copySelection")}
            title={t("terminal.copySelection")}
            onClick={(event) => {
              event.preventDefault();
              event.stopPropagation();
              copyVisibleTerminalSelection();
            }}
          >
            <Copy size={14} aria-hidden="true" />
            <span>{t("terminal.copySelection")}</span>
          </button>
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
      {!props.attached ? <div className="terminal-placeholder">{t("status.detached")}</div> : null}
    </section>
  );
}

function clampNumber(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function nowForThrottle(): number {
  return typeof performance === "undefined" ? Date.now() : performance.now();
}
