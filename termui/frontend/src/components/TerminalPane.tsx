import { useEffect, useLayoutEffect, useRef, useState, type FormEvent, type MouseEvent, type PointerEvent as ReactPointerEvent } from "react";
import { ChevronDown, ChevronUp, ClipboardPaste, Search, X } from "lucide-react";
import type { BrowserMobileShortcut, EffectiveTheme, SessionCursorPresence, SessionSearchResultPayload, TerminalSize } from "../protocol/types";
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
const MOBILE_DIRECTION_HOLD_MS = 1000;
const MOBILE_DIRECTION_DEAD_ZONE_PX = 24;
const MOBILE_DIRECTION_STEP_PX = 38;
const MOBILE_DIRECTION_REPEAT_MS = 500;
const MOBILE_DIRECTION_TIER_TWO_PX = 56;
const MOBILE_DIRECTION_TIER_THREE_PX = 84;
const MOBILE_DIRECTION_CANCEL_PX = 10;
const MOBILE_COMPOSITION_SETTLE_MS = 80;
const TERMINAL_BOTTOM_EPSILON = 1;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_MIN_BYTES = 8 * 1024;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_USER_MIN_BYTES = 1024;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_COOLDOWN_MS = 5_000;
const TERMINAL_SERVER_SCROLLBACK_RESYNC_IDLE_SETTLE_MS = 1_000;
const TERMINAL_SELECTION_DRAG_THRESHOLD_PX = 4;
const GHOSTTY_SCROLLBAR_GUTTER_PX = 12;
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
  layout: 2,
  focus: 3,
  "mobile-viewport": 3,
};
type MobileDirection = "up" | "down" | "left" | "right";
type MobileDirectionTier = 1 | 2 | 3;

interface DeferredTerminalFrameHandle {
  id: number;
  cancel: () => void;
  rescueHidden: (force?: boolean) => void;
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
  onCursorChange?: (presence: SessionCursorPresence) => void;
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
  const onCursorChangeRef = useRef(props.onCursorChange);
  const onTerminalResyncRef = useRef(props.onTerminalResync);
  const onTerminalSeqRenderedRef = useRef(props.onTerminalSeqRendered);
  const onTerminalSizeRenderedRef = useRef(props.onTerminalSizeRendered);
  const onOutputResetAppliedRef = useRef(props.onOutputResetApplied);
  const sessionSizeRef = useRef(props.sessionSize);
  const confirmedSessionSizeRef = useRef(props.sessionSize);
  const mobileInputModeRef = useRef(Boolean(props.mobileInputMode));
  const mobileKeyboardOpenRef = useRef(Boolean(props.mobileKeyboardOpen));
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
  const lastNativePasteRef = useRef<{ text: string; atMs: number } | undefined>(undefined);
  const mobileCompositionActiveRef = useRef(false);
  const lastMobileCompositionEndAtRef = useRef(0);
  const currentFontSizeRef = useRef(TERMINAL_FONT_SIZE);
  const bottomScrollPassesRef = useRef(0);
  const bottomScrollGenerationRef = useRef(0);
  const bottomScrollProgrammaticRef = useRef(false);
  const terminalSelectionCopyRef = useRef<{ text: string; atMs: number } | undefined>(undefined);
  const terminalSelectionCopyGenerationRef = useRef(0);
  const terminalNativeSelectionCopySuppressUntilRef = useRef(0);
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
    mobileViewportResizeOwnerRef,
    focusActivationArmedRef,
    suppressPassiveFocusRef,
    windowActiveRef,
    focusOutTimerRef,
    clearPendingFocusOut,
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
    if (terminalRenderedOutputBytesSinceSnapshotRef.current > 0) {
      recordTermdDiagnostic("terminal_snapshot_history_repair_abandoned", {
        snapshotRows: pendingRepair.snapshotRows,
        snapshotCols: pendingRepair.snapshotCols,
        sessionRows: sessionSize.rows,
        sessionCols: sessionSize.cols,
        repairAgeMs,
        renderedBytesSinceSnapshot: terminalRenderedOutputBytesSinceSnapshotRef.current,
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
    // 中文注释：用户上滚触发的 reveal-history 只属于当前 Ghostty buffer。
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
      // resize / Ghostty renderer / 移动端 visual viewport 可能分多帧稳定。
      // attach 后的贴底只在首屏执行，多补几帧不会放大持续输出路径压力。
      bottomScrollFrameRef.current = scheduleDeferredTerminalFrame(runScrollPass);
    };
    bottomScrollFrameRef.current = scheduleDeferredTerminalFrame(runScrollPass);
  };
  const scheduleScrollToBottomIfPinned = (wasPinnedToBottom = isTerminalPinnedToBottom(), passes = 2) => {
    if (terminalRevealHistorySuppressBottomUntilRef.current > nowForThrottle()) {
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
      // 中文注释：ghostty-web / Chromium 的剪贴板权限和 user activation 时序并不总是稳定；
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
    const canvas = hostRef.current?.querySelector<HTMLCanvasElement>("canvas");
    if (!terminal || !canvas || terminal.cols <= 0 || terminal.rows <= 0) {
      return undefined;
    }
    const rect = canvas.getBoundingClientRect();
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
    return terminal.selectViewportRange(start, end);
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
      // 直接写入这份 payload，能绕过 Ghostty 隐藏 textarea 没有 DOM 选区的问题。
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
  const terminalShouldHandleClipboardEventTarget = (eventTarget: EventTarget | null) => {
    const host = hostRef.current;
    if (!host) {
      return false;
    }
    if (eventTarget instanceof Node && host.contains(eventTarget)) {
      return true;
    }
    // 中文注释：自定义拖拽选区结束后，浏览器焦点可能暂时落回 body/document。
    // 但一旦用户已经点到终端外，就不能再让旧的 Ghostty 选区劫持页面里的普通复制。
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
        terminalRef.current?.focus();
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
    const canvas = host?.querySelector("canvas");
    const canvasRect = canvas?.getBoundingClientRect();
    const snapshotBufferLineCount = (host?.dataset.termdBuffer ?? "").split("\n").length;
    updateTerminalSelectionDebug({
      selectionNativeMouseDownTerminal: terminal ? JSON.stringify({ cols: terminal.cols, rows: terminal.rows }) : undefined,
      selectionNativeMouseDownRect: canvasRect ? JSON.stringify({ width: canvasRect.width, height: canvasRect.height }) : undefined,
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
  const noteTerminalOutputRendered = (item: TerminalOutputItem) => {
    if (item.kind === "snapshot") {
      terminalRenderedOutputBytesSinceSnapshotRef.current = 0;
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
          // 刚 attach/reconnect/relay 恢复完成时，Ghostty 很容易短暂处于
          // “baseY=0 但画面还在追平”的中间态；这里等待一个 idle settle window，
          // 避免把暂态误判成 tmux 全屏重绘，从而过早触发 full snapshot 重连。
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
    // 中文注释：tmux attach client 是全屏 renderer，实时输出可能只重绘可见屏幕，
    // 不会让 Ghostty 自身生成 scrollback。此时主动拉一次 daemon/tmux snapshot，
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
    const terminal = terminalRef.current;
    const scrollState = rendererRef.current?.scrollState(terminal ?? undefined);
    if (!terminal || !scrollState) {
      return;
    }
    if (scrollState.baseY > 0) {
      const canvas = hostRef.current?.querySelector<HTMLCanvasElement>("canvas");
      const canvasHeight = canvas?.getBoundingClientRect().height ?? 0;
      const cellHeight = canvasHeight > 0 && terminal.rows > 0 ? canvasHeight / terminal.rows : 16;
      event.preventDefault();
      event.stopPropagation();
      let deltaLines = 0;
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
        deltaLines = Math.trunc(deltaLinesFloat);
        terminalWheelLineRemainderRef.current = deltaLinesFloat - deltaLines;
        terminalWheelRemainderModeRef.current = event.deltaMode;
      } else {
        terminalWheelLineRemainderRef.current = 0;
        terminalWheelRemainderModeRef.current = event.deltaMode;
        deltaLines = Math.trunc(event.deltaY * (event.deltaMode === 2 ? terminal.rows : 1));
      }
      if (deltaLines === 0) {
        return;
      }
      const nextViewportY = clampNumber(
        scrollState.viewportY + deltaLines,
        0,
        scrollState.baseY,
      );
      if (Math.abs(nextViewportY - scrollState.viewportY) < TERMINAL_BOTTOM_EPSILON) {
        return;
      }
      terminal.scrollToLine(nextViewportY);
      syncTerminalInputAnchor(terminal, "scroll");
      scheduleTerminalScrollPosition({ immediate: true });
      return;
    }
    if (event.deltaY < 0) {
      // 中文注释：tmux attach 的实时输出可能只是全屏 repaint，Ghostty 本地 baseY 仍为 0；
      // 用户向上滚就是明确的“我要历史”信号，此时主动拉一次 daemon/tmux snapshot。
      event.preventDefault();
      event.stopPropagation();
      maybeRequestServerScrollbackResync(scrollState.baseY, "user-scroll");
    }
  };
  useEffect(() => {
    attachedRef.current = props.attached;
    onInputRef.current = props.onInput;
    onResizeRef.current = props.onResize;
    onCursorChangeRef.current = props.onCursorChange;
    onTerminalResyncRef.current = props.onTerminalResync;
    onTerminalSeqRenderedRef.current = props.onTerminalSeqRendered;
    onTerminalSizeRenderedRef.current = props.onTerminalSizeRendered;
    onOutputResetAppliedRef.current = props.onOutputResetApplied;
    sessionSizeRef.current = props.sessionSize;
    confirmedSessionSizeRef.current = props.sessionSize;
    mobileInputModeRef.current = Boolean(props.mobileInputMode);
    mobileKeyboardOpenRef.current = Boolean(props.mobileKeyboardOpen);
  }, [props.attached, props.mobileInputMode, props.mobileKeyboardOpen, props.onCursorChange, props.onInput, props.onOutputResetApplied, props.onResize, props.onTerminalResync, props.onTerminalSeqRendered, props.onTerminalSizeRendered, props.sessionSize, props.takeOutput]);

  useEffect(() => props.registerOutputDrain(() => drainOutputRef.current()), [props.registerOutputDrain]);

  useEffect(() => {
    if (props.mobileInputMode) {
      return;
    }
    mobileViewportResizeOwnerRef.current = false;
  }, [props.mobileInputMode]);

  useLayoutEffect(() => {
    if (!props.mobileInputMode) {
      return;
    }
    // 移动端软键盘会改变 visual viewport；只看 keyboardOpen 布尔值不够，
    // 因为部分浏览器会让 innerHeight 跟着缩放，导致键盘开关前后布尔值都为 false。
    const wasPinnedToBottom = isTerminalPinnedToBottom();
    stabilizeRef.current?.(hasActiveTerminalFocus() ? "focus" : "mobile-viewport");
    scheduleScrollToBottomIfPinned(wasPinnedToBottom);
  }, [props.mobileInputMode, props.mobileKeyboardOpen, props.mobileViewportHeight, props.mobileViewportOffsetTop]);

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
  }, [props.sessionSize?.cols, props.sessionSize?.pixel_height, props.sessionSize?.pixel_width, props.sessionSize?.rows]);

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
      if (!terminal || !onCursorChangeRef.current) {
        return;
      }
      lastCursorReportAtRef.current = nowForThrottle();

      // Ghostty 内部 cursorX/cursorY 是 0-based；协议用 1-based，便于顶部状态条直接展示。
      // jsdom 测试环境不会完整实现 Ghostty buffer，缺失时用 1:1 兜底，不影响浏览器真实值。
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
    onInputRef.current(data);
    queueCursorReport({ immediate: true });
    if (mobileInputModeRef.current) {
      terminalRef.current?.focus();
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
    // daemon 搜索负责跨 snapshot 的结果数量和目标行；Ghostty canvas 暂不暴露文本
    // decoration API，因此可见反馈由 React 层的搜索结果浮层承担。renderer search hook
    // 只保留为可选扩展点，当前 Ghostty adapter 是 no-op。
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
          clearMobileDirectionGesture();
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
      terminalRef.current?.focus();
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

  const handleTerminalContextMenu = () => {
    clearMobileDirectionGesture();
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
    // 就把 canvas 露出来。若过程中又来了一次 focus/layout/mobile 变化，就继续延长遮罩。
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
    // 中文注释：snapshot 刚写完时 Ghostty 还会补一到两帧 repaint/fit；等稳定帧后再露出 canvas，
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

  const hasActiveTerminalFocus = () => focusedRef.current && windowActiveRef.current;
  const terminalDomHasActiveFocus = () => {
    const terminalHost = hostRef.current;
    const activeElement = document.activeElement;
    return Boolean(
      windowActiveRef.current &&
      terminalHost &&
      activeElement instanceof HTMLElement &&
      terminalHost.contains(activeElement),
    );
  };
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
      // 收起移动端软键盘时 textarea 可能先 blur，visualViewport 稍后才恢复。
      // 只要窗口仍活跃，最后显式聚焦过终端的客户端仍负责把 PTY 尺寸恢复到当前可视高度。
      mobileViewportResizeOwnerRef.current = true;
    }
    if (!nextFocused) {
      cancelLocalResizeReport();
      suppressPassiveFocusRef.current = true;
    }
    queueCursorReport({ immediate: true });
  };

  const focusTerminalFromTerminalClick = (event: MouseEvent<HTMLDivElement>) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!isTerminalActivationTarget(target)) {
      return;
    }
    if (target?.closest("canvas")) {
      // 中文注释：Ghostty 自己会在 canvas mousedown 时把 host 聚焦；
      // canvas 上的 click 不再额外抢焦点，避免刚完成的鼠标选区被这层逻辑清掉。
      return;
    }
    const wasPinnedToBottom = isTerminalPinnedToBottom();
    windowActiveRef.current = true;
    // 点击终端 frame 是用户显式接管终端的动作；有些浏览器和 jsdom mock
    // 不会把外层 frame 点击稳定转成内部 textarea 的 focusin，因此这里先同步本地聚焦态。
    focusActivationArmedRef.current = false;
    suppressPassiveFocusRef.current = false;
    reportTerminalFocus(true);
    terminalRef.current?.focus();
    resizeRef.current?.("focus");
    // 当前客户端接管 PTY 尺寸时，只在用户本来就在底部时继续贴底。
    // 用户已经上滚查看历史时，点击空白处应该只聚焦终端，不能强行跳到最新输出。
    scheduleScrollToBottomIfPinned(wasPinnedToBottom);
  };

  const applyTerminalFocusRequest = (requestId?: number) => {
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
      if (requestId !== undefined && pendingFocusRequestRef.current === requestId) {
        pendingFocusRequestRef.current = undefined;
      }
      return;
    }
    focusActivationArmedRef.current = true;
    suppressPassiveFocusRef.current = false;
    terminalRef.current?.focus();
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
          const canvas = host?.querySelector("canvas");
          const canvasRect = canvas?.getBoundingClientRect();
          updateTerminalSelectionDebug({
            selectionNativeMouseDownTarget: target?.tagName.toLowerCase() ?? undefined,
          });
          if (!target?.closest("canvas")) {
            terminalSelectionCopyGenerationRef.current += 1;
            clearTerminalSelectionDrag();
            updateTerminalSelectionDebug({
              selectionNativeMouseDownStarted: "false",
            });
            return;
          }
          if (
            canvasRect &&
            Number.parseFloat(host.dataset.termdScrollbackLength ?? "0") > 0 &&
            event.clientX >= canvasRect.right - GHOSTTY_SCROLLBAR_GUTTER_PX
          ) {
            updateTerminalSelectionDebug({
              selectionNativeMouseDownTarget: target?.tagName.toLowerCase() ?? undefined,
              selectionNativeMouseDownScrollbarGutter: "true",
              selectionNativeMouseDownStarted: "false",
            });
            event.preventDefault();
            event.stopPropagation();
            event.stopImmediatePropagation();
            return;
          }
          windowActiveRef.current = true;
          focusActivationArmedRef.current = true;
          suppressPassiveFocusRef.current = false;
          markTerminalClipboardSelectionOwner();
          const started = startTerminalSelectionDrag(event.clientX, event.clientY);
          updateTerminalSelectionDebug({
            selectionNativeMouseDownStarted: String(started),
          });
          if (!started) {
            terminal.focus();
            stabilizeRef.current?.("focus");
            return;
          }
          // 中文注释：Ghostty 自己也会在 canvas 上挂鼠标监听；这里必须用原生 capture
          // 抢在它前面拦截，避免 native selection manager 把拖拽和复制逻辑带偏。
          event.preventDefault();
          event.stopPropagation();
          event.stopImmediatePropagation();
          terminalSelectionFocusPendingRef.current = true;
        };
        host.addEventListener("mousedown", handleMouseDown, true);
        return () => {
          host.removeEventListener("mousedown", handleMouseDown, true);
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
      // 这个短暂窗口被吞掉，否则 tmux 会继续维持旧 rows/cols，首屏就会按旧网格乱掉。
      return windowActiveRef.current && focusActivationArmedRef.current;
    };
    const canReportLocalResizeForSource = (source: ResizeSource | undefined) => {
      if (!source || source === "session" || source === "snapshot" || snapshotRedrawInProgressRef.current) {
        return false;
      }
      if (mobileInputModeRef.current && mobileKeyboardOpenRef.current) {
        return false;
      }
      if (source === "mobile-viewport") {
        return mobileInputModeRef.current && windowActiveRef.current && mobileViewportResizeOwnerRef.current;
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
      onInputRef.current(data);
    });
    const helperTextarea = renderer.getInputElement(host);
    if (helperTextarea) {
      // 中文注释：真实 Ghostty 会同时让 host 和隐藏 textarea 具备输入能力；
      // 对用户和 Playwright role locator 来说，外层 host 才是唯一可见终端输入框。
      helperTextarea.setAttribute("aria-hidden", "true");
    }
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
      if (!mobileInputModeRef.current || event.defaultPrevented || isMobileCompositionInput(event)) {
        return;
      }

      const text =
        event.inputType === "insertFromPaste" && event.data
          ? event.data
          : event.inputType === "insertText" && event.data
            ? event.data
            : undefined;
      if (!text) {
        return;
      }

      // iOS/Safari 软键盘有时只给 beforeinput，不走 Ghostty 的 keydown/keypress。
      // 对非组合文本和粘贴文本做兜底，并阻止后续 input，避免同一份内容发送两次。
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
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
      event.stopImmediatePropagation();
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
    document.addEventListener("copy", handleTerminalCopyEvent, true);
    document.addEventListener("mousedown", handleTerminalClipboardContextMouseDown, true);
    document.addEventListener("focusin", handleTerminalClipboardContextFocusIn, true);
    const selectionSubscription = terminal.onSelectionChange(() => {
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
      // Ghostty 原生选择完成后同步复制到系统剪贴板；复制失败时不打断终端交互。
      copyCurrentTerminalSelection({ selectionOverride: selection });
    });
    // 本地 Ghostty 只有在当前浏览器窗口聚焦终端时才把尺寸写回 shared PTY。
    // 未聚焦客户端按 daemon 确认的 session rows/cols 渲染，不再做本地等比缩放。
    const resize = (source: ResizeSource = "layout") => {
      if (snapshotRedrawInProgressRef.current) {
        if (
          source === "focus" ||
          source === "mobile-viewport" ||
          (source === "layout" && hasActiveTerminalFocus())
        ) {
          // 中文注释：snapshot 字节写入期间不能改变 Ghostty 尺寸；但用户主动聚焦/窗口变化
          // 不能丢，等 snapshot 渲染完成后再补一次真实 resize 上报。
          pendingResizeAfterSnapshotRef.current = source;
        }
        // 中文注释：snapshot 字节按生成时的列宽解释；写入完成前禁止 layout/session resize
        // 把 Ghostty 改回旧尺寸，否则宽行换行和光标位置会被错误解析。
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
      const armedFocusResizeAuthority = canUseArmedFocusResizeAuthority(source);
      const terminalCanReportActiveFocus =
        terminalHasActiveFocus ||
        (windowActiveRef.current && terminalDomHasActiveFocus()) ||
        armedFocusResizeAuthority;
      const mobileKeyboardIsOpen =
        mobileInputModeRef.current &&
        mobileKeyboardOpenRef.current;
      const hasMobileViewportResizeOwnership =
        source === "mobile-viewport" &&
        mobileInputModeRef.current &&
        windowActiveRef.current &&
        mobileViewportResizeOwnerRef.current;
      const canReportLocalResize =
        source !== "session" &&
        source !== "snapshot" &&
        !mobileKeyboardIsOpen &&
        (terminalCanReportActiveFocus || hasMobileViewportResizeOwnership);
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
          // 中文注释：snapshot 后仍要把本地 Ghostty 贴合当前容器，避免内容停在旧高度；
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
          // 中文注释：未聚焦客户端虽然不能把自己的布局写回 shared PTY，但 daemon/tmux
          // 已确认的新 rows/cols 仍然是权威尺寸。这里必须被动跟随远端 grid，否则后续
          // output/snapshot 仍会按旧列宽解释，tmux/vim/top 这类全屏界面会直接错位。
          terminal.resize(remoteSize.cols, remoteSize.rows);
        }
        // 中文注释：窗口失焦后的 layout/blur 抖动不能把本地 Ghostty 立即缩回远端尺寸。
        // 只有 daemon/tmux 真的确认了新的 sessionSize，未聚焦客户端才跟随权威 grid；
        // 否则回到页面时会先看到一次旧 grid/旧布局，再被 focus resize 拉回当前容器。
        scheduleScrollToBottomIfPinned(wasPinnedToBottom);
        queueCursorReport({ immediate: true });
        return;
      }
      applyFontSize(terminal, currentTerminalFontSize());
      // 移动端软键盘或外层 grid 短暂重排时可能把 Ghostty 容器压到 0 高。
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
        // 同一条稳定帧通道里，避免 reload/focus 期间把临时测量先写进 Ghostty。
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
      // Ghostty 在 CSS grid / 右侧文件 panel 同步变化时可能先按旧尺寸完成 open/write。
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
        const shouldRepairSnapshotHistory =
          !revealHistory &&
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
          // 中文注释：reload/重连时 daemon snapshot 可能仍是旧的 80x24。若当前客户端
          // 已经重新聚焦终端，snapshot 重放完成后必须按 focus 路径把真实浏览器尺寸
          // 写回 daemon/tmux；仅本地 fit 会让下一次 attach 继续拿到旧分辨率。
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
      reportTerminalFocus,
      queueCursorReport,
      scheduleScrollToBottomIfPinned,
      resize,
    });
    terminalRef.current = terminal;
    fitRef.current = fit;
    searchAddonRef.current = searchAddon;
    rendererRef.current = renderer;
    outputResetVersionRef.current = props.outputResetVersion;
    const confirmOutputReset = () => onOutputResetAppliedRef.current?.(props.outputResetVersion);
    // 测试桩可以延迟 reset 确认，用来覆盖“新 snapshot 必须等 Ghostty reset 完成后才能消费”的竞态。
    const deferOutputResetApplied = (globalThis as {
      __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void;
    }).__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__;
    if (deferOutputResetApplied) {
      deferOutputResetApplied(confirmOutputReset);
    } else {
      confirmOutputReset();
    }
    markWriterNeedsRefreshAndScroll();
    // attach 输出可能早于 Ghostty 初始化到达；创建实例时先取走待写队列，避免首屏输出丢失。
    drainOutputRef.current = drainOutput;
    drainOutput();
    queueCursorReport({ immediate: true });
    scheduleTerminalScrollPosition({ immediate: true });

    // 初次 attach 只做本地 fit；用户聚焦该终端时才接管 shared PTY 的远端尺寸。
    stabilizeTerminal();
    if (terminalDomHasActiveFocus()) {
      // 中文注释：reload 后用户或测试可能在 Ghostty canvas mount 前已经把焦点放到 host；
      // renderer 就绪后必须补一次 focus resize，否则 daemon/tmux 会继续停在旧 snapshot 尺寸。
      reportTerminalFocus(true);
      stabilizeTerminal("focus");
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
        outputResetVersion: props.outputResetVersion,
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
      clearMobileDirectionGesture();
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
      terminalFrame?.removeEventListener("wheel", handleTerminalWheel, true);
      document.removeEventListener("keydown", handleTerminalCopyShortcut, true);
      document.removeEventListener("copy", handleTerminalCopyEvent, true);
      document.removeEventListener("mousedown", handleTerminalClipboardContextMouseDown, true);
      document.removeEventListener("focusin", handleTerminalClipboardContextFocusIn, true);
      clearTerminalSelectionDrag();
      dataSubscription.dispose();
      cursorMoveSubscription.dispose();
      writeParsedSubscription.dispose();
      scrollSubscription.dispose();
      selectionSubscription.dispose();
      cleanupTerminalSelectionNativeListeners?.();
      cleanupTerminalSelectionNativeListeners = undefined;
      terminal.dispose();
      // 清理 host 里的旧 Ghostty DOM，避免切换 session 后旧终端明文或隐藏 textarea 残留。
      host.replaceChildren();
      // 中文注释：真实 ghostty-web dispose 不会清理应用层 debug dataset；
      // 即使该镜像只在 dev/test/E2E 构建启用，detach/reset 后也不能残留旧终端明文。
      delete host.dataset.termdBuffer;
      delete host.dataset.termdCols;
      delete host.dataset.termdRows;
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
      terminalSelectionCopyGenerationRef.current += 1;
      pendingFocusRequestRef.current = undefined;
      drainOutputRef.current = () => undefined;
      resetWriterState();
      forcedCursorBottomModeRef.current = false;
      focusedRef.current = false;
      mobileCompositionActiveRef.current = false;
      lastMobileCompositionEndAtRef.current = 0;
      clientSizeRef.current = undefined;
      mobileViewportResizeOwnerRef.current = false;
      focusActivationArmedRef.current = false;
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
        cursorBlink: true,
        cursorStyle: "block",
        cursorInactiveStyle: "outline",
        // MVP 只需要普通终端渲染；屏幕阅读模式会额外维持可访问性树，增加高输出场景的内存和 CPU 压力。
        screenReaderMode: false,
        scrollback: 2000,
        smoothScrollDuration: 0,
        // 中文注释：Ghostty 会把中文等宽字符按 2 个 terminal cells 排版。
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
  }, [props.attached, props.outputResetVersion]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    // 中文注释：ghostty-web 运行期换 theme 不会重写 WASM buffer 的颜色。
    // App 会在 effective theme 变化时触发 full snapshot resync，并通过 outputResetVersion
    // 重建 Ghostty；这里不再调用 setOptions，避免短暂显示第二套错误颜色和 upstream 警告。
    if (rendererRef.current?.kind === "ghostty") {
      return;
    }
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
    // session 切换时上面的 terminal effect 会按 outputResetVersion 重建 Ghostty 实例。
    // 这里仅保留防御式同步清屏：如果未来 effect 条件调整导致实例未重建，也不能残留旧 session 明文。
    recordTermdDiagnostic("terminal_pane_defensive_reset", {
      previousResetVersion: outputResetVersionRef.current,
      outputResetVersion: props.outputResetVersion,
    });
    invalidateBottomScrollFollow();
    terminal.reset();
  }, [props.outputResetVersion]);

  useEffect(() => {
    if (!props.attached || !props.focusRequest || !terminalRef.current) {
      if (props.attached && props.focusRequest) {
        pendingFocusRequestRef.current = props.focusRequest;
      }
      return undefined;
    }

    // 新建 session 后要直接进入可输入状态；等一帧可以确保 Ghostty 已完成 open/fit，
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
            onClickCapture={focusTerminalFromTerminalClick}
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
