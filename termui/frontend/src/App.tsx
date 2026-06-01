import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from "react";
import {
  Cable,
  CircleAlert,
  Folder,
  MonitorUp,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  PanelRightOpen,
  Plus,
  RefreshCcw,
  Server,
  Settings,
  UsersRound,
  X,
} from "lucide-react";
import { DirectClient, ProtocolClientError } from "./protocol/direct-client";
import { toSafeError } from "./protocol/errors";
import { parsePairingQrPayload } from "./protocol/pairing-payload";
import type {
  BrowserState,
  DaemonClientSummaryPayload,
  DaemonStatusResultPayload,
  PairedServerState,
  SafeError,
  SessionCreatedPayload,
  SessionCursorPresence,
  SessionActivityPayload,
  SessionAttachedPayload,
  SessionDataPayload,
  SessionFileEntryPayload,
  SessionFilesResultPayload,
  SessionGitActionKind,
  SessionGitFileChangePayload,
  SessionGitDiffResultPayload,
  SessionGitResultPayload,
  SessionGitWorktreePayload,
  SessionResizedPayload,
  SessionSearchResultPayload,
  SessionSummaryPayload,
  RenderableTerminalFramePayload,
  TerminalSize,
  UUID,
} from "./protocol/types";
import { sessionDataFromBase64 } from "./protocol/wire";
import {
  defaultServer,
  DEFAULT_BROWSER_PREFERENCES,
  ensureDevice,
  loadBrowserState,
  normalizeRouteWsUrl,
  forgetDaemon,
  recordPairing,
  recordServerUrl,
  renameDaemon,
  saveBrowserPreferences,
  selectDefaultServer,
} from "./state/browser-state";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { DaemonClientsPanel } from "./components/DaemonClientsPanel";
import { DaemonManagerPanel } from "./components/DaemonManagerPanel";
import { SessionList } from "./components/SessionList";
import { SessionFilesPanel } from "./components/SessionFilesPanel";
import { FileEditorDialog } from "./components/FileEditorDialog";
import { StatusBar } from "./components/StatusBar";
import { TerminalPane, type TerminalOutputItem } from "./components/TerminalPane";
import { PairingQrScanner } from "./components/PairingQrScanner";
import { SettingsDialog } from "./components/SettingsDialog";
import { sessionDisplayName } from "./session-names";
import { createTranslator, I18nProvider, resolveLocale, translateSafeErrorMessage, useI18n, type Translate } from "./i18n";
import { resolveTheme } from "./theme";
import type { BrowserPreferences } from "./protocol/types";

const FALLBACK_WS_URL = "ws://127.0.0.1:8765/ws";
const DEFAULT_SESSION_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
const DEFAULT_FILES_PANEL_WIDTH = 286;
const MIN_FILES_PANEL_WIDTH = 240;
const MAX_FILES_PANEL_WIDTH = 640;
const CONNECTION_AUTO_RETRY_DELAY_MS = 1500;
const CONNECTION_AUTO_RETRY_LIMIT = 3;
const ATTACH_RECONNECT_DELAYS_MS = [250, 1000, 2500, 5000, 10000, 20000];
const ATTACH_SWITCH_COALESCE_DELAY_MS = 80;
const FILES_CWD_FOLLOW_POLL_INTERVAL_MS = 1000;
const TEXT_FILE_EDITOR_MAX_BYTES = 1024 * 1024;
const FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES = 16 * 1024 * 1024;
const RECEIVE_LOOP_YIELD_MESSAGES = 64;
const RECEIVE_LOOP_YIELD_BYTES = 256 * 1024;
const MOBILE_LAYOUT_QUERY = "(max-width: 760px)";
const MOBILE_LAYOUT_BREAKPOINT = 760;
const MOBILE_TITLE_PULL_START_PX = 8;
const MOBILE_TITLE_PULL_REFRESH_PX = 52;
const MOBILE_TITLE_PULL_MAX_PX = 72;
const CPU_HISTORY_LIMIT = 48;
const CPU_BAR_CHART_WIDTH = 56;
const CPU_BAR_CHART_HEIGHT = 18;
const CPU_BAR_CHART_COUNT = 18;
export const DAEMON_STATUS_POLL_INTERVAL_MS = 1000;
// 普通前端操作走同一条可靠 WebSocket；relay 下终端输出可能排在 RPC 响应前面。
// 5 秒给公网 relay 和浏览器调度留出缓冲，避免把短暂排队误报成“操作超时”。
export const APP_CONNECTION_TIMEOUT_MS = 5000;
// WebSocket 新建连接偶发卡住时，不能把整个 session attach 挂到 15 秒。
// terminal snapshot 仍有自己的 attach timeout；这里只约束 route/E2EE 建连阶段。
const APP_SOCKET_CONNECT_TIMEOUT_MS = 3000;
// 中文注释：真实故障里慢点出现在 socket open 的 TCP/TLS/WebSocket 阶段。
// 该阶段单独快速失败并重试，避免一次半卡住的 TLS 握手拖慢整个 relay attach。
const APP_SOCKET_OPEN_TIMEOUT_MS = 1200;
const APP_SOCKET_OPEN_HEDGE_DELAY_MS = 300;
const APP_SOCKET_CONNECT_ATTEMPTS = 4;
const APP_SOCKET_CONNECT_RETRY_DELAY_MS = 80;
const PAIRING_CONNECTION_TIMEOUT_MS = 5000;
const ATTACH_CONNECTION_TIMEOUT_MS = 15000;
type AppSurface = "admin" | "workspace";

interface FileTransferProgressState {
  sessionId: UUID;
  transferId: number;
  name: string;
  offsetBytes: number;
  sizeBytes: number;
  phase?: "sending" | "committing" | "confirmed";
  completed?: boolean;
}

interface AttachUiOptions {
  closeMobilePanel?: boolean;
}

interface AttachReconnectOptions {
  lastTerminalSeq?: number;
  sessionId?: UUID;
  reconnectKey?: string;
  skipCurrentClientClose?: boolean;
}

interface MobileTitlePullGesture {
  pointerId: number;
  startX: number;
  startY: number;
  dragging: boolean;
}

const RETRYABLE_CONNECTION_ERROR_CODES = new Set([
  "connection_closed",
  "connection_error",
  "connect_timeout",
  "route_prelude_timeout",
  "relay_daemon_offline",
  "relay_state_unavailable",
  "handshake_timeout",
  "terminal_resync",
]);

function isRetryableConnectionError(caught: unknown): boolean {
  return RETRYABLE_CONNECTION_ERROR_CODES.has(toSafeError(caught).code);
}

const BROKEN_WORKSPACE_CONNECTION_ERROR_CODES = new Set([
  "connection_closed",
  "connection_error",
  "connect_timeout",
  "route_prelude_timeout",
  "relay_daemon_offline",
  "relay_state_unavailable",
  "handshake_timeout",
]);

function isBrokenWorkspaceConnectionError(caught: unknown): boolean {
  return BROKEN_WORKSPACE_CONNECTION_ERROR_CODES.has(toSafeError(caught).code);
}

const LOCALLY_SUPERSEDED_CONNECTION_ERROR_CODES = new Set([
  "connection_closed",
  "stale_connection",
]);

function isLocallySupersededConnectionError(caught: unknown): boolean {
  return LOCALLY_SUPERSEDED_CONNECTION_ERROR_CODES.has(toSafeError(caught).code);
}

function isBackgroundStatusTransientError(caught: unknown): boolean {
  const code = toSafeError(caught).code;
  return code === "response_timeout" || isLocallySupersededConnectionError(caught);
}

function isDocumentHidden(): boolean {
  return typeof document !== "undefined" && document.visibilityState === "hidden";
}

function isBrowserOffline(): boolean {
  return typeof navigator !== "undefined" && navigator.onLine === false;
}

function isPagePaused(): boolean {
  return isDocumentHidden() || isBrowserOffline();
}

function isTerminalTransportPaused(): boolean {
  return isBrowserOffline();
}

function terminalTransportPausedError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection paused while browser is offline");
}

function createTransportAbortController(): { controller: AbortController; dispose: () => void } | undefined {
  if (typeof window === "undefined") {
    return undefined;
  }
  const controller = new AbortController();
  const abortWhenOffline = () => {
    if (isTerminalTransportPaused()) {
      controller.abort();
    }
  };
  window.addEventListener("offline", abortWhenOffline);
  abortWhenOffline();
  return {
    controller,
    dispose: () => {
      window.removeEventListener("offline", abortWhenOffline);
    },
  };
}

function createLinkedAbortController(
  ...signals: Array<AbortSignal | undefined>
): { controller: AbortController; dispose: () => void } | undefined {
  const activeSignals = signals.filter((signal): signal is AbortSignal => Boolean(signal));
  if (activeSignals.length === 0) {
    return undefined;
  }
  const controller = new AbortController();
  const abortLinked = () => controller.abort();
  for (const signal of activeSignals) {
    if (signal.aborted) {
      controller.abort();
      continue;
    }
    signal.addEventListener("abort", abortLinked, { once: true });
  }
  return {
    controller,
    dispose: () => {
      for (const signal of activeSignals) {
        signal.removeEventListener("abort", abortLinked);
      }
    },
  };
}

function connectionAbortedError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection closed");
}

function throwIfConnectionAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw connectionAbortedError();
  }
}

function abortableConnectionStep<T>(promise: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) {
    return promise;
  }
  throwIfConnectionAborted(signal);
  return new Promise((resolve, reject) => {
    const abort = () => reject(connectionAbortedError());
    signal.addEventListener("abort", abort, { once: true });
    promise.then(
      (value) => {
        signal.removeEventListener("abort", abort);
        resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", abort);
        reject(error);
      },
    );
  });
}

function waitForConnectionRetryDelay(signal?: AbortSignal): Promise<void> {
  return abortableConnectionStep(
    new Promise((resolve) => {
      globalThis.setTimeout(resolve, APP_SOCKET_CONNECT_RETRY_DELAY_MS);
    }),
    signal,
  );
}

export interface DaemonNetworkCounterSample {
  rxBytes: number;
  txBytes: number;
  sampledAtMs: number;
}

export interface DaemonNetworkRate {
  rxBytesPerSecond: number;
  txBytesPerSecond: number;
}

export default function App() {
  const [state, setState] = useState<BrowserState>({ pairedServers: [] });
  const [url, setUrl] = useState(() => defaultWsUrlFromPage());
  const [pairingToken, setPairingToken] = useState("");
  const [sessions, setSessions] = useState<SessionSummaryPayload[]>([]);
  const [sessionOrder, setSessionOrder] = useState<UUID[]>([]);
  const sessionOrderRef = useRef<UUID[]>([]);
  const sessionOrderGenerationRef = useRef(0);
  const pendingSessionReorderRef = useRef(false);
  const [newOutputSessionIds, setNewOutputSessionIds] = useState<Set<UUID>>(() => new Set());
  const [daemonClients, setDaemonClients] = useState<DaemonClientSummaryPayload[]>([]);
  const [forgettingClientIds, setForgettingClientIds] = useState<Set<UUID>>(() => new Set());
  const [clientsOpen, setClientsOpen] = useState(false);
  const [daemonStatus, setDaemonStatus] = useState<DaemonStatusResultPayload | undefined>();
  const [daemonCpuHistory, setDaemonCpuHistory] = useState<number[]>([]);
  const [daemonNetworkRate, setDaemonNetworkRate] = useState<DaemonNetworkRate | undefined>();
  const [daemonNetworkLatencyMs, setDaemonNetworkLatencyMs] = useState<number | undefined>();
  const [daemonStatusLoading, setDaemonStatusLoading] = useState(false);
  const [daemonStatusError, setDaemonStatusError] = useState<SafeError | undefined>();
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [attachedSessionId, setAttachedSessionId] = useState<UUID | undefined>();
  const [renamingSessionId, setRenamingSessionId] = useState<UUID | undefined>();
  const [renameDraft, setRenameDraft] = useState("");
  const [renameOriginalName, setRenameOriginalName] = useState("");
  const [terminalOutputResetVersion, setTerminalOutputResetVersion] = useState(0);
  const [terminalFocusRequest, setTerminalFocusRequest] = useState(0);
  const [sessionFiles, setSessionFiles] = useState<SessionFilesResultPayload | undefined>();
  const [sessionFilesLoading, setSessionFilesLoading] = useState(false);
  const [sessionFilesError, setSessionFilesError] = useState<SafeError | undefined>();
  const [sessionFilesFollowTerminalCwd, setSessionFilesFollowTerminalCwd] = useState(true);
  const [sessionFileUploadProgress, setSessionFileUploadProgress] = useState<FileTransferProgressState | undefined>();
  const [sessionFileDownloadProgress, setSessionFileDownloadProgress] = useState<FileTransferProgressState | undefined>();
  const [sessionFilesPanelTab, setSessionFilesPanelTab] = useState<"files" | "git">("files");
  const [sessionGit, setSessionGit] = useState<SessionGitResultPayload | undefined>();
  const [sessionGitLoading, setSessionGitLoading] = useState(false);
  const [sessionGitError, setSessionGitError] = useState<SafeError | undefined>();
  const [fileEditor, setFileEditor] = useState<{
    path: string;
    name: string;
    text: string;
    loading: boolean;
    saving: boolean;
    error?: string;
  } | undefined>();
  const [diffViewer, setDiffViewer] = useState<{
    path: string;
    name: string;
    text: string;
    loading: boolean;
    error?: string;
  } | undefined>();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [filesPanelOpen, setFilesPanelOpen] = useState(true);
  const [filesPanelWidth, setFilesPanelWidth] = useState(DEFAULT_FILES_PANEL_WIDTH);
  const [isFilesPanelResizing, setIsFilesPanelResizing] = useState(false);
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);
  const [mobilePanel, setMobilePanel] = useState<"sessions" | "files" | undefined>();
  const [mobileTitlePullDistance, setMobileTitlePullDistance] = useState(0);
  const [connectionEditorOpen, setConnectionEditorOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [qrScannerOpen, setQrScannerOpen] = useState(false);
  const [renamingDaemonId, setRenamingDaemonId] = useState<UUID | undefined>();
  const [daemonRenameDraft, setDaemonRenameDraft] = useState("");
  const [activeSurface, setActiveSurface] = useState<AppSurface>("admin");
  const [status, setStatus] = useState("idle");
  const [error, setError] = useState<SafeError | undefined>();
  // 中文注释：当前打开的 session 只绑定一条可靠 WebSocket；terminal 与普通 RPC 都在
  // 这条连接的 E2EE 内层 ProtocolPacket segment 中分类。切换 session 或重连时必须
  // 关闭旧连接并重建，保证 relay/daemon 都能用 transport close 明确清理旧 client。
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const pendingAttachClientRef = useRef<DirectClient | undefined>(undefined);
  const workspaceClientPromiseRef = useRef<Promise<DirectClient> | undefined>(undefined);
  const workspaceClientAbortControllerRef = useRef<AbortController | undefined>(undefined);
  const workspaceClientGenerationRef = useRef(0);
  const mobileTitlePullGestureRef = useRef<MobileTitlePullGesture | undefined>(undefined);
  const suppressMobileTitleClickRef = useRef(false);
  const pendingTerminalAttachSessionRef = useRef<UUID | undefined>(undefined);
  const sessionPermissionIdsRef = useRef<Set<UUID>>(new Set());
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const autoAttachAttemptedSessionRef = useRef<UUID | undefined>(undefined);
  const attachingSessionIdRef = useRef<UUID | undefined>(undefined);
  const attachRequestIdRef = useRef(0);
  const sessionCreateRequestIdRef = useRef(0);
  const attachSwitchTimerRef = useRef<number | undefined>(undefined);
  const attachSwitchGenerationRef = useRef(0);
  const reattachCurrentSessionOnOpenRef = useRef(false);
  const userDetachedRef = useRef(false);
  const pendingResizeKeyRef = useRef<string | undefined>(undefined);
  const confirmedSessionSizesRef = useRef<Map<UUID, TerminalSize>>(new Map());
  const receiveLoopActiveRef = useRef(false);
  const receiveLoopGenerationRef = useRef(0);
  const closingSessionIdsRef = useRef<Set<UUID>>(new Set());
  const forgettingClientIdsRef = useRef<Set<UUID>>(new Set());
  const renamingSessionIdRef = useRef<UUID | undefined>(undefined);
  const filesPanelWidthRef = useRef(DEFAULT_FILES_PANEL_WIDTH);
  const sessionFilesFollowTerminalCwdRef = useRef(sessionFilesFollowTerminalCwd);
  const sessionFileUploadProgressClearTimeoutRef = useRef<number | undefined>(undefined);
  const sessionFileDownloadProgressClearTimeoutRef = useRef<number | undefined>(undefined);
  const fileTransferIdRef = useRef(0);
  const activeUploadTransferIdRef = useRef<number | undefined>(undefined);
  const activeDownloadTransferIdRef = useRef<number | undefined>(undefined);
  const sessionFilesRequestSeqRef = useRef(0);
  const sessionGitRequestSeqRef = useRef(0);
  const sessionFilesFollowRefreshInFlightRef = useRef(false);
  const filesPanelResizeRef = useRef<{
    pointerId: number;
    startX: number;
    startWidth: number;
  } | null>(null);
  const urlTouchedRef = useRef(false);
  const autoCheckedServerRef = useRef<UUID | undefined>(undefined);
  const lastCursorReportRef = useRef("");
  const lastCursorFocusedRef = useRef<boolean | undefined>(undefined);
  const cursorRefreshTimerRef = useRef<number | undefined>(undefined);
  const terminalOutputQueueRef = useRef<TerminalOutputItem[]>([]);
  const lastRenderedTerminalSeqRef = useRef<Map<UUID, number>>(new Map());
  const terminalOutputResetVersionRef = useRef(0);
  const terminalOutputAppliedResetVersionRef = useRef(0);
  const terminalOutputResetWaitersRef = useRef<Map<number, Set<() => void>>>(new Map());
  const terminalOutputFlushFrameRef = useRef<number | undefined>(undefined);
  const terminalOutputDrainRef = useRef<(() => void) | undefined>(undefined);
  const selectedSessionIdRef = useRef<UUID | undefined>(undefined);
  const activeSurfaceRef = useRef<AppSurface>(activeSurface);
  const statusRef = useRef(status);
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);
  const attachReconnectTimerRef = useRef<number | undefined>(undefined);
  const attachReconnectKeyRef = useRef<string | undefined>(undefined);
  const attachReconnectAttemptsRef = useRef(0);
  const attachReconnectLastErrorRef = useRef<unknown>(undefined);
  const attachReconnectHandlerRef = useRef<(client: DirectClient, caught: unknown, options?: AttachReconnectOptions) => boolean>(() => false);
  const daemonNetworkSampleRef = useRef<DaemonNetworkCounterSample | undefined>(undefined);
  const daemonStatusRefreshInFlightRef = useRef(false);
  const daemonStatusRequestSeqRef = useRef(0);
  const daemonClientsRefreshInFlightRef = useRef(false);
  const lastNotificationAtRef = useRef(0);
  const isMobileLayout = useMobileLayout();
  const visualViewportMetrics = useVisualViewportMetrics(isMobileLayout && activeSurface === "workspace");
  const systemTheme = useSystemTheme();
  const preferences = state.preferences ?? DEFAULT_BROWSER_PREFERENCES;
  const effectiveTheme = resolveTheme(preferences.theme, systemTheme);
  const effectiveLocale = resolveLocale(preferences.language);
  const t = useMemo(() => createTranslator(effectiveLocale), [effectiveLocale]);

  const closeWorkspaceClient = useCallback(() => {
    workspaceClientGenerationRef.current += 1;
    workspaceClientAbortControllerRef.current?.abort();
    workspaceClientAbortControllerRef.current = undefined;
    receiveLoopActiveRef.current = false;
    receiveLoopGenerationRef.current += 1;
    workspaceClientPromiseRef.current = undefined;
    const clients = new Set<DirectClient>();
    if (pendingAttachClientRef.current) {
      clients.add(pendingAttachClientRef.current);
    }
    if (attachClientRef.current) {
      clients.add(attachClientRef.current);
    }
    for (const client of clients) {
      client.interruptReceiveWaiters();
      client.close();
    }
    pendingAttachClientRef.current = undefined;
    attachClientRef.current = undefined;
    pendingTerminalAttachSessionRef.current = undefined;
    sessionPermissionIdsRef.current.clear();
  }, []);

  const selectSession = useCallback((sessionId: UUID | undefined) => {
    selectedSessionIdRef.current = sessionId;
    setSelectedSessionId(sessionId);
  }, []);

  useEffect(() => {
    activeSurfaceRef.current = activeSurface;
  }, [activeSurface]);

  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  useEffect(() => {
    sessionFilesFollowTerminalCwdRef.current = sessionFilesFollowTerminalCwd;
  }, [sessionFilesFollowTerminalCwd]);

  const nextFileTransferId = useCallback(() => {
    fileTransferIdRef.current += 1;
    return fileTransferIdRef.current;
  }, []);

  const clearSessionFileUploadProgressTimer = useCallback(() => {
    if (sessionFileUploadProgressClearTimeoutRef.current !== undefined) {
      window.clearTimeout(sessionFileUploadProgressClearTimeoutRef.current);
      sessionFileUploadProgressClearTimeoutRef.current = undefined;
    }
  }, []);

  const clearSessionFileDownloadProgressTimer = useCallback(() => {
    if (sessionFileDownloadProgressClearTimeoutRef.current !== undefined) {
      window.clearTimeout(sessionFileDownloadProgressClearTimeoutRef.current);
      sessionFileDownloadProgressClearTimeoutRef.current = undefined;
    }
  }, []);

  const updateUploadProgressForTransfer = useCallback((
    transferId: number,
    sessionId: UUID,
    progress: Omit<FileTransferProgressState, "sessionId" | "transferId">,
  ) => {
    // 中文注释：上传使用独立的会话操作 client；用户切到其他 session 时传输仍可继续。
    // 因此进度只能按 transferId 判定是否过期，不能用当前 attached session 过滤掉。
    if (activeUploadTransferIdRef.current !== transferId) {
      return;
    }
    setSessionFileUploadProgress((current) => {
      if (!current || current.transferId !== transferId) {
        return current;
      }
      const offsetBytes = Math.max(current.offsetBytes, progress.offsetBytes);
      const completed = current.completed || progress.completed;
      // 中文注释：HTTP 上传现在允许 2 并发；sent progress 和 committed progress
      // 可能乱序到达。UI 状态必须只前进，不能被较旧的 non-eof 响应拉回。
      return {
        sessionId,
        transferId,
        ...progress,
        offsetBytes,
        phase: completed ? "confirmed" : progress.phase,
        completed,
      };
    });
  }, []);

  const updateDownloadProgressForTransfer = useCallback((
    transferId: number,
    sessionId: UUID,
    progress: Omit<FileTransferProgressState, "sessionId" | "transferId">,
  ) => {
    // 中文注释：下载同样可能跨 session 切换继续执行，进度条应保持到传输完成或失败。
    if (activeDownloadTransferIdRef.current !== transferId) {
      return;
    }
    setSessionFileDownloadProgress((current) => {
      if (!current || current.transferId !== transferId) {
        return current;
      }
      return { sessionId, transferId, ...progress };
    });
  }, []);

  const scheduleUploadProgressClear = useCallback((transferId: number) => {
    if (activeUploadTransferIdRef.current !== transferId) {
      return;
    }
    clearSessionFileUploadProgressTimer();
    sessionFileUploadProgressClearTimeoutRef.current = window.setTimeout(() => {
      if (activeUploadTransferIdRef.current === transferId) {
        activeUploadTransferIdRef.current = undefined;
        setSessionFileUploadProgress((current) => current?.transferId === transferId ? undefined : current);
      }
      sessionFileUploadProgressClearTimeoutRef.current = undefined;
    }, 1200);
  }, [clearSessionFileUploadProgressTimer]);

  const scheduleDownloadProgressClear = useCallback((transferId: number) => {
    if (activeDownloadTransferIdRef.current !== transferId) {
      return;
    }
    clearSessionFileDownloadProgressTimer();
    sessionFileDownloadProgressClearTimeoutRef.current = window.setTimeout(() => {
      if (activeDownloadTransferIdRef.current === transferId) {
        activeDownloadTransferIdRef.current = undefined;
        setSessionFileDownloadProgress((current) => current?.transferId === transferId ? undefined : current);
      }
      sessionFileDownloadProgressClearTimeoutRef.current = undefined;
    }, 1200);
  }, [clearSessionFileDownloadProgressTimer]);

  useEffect(() => {
    void loadBrowserState().then((loaded) => {
      setState(loaded);
      if (!urlTouchedRef.current) {
        setUrl(browserReachableWsUrl(loaded.defaultUrl ?? defaultServer(loaded)?.url ?? defaultWsUrlFromPage()));
      }
      // 已配对的浏览器默认进入工作台；连接失败时再回落到后台管理页重新选择 daemon。
      setActiveSurface(defaultServer(loaded) && loaded.device ? "workspace" : "admin");
    });
  }, []);

  useEffect(() => {
    document.documentElement.lang = effectiveLocale;
    document.documentElement.dataset.theme = effectiveTheme;
    document.documentElement.style.colorScheme = effectiveTheme;
    document.querySelector('meta[name="theme-color"]')?.setAttribute(
      "content",
      effectiveTheme === "light" ? "#e5dfc5" : "#293136",
    );
  }, [effectiveLocale, effectiveTheme]);

  useEffect(() => {
    return () => {
      if (terminalOutputFlushFrameRef.current !== undefined) {
        window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
        terminalOutputFlushFrameRef.current = undefined;
      }
      if (connectionAutoRetryTimerRef.current !== undefined) {
        window.clearTimeout(connectionAutoRetryTimerRef.current);
        connectionAutoRetryTimerRef.current = undefined;
      }
      if (attachReconnectTimerRef.current !== undefined) {
        window.clearTimeout(attachReconnectTimerRef.current);
        attachReconnectTimerRef.current = undefined;
      }
      clearSessionFileUploadProgressTimer();
      clearSessionFileDownloadProgressTimer();
      closeWorkspaceClient();
    };
  }, [clearSessionFileDownloadProgressTimer, clearSessionFileUploadProgressTimer, closeWorkspaceClient]);

  useEffect(() => {
    renamingSessionIdRef.current = renamingSessionId;
  }, [renamingSessionId]);

  useEffect(() => {
    filesPanelWidthRef.current = filesPanelWidth;
  }, [filesPanelWidth]);

  useEffect(() => {
    sessionOrderRef.current = sessionOrder;
  }, [sessionOrder]);

  useEffect(() => {
    if (typeof window === "undefined") {
      return undefined;
    }

    const clampToViewport = () => {
      const nextWidth = clampFilesPanelWidth(filesPanelWidthRef.current, window.innerWidth);
      if (nextWidth !== filesPanelWidthRef.current) {
        setFilesPanelWidth(nextWidth);
      }
    };

    clampToViewport();
    window.addEventListener("resize", clampToViewport);
    return () => window.removeEventListener("resize", clampToViewport);
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") {
      return undefined;
    }

    const finishResize = (pointerId?: number) => {
      const drag = filesPanelResizeRef.current;
      if (!drag || (pointerId !== undefined && drag.pointerId !== pointerId)) {
        return;
      }
      filesPanelResizeRef.current = null;
      setIsFilesPanelResizing(false);
    };

    const handlePointerMove = (event: PointerEvent) => {
      const drag = filesPanelResizeRef.current;
      if (!drag || event.pointerId !== drag.pointerId) {
        return;
      }
      const nextWidth = clampFilesPanelWidth(drag.startWidth + drag.startX - event.clientX, window.innerWidth);
      setFilesPanelWidth(nextWidth);
    };

    const handlePointerUp = (event: PointerEvent) => finishResize(event.pointerId);
    const handlePointerCancel = (event: PointerEvent) => finishResize(event.pointerId);

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp);
    window.addEventListener("pointercancel", handlePointerCancel);
    return () => {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
      window.removeEventListener("pointercancel", handlePointerCancel);
    };
  }, []);

  const activeServer = useMemo<PairedServerState | undefined>(() => defaultServer(state), [state]);
  const activeServerIdRef = useRef<UUID | undefined>(activeServer?.server_id);
  useEffect(() => {
    activeServerIdRef.current = activeServer?.server_id;
  }, [activeServer?.server_id]);
  const hasPairedServer = Boolean(activeServer && state.device);
  const showConnectionStatus = hasPairedServer && !error && status !== "pairing";
  // session 列表刷新只是旁路请求，不能把正在显示的 xterm 卸载成 disconnected。
  const connectionReady = showConnectionStatus && status !== "idle" && status !== "connecting";
  const sessionOperators = useMemo(() => {
    if (!attachedSessionId) {
      return [];
    }
    return daemonClients.filter(
      (client) => client.online && client.attached_session_ids.includes(attachedSessionId),
    );
  }, [attachedSessionId, daemonClients]);
  const attachedSession = useMemo(
    () => sessions.find((session) => session.session_id === attachedSessionId),
    [attachedSessionId, sessions],
  );
  const toolbarSession = useMemo(
    () =>
      attachedSession ?? sessions.find((session) => session.session_id === selectedSessionId),
    [attachedSession, selectedSessionId, sessions],
  );
  const toolbarSessionName = useMemo(() => {
    if (!toolbarSession) {
      return t("app.noSession");
    }
    return sessionDisplayName(toolbarSession);
  }, [sessions, t, toolbarSession]);
  const toolbarSessionSize = toolbarSession ? terminalSizeDisplay(toolbarSession.size) : undefined;
  const toolbarLatency = toolbarSession ? formatLatency(daemonNetworkLatencyMs) : undefined;
  const toolbarLatencyLevel = latencyLevelClass(daemonNetworkLatencyMs);

  useEffect(() => {
    if (!activeServer?.url || !toolbarSession) {
      document.title = "Termd";
      return;
    }

    // 浏览器标题只使用 daemon 地址和当前 session 名称；URL query/fragment 可能包含 relay token，
    // 不能放进窗口标题或系统任务切换器。
    document.title = `Termd - ${daemonAddressForTitle(activeServer.url)} - ${sessionDisplayName(toolbarSession)}`;
  }, [activeServer?.url, toolbarSession]);

  const orderedSessions = useMemo(
    () => applyLocalSessionOrder(sessions, sessionOrder),
    [sessionOrder, sessions],
  );
  const pairedServerOptions = useMemo(
    () =>
      state.pairedServers.map((server, index) => ({
        server,
        label: daemonDisplayLabel(server, index, t),
      })),
    [state.pairedServers, t],
  );
  const showMobileWorkspaceMenu = isMobileLayout && connectionReady;
  const showMobileSessionsPanel = showMobileWorkspaceMenu && mobilePanel === "sessions";
  const showMobileFilesPanel = showMobileWorkspaceMenu && mobilePanel === "files";
  const mobileTitlePullReady = mobileTitlePullDistance >= MOBILE_TITLE_PULL_REFRESH_PX;
  const mobileTitlePullStyle =
    showMobileWorkspaceMenu && mobileTitlePullDistance > 0
      ? ({ "--termd-mobile-title-pull": `${mobileTitlePullDistance}px` } as CSSProperties)
      : undefined;
  const showDesktopFilesPanel = !isMobileLayout && filesPanelOpen;
  const desktopWorkspaceStyle =
    !isMobileLayout && showDesktopFilesPanel
      ? { gridTemplateColumns: `minmax(0, 1fr) ${filesPanelWidth}px` }
      : undefined;
  const mobileKeyboardOpen = isMobileLayout && activeSurface === "workspace" && visualViewportMetrics.keyboardOpen;
  const appShellStyle = isMobileLayout
    ? ({
        "--termd-layout-viewport-height": `${window.innerHeight}px`,
        "--termd-visual-viewport-height": `${mobileKeyboardOpen ? window.innerHeight : visualViewportMetrics.height}px`,
        "--termd-visual-viewport-offset-top": `${visualViewportMetrics.offsetTop}px`,
        "--termd-visual-viewport-keyboard-inset": `${visualViewportMetrics.keyboardInset}px`,
      } as CSSProperties)
    : undefined;
  const canOpenWorkspace = Boolean(activeServer && state.device);
  const canSaveRename = Boolean(renameDraft.trim()) && renameDraft.trim() !== renameOriginalName.trim();
  const activeDaemonLabel =
    pairedServerOptions.find((item) => item.server.server_id === activeServer?.server_id)?.label ?? t("app.noDaemon");
  const handleOpenAdmin = useCallback((options: { editConnection?: boolean } = {}) => {
    setActiveSurface("admin");
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
    // 只有显式进入连接编辑时才保留编辑器，普通返回管理页时收起它。
    setConnectionEditorOpen(Boolean(options.editConnection));
  }, []);

  const setSafeError = useCallback((caught: unknown) => {
    setError(toSafeError(caught));
    setStatus("error");
  }, []);

  const handlePreferencesChange = useCallback(
    (nextPreferences: BrowserPreferences) => {
      // 偏好是当前浏览器的纯 UI 状态；先乐观更新，保存失败再显示错误。
      setState((current) => ({ ...current, preferences: nextPreferences }));
      if (nextPreferences.notifications !== "off" && typeof Notification !== "undefined" && Notification.permission === "default") {
        void Notification.requestPermission().catch(() => undefined);
      }
      void saveBrowserPreferences(nextPreferences)
        .then((nextState) => setState(nextState))
        .catch(setSafeError);
    },
    [setSafeError],
  );

  const isIgnoredClosingSessionNotFound = useCallback((sessionId: UUID, caught: unknown) => {
    if (!closingSessionIdsRef.current.has(sessionId)) {
      return false;
    }
    return toSafeError(caught).code === "session_not_found";
  }, []);

  const clearSessionFiles = useCallback(() => {
    sessionFilesRequestSeqRef.current += 1;
    sessionGitRequestSeqRef.current += 1;
    sessionFilesFollowRefreshInFlightRef.current = false;
    setSessionFiles(undefined);
    setSessionFilesError(undefined);
    setSessionFilesLoading(false);
    setSessionGit(undefined);
    setSessionGitError(undefined);
    setSessionGitLoading(false);
    setFileEditor(undefined);
  }, []);

  const discardPendingTerminalOutput = useCallback(() => {
    // 终端输出由 xterm 自己维护 scrollback；React 只保留尚未写入 xterm 的短队列。
    terminalOutputQueueRef.current = [];
    if (terminalOutputFlushFrameRef.current !== undefined) {
      window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
      terminalOutputFlushFrameRef.current = undefined;
    }
  }, []);

  const clearTerminalOutput = useCallback(() => {
    const currentSessionId = attachedSessionRef.current;
    if (currentSessionId) {
      lastRenderedTerminalSeqRef.current.delete(currentSessionId);
    }
    discardPendingTerminalOutput();
    terminalOutputResetVersionRef.current += 1;
    setTerminalOutputResetVersion(terminalOutputResetVersionRef.current);
    return terminalOutputResetVersionRef.current;
  }, [discardPendingTerminalOutput]);

  const handleTerminalOutputResetApplied = useCallback((version: number) => {
    terminalOutputAppliedResetVersionRef.current = Math.max(
      terminalOutputAppliedResetVersionRef.current,
      version,
    );
    for (const [pendingVersion, resolvers] of terminalOutputResetWaitersRef.current) {
      if (pendingVersion <= terminalOutputAppliedResetVersionRef.current) {
        terminalOutputResetWaitersRef.current.delete(pendingVersion);
        for (const resolve of resolvers) {
          resolve();
        }
      }
    }
  }, []);

  const resolveTerminalOutputResetWaiters = useCallback(() => {
    for (const resolvers of terminalOutputResetWaitersRef.current.values()) {
      for (const resolve of resolvers) {
        resolve();
      }
    }
    terminalOutputResetWaitersRef.current.clear();
  }, []);

  const waitForTerminalOutputResetApplied = useCallback((version: number) => {
    if (terminalOutputAppliedResetVersionRef.current >= version) {
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      const resolvers = terminalOutputResetWaitersRef.current.get(version) ?? new Set<() => void>();
      resolvers.add(() => {
        resolve();
      });
      terminalOutputResetWaitersRef.current.set(version, resolvers);
    });
  }, []);

  const resetAttachReconnectState = useCallback(() => {
    if (attachReconnectTimerRef.current !== undefined) {
      window.clearTimeout(attachReconnectTimerRef.current);
      attachReconnectTimerRef.current = undefined;
    }
    attachReconnectKeyRef.current = undefined;
    attachReconnectAttemptsRef.current = 0;
    attachReconnectLastErrorRef.current = undefined;
  }, []);

  const cancelScheduledAttachSwitch = useCallback(() => {
    attachSwitchGenerationRef.current += 1;
    if (attachSwitchTimerRef.current !== undefined) {
      window.clearTimeout(attachSwitchTimerRef.current);
      attachSwitchTimerRef.current = undefined;
    }
  }, []);

  const closeAttachForReconnect = useCallback((client?: DirectClient) => {
    const belongsToCurrentAttach =
      !client ||
      attachClientRef.current === client ||
      pendingAttachClientRef.current === client;
    if (!belongsToCurrentAttach) {
      // 中文注释：旧 attach client 的异步 RPC 可能在用户已经切到新 session 后才失败。
      // 这类 stale 错误只关闭旧 client，不能取消新的 attach 计时器，也不能触发旧 session 重连。
      client?.close();
      return false;
    }
    cancelScheduledAttachSwitch();
    closeWorkspaceClient();
    pendingResizeKeyRef.current = undefined;
    lastCursorReportRef.current = "";
    lastCursorFocusedRef.current = undefined;
    if (cursorRefreshTimerRef.current !== undefined) {
      window.clearTimeout(cursorRefreshTimerRef.current);
      cursorRefreshTimerRef.current = undefined;
    }
    return true;
  }, [cancelScheduledAttachSwitch, closeWorkspaceClient]);

  const flushTerminalOutput = useCallback(() => {
    terminalOutputFlushFrameRef.current = undefined;
    // 这一帧里累积的 session_data 直接交给 xterm drain，避免每帧输出都触发 React 重渲染。
    terminalOutputDrainRef.current?.();
  }, []);

  const scheduleTerminalOutputFlush = useCallback(() => {
    if (terminalOutputFlushFrameRef.current !== undefined) {
      return;
    }
    terminalOutputFlushFrameRef.current = window.requestAnimationFrame(() => {
      flushTerminalOutput();
    });
  }, [flushTerminalOutput]);

  const enqueueTerminalOutput = useCallback((item: TerminalOutputItem) => {
    terminalOutputQueueRef.current.push(item);
    scheduleTerminalOutputFlush();
  }, [scheduleTerminalOutputFlush]);

  const takeTerminalOutput = useCallback(() => {
    const chunks = terminalOutputQueueRef.current;
    terminalOutputQueueRef.current = [];
    return chunks;
  }, []);

  const registerTerminalOutputDrain = useCallback((drain: () => void) => {
    terminalOutputDrainRef.current = drain;
    // TerminalPane 可能在已有 attach 输出之后才挂载；注册完成后立刻尝试消费积压输出。
    drain();
    return () => {
      if (terminalOutputDrainRef.current === drain) {
        terminalOutputDrainRef.current = undefined;
      }
    };
  }, []);

  const disconnectAttach = useCallback((options: AttachUiOptions = {}) => {
    const shouldCloseMobilePanel = options.closeMobilePanel ?? true;
    cancelScheduledAttachSwitch();
    resetAttachReconnectState();
    resolveTerminalOutputResetWaiters();
    receiveLoopActiveRef.current = false;
    receiveLoopGenerationRef.current += 1;
    // 中文注释：切换 session、主动断开、恢复重连都以 WebSocket 生命周期作为边界。
    // DirectClient.close 会先尽力 cancel 已知 terminal stream，再关闭 transport；即使 cancel
    // 没送达，daemon/relay 也能通过 WebSocket close 清掉旧 client context。
    closeWorkspaceClient();
    if (attachedSessionRef.current) {
      lastRenderedTerminalSeqRef.current.delete(attachedSessionRef.current);
    }
    attachedSessionRef.current = undefined;
    pendingResizeKeyRef.current = undefined;
    confirmedSessionSizesRef.current.clear();
    setAttachedSessionId(undefined);
    lastCursorReportRef.current = "";
    lastCursorFocusedRef.current = undefined;
    if (cursorRefreshTimerRef.current !== undefined) {
      window.clearTimeout(cursorRefreshTimerRef.current);
      cursorRefreshTimerRef.current = undefined;
    }
    clearTerminalOutput();
    clearSessionFiles();
    if (shouldCloseMobilePanel) {
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
    }
  }, [cancelScheduledAttachSwitch, clearSessionFiles, clearTerminalOutput, closeWorkspaceClient, resetAttachReconnectState, resolveTerminalOutputResetWaiters]);

  useEffect(() => {
    if (activeSurface !== "admin" || !attachClientRef.current) {
      return;
    }

    // 管理页会卸载 TerminalPane；继续保留旧 attach 会让返回工作台时 xterm 为空。
    // 这里仅断开本地 attach，daemon 端 session 仍保持运行，回到工作台后会自动重新 attach。
    userDetachedRef.current = false;
    autoAttachAttemptedSessionRef.current = undefined;
    reattachCurrentSessionOnOpenRef.current = true;
    disconnectAttach();
    setStatus("ready");
  }, [activeSurface, disconnectAttach]);

  const resetWorkspaceState = useCallback(() => {
    setSessions([]);
    confirmedSessionSizesRef.current.clear();
    closeWorkspaceClient();
    receiveLoopGenerationRef.current += 1;
    setSessionOrder([]);
    sessionOrderRef.current = [];
    autoAttachAttemptedSessionRef.current = undefined;
    attachingSessionIdRef.current = undefined;
    attachRequestIdRef.current += 1;
    cancelScheduledAttachSwitch();
    resolveTerminalOutputResetWaiters();
    reattachCurrentSessionOnOpenRef.current = false;
    userDetachedRef.current = false;
    setNewOutputSessionIds(new Set());
    lastRenderedTerminalSeqRef.current.clear();
    attachedSessionRef.current = undefined;
    pendingAttachClientRef.current = undefined;
    pendingTerminalAttachSessionRef.current = undefined;
    pendingResizeKeyRef.current = undefined;
    sessionPermissionIdsRef.current.clear();
    setAttachedSessionId(undefined);
    setDaemonClients([]);
    setDaemonStatus(undefined);
    setDaemonCpuHistory([]);
    setDaemonNetworkRate(undefined);
    setDaemonNetworkLatencyMs(undefined);
    daemonNetworkSampleRef.current = undefined;
    setDaemonStatusError(undefined);
    selectSession(undefined);
    renamingSessionIdRef.current = undefined;
    setRenamingSessionId(undefined);
    setRenameDraft("");
    setRenameOriginalName("");
    clearTerminalOutput();
    clearSessionFiles();
    autoCheckedServerRef.current = undefined;
  }, [cancelScheduledAttachSwitch, clearSessionFiles, clearTerminalOutput, closeWorkspaceClient, resolveTerminalOutputResetWaiters, selectSession]);

  const handleStartDaemonRename = useCallback(
    (serverId: UUID) => {
      const target = pairedServerOptions.find((item) => item.server.server_id === serverId);
      if (!target) {
        return;
      }
      setRenamingDaemonId(serverId);
      setDaemonRenameDraft(target.server.name?.trim() ?? target.label);
    },
    [pairedServerOptions],
  );

  const handleCancelDaemonRename = useCallback(() => {
    setRenamingDaemonId(undefined);
    setDaemonRenameDraft("");
  }, []);

  const handleSaveDaemonRename = useCallback(
    async (serverId: UUID) => {
      try {
        const nextState = await renameDaemon(serverId, daemonRenameDraft);
        setState(nextState);
        handleCancelDaemonRename();
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [daemonRenameDraft, handleCancelDaemonRename, setSafeError],
  );

  const handleForgetDaemon = useCallback(
    async (serverId: UUID) => {
      const wasActive = activeServer?.server_id === serverId;
      if (wasActive) {
        disconnectAttach();
        resetWorkspaceState();
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
      }

      try {
      const nextState = await forgetDaemon(serverId);
      setState(nextState);
      setRenamingDaemonId(undefined);
        setDaemonRenameDraft("");
        setConnectionEditorOpen(false);
        setMobilePanel(undefined);
        setMobileMenuOpen(false);

      const nextServer = defaultServer(nextState);
      activeServerIdRef.current = nextServer?.server_id;
      const nextUrl = nextState.defaultUrl ?? nextServer?.url ?? defaultWsUrlFromPage();
      setUrl(browserReachableWsUrl(nextUrl));
      setActiveSurface("admin");

        if (!nextState.pairedServers.length) {
          setConnectionEditorOpen(false);
          setStatus("idle");
        } else if (wasActive) {
          setStatus("idle");
        }
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [activeServer?.server_id, disconnectAttach, resetWorkspaceState, setSafeError],
  );

  const handlePair = useCallback(async (rawPairingInput?: string) => {
    setError(undefined);
    setStatus("pairing");
    const pairingInput = typeof rawPairingInput === "string" ? rawPairingInput : pairingToken;
    try {
      const device = await ensureDevice();
      const payload = parsePairingQrPayload(pairingInput);
      const routeServerId = payload?.server_id ?? activeServer?.server_id;
      if (!routeServerId) {
        throw new ProtocolClientError(
          "pairing_server_unknown",
          "pairing requires a known daemon server id",
        );
      }
      const daemonPublicKey = payload?.daemon_public_key ?? activeServer?.daemon_public_key;
      if (!daemonPublicKey) {
        throw new ProtocolClientError(
          "pairing_server_unknown",
          "pairing requires a known daemon public key",
        );
      }
      const rawCandidateUrl = payload?.ws_url ?? (url.trim() || activeServer?.url || defaultWsUrlFromPage());
      const candidateUrls = pairingWsUrlCandidates(rawCandidateUrl, routeServerId);
      const token = payload?.token ?? pairingInput.trim();
      const { client, effectiveUrl } = await connectPairingClient(
        candidateUrls,
        routeServerId,
        device.device_id,
        daemonPublicKey,
        PAIRING_CONNECTION_TIMEOUT_MS,
      );
      const accepted = await client.pair(token, device.device_public_key);
      client.close();
      const nextState = await recordPairing(accepted, effectiveUrl);
      activeServerIdRef.current = accepted.server_id;
      setState(nextState);
      setPairingToken("");
      setConnectionEditorOpen(false);
      resetWorkspaceState();
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      if (payload) {
        setUrl(effectiveUrl);
      }
      setActiveSurface("workspace");
      // 配对成功只建立信任关系，后续 session/client 列表仍交给统一的自动刷新流程加载。
      setStatus("idle");
    } catch (caught) {
      setPairingToken("");
      setSafeError(caught);
    }
  }, [activeServer, pairingToken, resetWorkspaceState, setSafeError, url]);

  const handleQrDetected = useCallback(
    (value: string) => {
      setQrScannerOpen(false);
      if (!parsePairingQrPayload(value)) {
        setPairingToken(value);
        return;
      }

      // 有效邀请码直接进入配对流程，避免把一次性 token 暴露在输入框里。
      void handlePair(value);
    },
    [handlePair],
  );

  const handleUrlChange = useCallback((nextUrl: string) => {
    urlTouchedRef.current = true;
    setUrl(nextUrl);
  }, []);

  const handleSaveConnectionUrl = useCallback(async () => {
    const server = activeServer;
    const device = state.device;
    if (!server || !device || !url.trim()) {
      setSafeError(new ProtocolClientError("missing_pairing", "device is not paired"));
      return;
    }
    const effectiveUrl = routeWsUrlForKnownServer(url.trim(), server.server_id) ?? url.trim();

    setError(undefined);
    setStatus("saving_url");
    let client: DirectClient | undefined;
    try {
      client = await DirectClient.connect(effectiveUrl, server.server_id, device.device_id, {
        expectedDaemonPublicKey: server.daemon_public_key,
      });
      await client.authenticate(device, { ...server, url: effectiveUrl });
      client.close();
      client = undefined;
      const nextState = await recordServerUrl(server.server_id, effectiveUrl);
      activeServerIdRef.current = server.server_id;
      setState(nextState);
      resetWorkspaceState();
      setConnectionEditorOpen(false);
      autoCheckedServerRef.current = undefined;
      setActiveSurface("workspace");
      // 保存新地址后复用自动刷新流程重新探测 daemon，避免工作台停留在空列表。
      setStatus("idle");
    } catch (caught) {
      setSafeError(caught);
    } finally {
      client?.close();
    }
  }, [activeServer, resetWorkspaceState, setSafeError, state.device, url]);

  const handleSelectServer = useCallback(
    async (serverId: UUID) => {
      const target = state.pairedServers.find((server) => server.server_id === serverId);
      if (!target || target.server_id === activeServer?.server_id) {
        return;
      }

      setError(undefined);
      // 中文注释：这里先同步推进逻辑上的 active daemon。旧 daemon 的 in-flight
      // session.list 可能晚于 IndexedDB/React 状态更新返回，必须立刻让请求守卫失效。
      activeServerIdRef.current = target.server_id;
      resetWorkspaceState();
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      autoCheckedServerRef.current = undefined;
      const nextState = await selectDefaultServer(target.server_id);
      setState(nextState);
      setUrl(browserReachableWsUrl(target.url));
      setConnectionEditorOpen(false);
      setActiveSurface("admin");
      setStatus("idle");
    },
    [activeServer?.server_id, resetWorkspaceState, state.pairedServers],
  );

  const authenticatedClient = useCallback(async (timeoutMs = APP_CONNECTION_TIMEOUT_MS, signal?: AbortSignal) => {
    const server = activeServer;
    const device = state.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    if (isTerminalTransportPaused()) {
      throw terminalTransportPausedError();
    }
    // 中文注释：document hidden 不能中断 terminal WebSocket。后台标签页仍应继续接收
    // stdout；这里只在浏览器明确 offline 时取消建连，避免恢复可见时被迫 snapshot 重绘。
    const transportAbort = createTransportAbortController();
    const linkedAbort = createLinkedAbortController(signal, transportAbort?.controller.signal);
    const abortSignal = linkedAbort?.controller.signal;
    let client: DirectClient | undefined;
    const closeClientOnAbort = () => client?.close();
    const reachableUrl = browserReachableWsUrl(server.url);
    const routeUrl = routeWsUrlForKnownServer(reachableUrl, server.server_id) ?? reachableUrl;
    try {
      throwIfConnectionAborted(abortSignal);
      let lastConnectError: unknown;
      const connectTimeoutMs = Math.min(timeoutMs, APP_SOCKET_CONNECT_TIMEOUT_MS);
      for (let attempt = 1; attempt <= APP_SOCKET_CONNECT_ATTEMPTS; attempt += 1) {
        try {
          client = await DirectClient.connect(routeUrl, server.server_id, device.device_id, {
            expectedDaemonPublicKey: server.daemon_public_key,
            timeoutMs: connectTimeoutMs,
            socketOpenTimeoutMs: Math.min(connectTimeoutMs, APP_SOCKET_OPEN_TIMEOUT_MS),
            socketOpenHedgeDelayMs: APP_SOCKET_OPEN_HEDGE_DELAY_MS,
            requestTimeoutMs: APP_CONNECTION_TIMEOUT_MS,
            signal: abortSignal,
          });
          break;
        } catch (caught) {
          lastConnectError = caught;
          client?.close();
          client = undefined;
          if (
            attempt >= APP_SOCKET_CONNECT_ATTEMPTS ||
            abortSignal?.aborted ||
            isTerminalTransportPaused() ||
            !isRetryableConnectionError(caught)
          ) {
            throw caught;
          }
          await waitForConnectionRetryDelay(abortSignal);
        }
      }
      if (!client) {
        throw lastConnectError ?? new ProtocolClientError("connection_error", "connection error");
      }
      abortSignal?.addEventListener("abort", closeClientOnAbort, { once: true });
      if (abortSignal?.aborted || isTerminalTransportPaused()) {
        throw terminalTransportPausedError();
      }
      // 中文注释：connect 后的 auth 仍属于同一个 workspace 建连生命周期；
      // session 切换 abort 时要立刻 reject 并关闭 socket，不能等 request timeout。
      await abortableConnectionStep(client.authenticate(device, { ...server, url: routeUrl }), abortSignal);
      if (abortSignal?.aborted || isTerminalTransportPaused()) {
        throw terminalTransportPausedError();
      }
      return client;
    } catch (caught) {
      // authenticate 发生在 route/E2EE 建立之后；这里失败时如果不关闭 socket，
      // relay 会长期保留一个只完成了前置握手的 client，后台重试会把这些半开连接堆满。
      client?.close();
      throw caught;
    } finally {
      abortSignal?.removeEventListener("abort", closeClientOnAbort);
      linkedAbort?.dispose();
      transportAbort?.dispose();
    }
  }, [activeServer, state.device]);

  const authenticatedWorkspaceClient = useCallback(async (timeoutMs = ATTACH_CONNECTION_TIMEOUT_MS) => {
    const existing = attachClientRef.current;
    if (existing && !existing.isClosed) {
      return existing;
    }
    if (!existing && attachedSessionRef.current) {
      // 中文注释：terminal reconnect 窗口里 attachedSessionRef 会保留当前会话，
      // attachClientRef 可能已被 closeWorkspaceClient 清空。此时 metadata/files/git
      // 不能抢先创建认证-only WebSocket，否则会覆盖后续 terminal attach 的主连接。
      throw new ProtocolClientError("connection_closed", "terminal connection is reconnecting");
    }
    if (existing?.isClosed) {
      if (attachedSessionRef.current) {
        const error = new ProtocolClientError("connection_closed", "terminal connection closed");
        // 中文注释：已有 attached session 时，关闭的 terminal client 不能被普通
        // metadata refresh 悄悄替换成“只认证未 terminal.attach”的 WebSocket。
        // 否则会出现上行 RPC 还能发、下行 terminal stream 不再消费的半活状态。
        if (attachReconnectHandlerRef.current(existing, error)) {
          throw error;
        }
      }
      attachClientRef.current = undefined;
      sessionPermissionIdsRef.current.clear();
    }
    if (workspaceClientPromiseRef.current) {
      return workspaceClientPromiseRef.current;
    }
    const requestGeneration = workspaceClientGenerationRef.current;
    const abortController = new AbortController();
    workspaceClientAbortControllerRef.current = abortController;
    const clearAbortController = () => {
      if (workspaceClientAbortControllerRef.current === abortController) {
        workspaceClientAbortControllerRef.current = undefined;
      }
    };
    let promise: Promise<DirectClient>;
    promise = authenticatedClient(timeoutMs, abortController.signal)
      .then((client) => {
        clearAbortController();
        if (workspaceClientGenerationRef.current !== requestGeneration) {
          // 中文注释：daemon 切换、session 切换或 workspace reset 可能发生在握手进行中。
          // 迟到的旧 client 只能关闭，不能重新写回 attachClientRef 污染当前 session。
          client.close();
          throw new ProtocolClientError("stale_connection", "session connection was superseded");
        }
        attachClientRef.current = client;
        workspaceClientPromiseRef.current = undefined;
        return client;
      })
      .catch((caught) => {
        clearAbortController();
        if (workspaceClientGenerationRef.current === requestGeneration) {
          workspaceClientPromiseRef.current = undefined;
        }
        throw caught;
      });
    workspaceClientPromiseRef.current = promise;
    return promise;
  }, [authenticatedClient]);

  const authenticatedSessionClient = useCallback(
    async (sessionId: UUID) => {
      // 中文注释：普通 session RPC 和 terminal stream 共用当前 session 的 WebSocket；
      // session 切换/重连会关闭旧 WebSocket 并重新认证，新连接需要重新补权限 attach。
      const client = await authenticatedWorkspaceClient();
      if (!sessionPermissionIdsRef.current.has(sessionId)) {
        await client.attachSessionPermission(sessionId);
        sessionPermissionIdsRef.current.add(sessionId);
      }
      return client;
    },
    [authenticatedWorkspaceClient],
  );

  const resolveSessionScopedClient = useCallback(
    async (sessionId: UUID): Promise<{ client: DirectClient; ownsClient: boolean }> => {
      return { client: await authenticatedSessionClient(sessionId), ownsClient: false };
    },
    [authenticatedSessionClient],
  );

  const openSessionOperationClient = useCallback(
    async (sessionId: UUID): Promise<{ client: DirectClient; ownsClient: true }> => {
      const client = await authenticatedClient(APP_CONNECTION_TIMEOUT_MS);
      try {
        // 文件上传/下载不应排在当前 terminal stream 的大 snapshot 后面。
        // 独立 permission-only 连接只拿 session 操作权限，不订阅 stdout。
        await client.attachSessionPermission(sessionId);
        return { client, ownsClient: true };
      } catch (caught) {
        client.close();
        throw caught;
      }
    },
    [authenticatedClient],
  );

  const loadSessionFiles = useCallback(
    async (
      sessionId: UUID,
      path?: string,
      options: { silent?: boolean; source?: "initial" | "manual" | "follow" } = {},
    ) => {
      const silent = Boolean(options.silent);
      const source = options.source ?? (path === undefined ? "initial" : "manual");
      const requestSeq = sessionFilesRequestSeqRef.current + 1;
      sessionFilesRequestSeqRef.current = requestSeq;
      if (!silent) {
        setSessionFilesLoading(true);
        setSessionFilesError(undefined);
      }
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        // 文件树当前位置是 daemon 端 session 共享状态；不传 path 时由 daemon 返回当前共享目录。
        const files = await client.listSessionFiles(sessionId, path);
        const isCurrentRequest = requestSeq === sessionFilesRequestSeqRef.current;
        const allowsFollowResult = source !== "follow" || sessionFilesFollowTerminalCwdRef.current;
        if (!isCurrentRequest || !allowsFollowResult) {
          return;
        }
        setSessionFiles(files);
        setSessionFilesError(undefined);
      } catch (caught) {
        if (!silent && requestSeq === sessionFilesRequestSeqRef.current) {
          // 文件列表是终端旁路信息；失败时只收敛到右侧 panel，不打断已 attach 的终端会话。
          setSessionFiles(undefined);
          setSessionFilesError(toSafeError(caught));
        }
      } finally {
        if (!silent && requestSeq === sessionFilesRequestSeqRef.current) {
          setSessionFilesLoading(false);
        }
      }
    },
    [authenticatedSessionClient],
  );

  const handleSessionFilesFollowTerminalCwdChange = useCallback((follow: boolean) => {
    sessionFilesFollowTerminalCwdRef.current = follow;
    setSessionFilesFollowTerminalCwd(follow);
  }, []);

  const loadSessionGit = useCallback(
    async (sessionId: UUID, options: { silent?: boolean } = {}) => {
      const silent = Boolean(options.silent);
      const requestServerId = activeServer?.server_id;
      const requestSeq = sessionGitRequestSeqRef.current + 1;
      sessionGitRequestSeqRef.current = requestSeq;
      const isCurrentRequest = () =>
        requestSeq === sessionGitRequestSeqRef.current &&
        activeServerIdRef.current === requestServerId &&
        attachedSessionRef.current === sessionId;
      if (!silent) {
        setSessionGitLoading(true);
        setSessionGitError(undefined);
      }
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const git = await client.getSessionGit(sessionId);
        if (!isCurrentRequest()) {
          return;
        }
        setSessionGit(git);
        setSessionGitError(undefined);
      } catch (caught) {
        if (!silent && isCurrentRequest()) {
          setSessionGit(undefined);
          setSessionGitError(toSafeError(caught));
        }
      } finally {
        if (!silent && isCurrentRequest()) {
          setSessionGitLoading(false);
        }
      }
    },
    [activeServer?.server_id, authenticatedSessionClient],
  );

  const handleRefresh = useCallback(async () => {
    if (isPagePaused()) {
      return;
    }
    const requestServerId = activeServer?.server_id;
    setError(undefined);
    setStatus("listing");
    const requestOrderGeneration = sessionOrderGenerationRef.current;
    const requestCreateGeneration = sessionCreateRequestIdRef.current;
    let sessionListApplied = false;
    try {
      const client = await authenticatedWorkspaceClient();
      const list = await client.listSessions();
      if (
        activeServerIdRef.current !== requestServerId ||
        requestCreateGeneration !== sessionCreateRequestIdRef.current
      ) {
        return;
      }
      const canApplyDaemonOrder =
        !pendingSessionReorderRef.current &&
        requestOrderGeneration === sessionOrderGenerationRef.current;
      const nextOrder = canApplyDaemonOrder
        ? sessionOrderFromDaemonList(list.sessions)
        : sessionOrderRef.current;
      if (canApplyDaemonOrder) {
        sessionOrderRef.current = nextOrder;
        setSessionOrder(nextOrder);
      }
      const orderedSessions = orderSessions(sortSessionsNewestFirst(list.sessions), nextOrder);
      confirmedSessionSizesRef.current = new Map(list.sessions.map((session) => [session.session_id, session.size]));
      const listedSessionIds = new Set(list.sessions.map((session) => session.session_id));
      const stickySessionId =
        attachingSessionIdRef.current ?? attachedSessionRef.current ?? selectedSessionIdRef.current;
      const nextSelectedSessionId = userDetachedRef.current
        ? undefined
        : stickySessionId && listedSessionIds.has(stickySessionId)
          ? stickySessionId
          : orderedSessions.at(0)?.session_id ?? renamingSessionIdRef.current ?? attachedSessionRef.current;
      setSessions((current) =>
        mergeSessionRefresh(list.sessions, current, [
          renamingSessionIdRef.current,
          attachedSessionRef.current,
        ], nextOrder),
      );
      // 列表刷新可能晚于用户点击 session 返回；不能用“第一行”覆盖用户刚选择/正在 attach 的目标。
      selectSession(nextSelectedSessionId);
      // session 列表刷新可能来自后台轮询或 cursor 同步；已有 attach 时保留右侧文件树，
      // 避免用户刷新 session 列表后文件 panel 被短暂清空。
      if (!attachedSessionRef.current) {
        clearSessionFiles();
      }
      if (!attachingSessionIdRef.current) {
        setStatus(attachedSessionRef.current ? "attached" : "ready");
      }
      // 中文注释：session.list 是刷新工作台的提交点。它成功后，即使后续非关键
      // daemon.clients 因 session 切换关闭了同一条 WebSocket，也不能把页面回滚到 admin。
      sessionListApplied = true;
      try {
        // session.list 是进入工作台的关键路径；daemon.clients 只是操作员展示元数据。
        // relay 或大量输出让该旁路 RPC 变慢时，不能把已经成功拿到的 session 列表回滚成连接失败。
        const clients = await client.listDaemonClients();
        if (
          activeServerIdRef.current === requestServerId &&
          requestCreateGeneration === sessionCreateRequestIdRef.current
        ) {
          setDaemonClients(clients.clients);
        }
      } catch {
        // 客户端列表下一轮后台刷新会再补；这里保持旧值，避免非关键元数据影响 attach。
      }
    } catch (caught) {
      if (
        activeServerIdRef.current !== requestServerId ||
        requestCreateGeneration !== sessionCreateRequestIdRef.current
      ) {
        return;
      }
      if (sessionListApplied) {
        return;
      }
      if (
        activeSurfaceRef.current === "workspace" &&
        isLocallySupersededConnectionError(caught) &&
        (selectedSessionIdRef.current || attachingSessionIdRef.current || attachedSessionRef.current)
      ) {
        // 中文注释：session 切换/自动 attach 会关闭旧 WebSocket。旧的 Refresh
        // 可能正复用这条连接，收到 connection_closed/stale_connection 只能说明它被本地
        // 新 session 连接取代，不能把 workspace 切回 admin。
        const nextStatus =
          statusRef.current === "attaching"
            ? "attaching"
            : attachedSessionRef.current
              ? "attached"
              : "ready";
        setStatus(nextStatus);
        return;
      }
      if (
        activeSurfaceRef.current === "workspace" &&
        (attachedSessionRef.current || attachClientRef.current)
      ) {
        // 中文注释：workspace 中已经有当前 session 的 WebSocket 时，session/list 只是旁路 segment。
        // relay 恢复或后台唤醒导致的短超时不能卸载 xterm，也不能升级成全局连接错误；
        // 真实终端断线由 attach receive loop 按长超时重连链路处理。
        const nextStatus =
          statusRef.current === "attaching"
            ? "attaching"
            : attachedSessionRef.current
              ? "attached"
              : "ready";
        setStatus(nextStatus);
        return;
      }
      setActiveSurface("admin");
      setSafeError(caught);
    } finally {
    }
  }, [activeServer?.server_id, authenticatedWorkspaceClient, clearSessionFiles, selectSession, setSafeError]);

  const refreshDaemonClients = useCallback(
    async () => {
      if (isPagePaused()) {
        return;
      }
      if (statusRef.current === "creating" || statusRef.current === "attaching") {
        // 中文注释：terminal.create/attach 是当前工作台的主链路。
        // 后台 session/client 刷新不能在慢 relay 上和终端建连竞争同一条 WebSocket。
        return;
      }
      if (daemonClientsRefreshInFlightRef.current) {
        return;
      }
      daemonClientsRefreshInFlightRef.current = true;
      const requestServerId = activeServer?.server_id;
      const requestOrderGeneration = sessionOrderGenerationRef.current;
      try {
        const client = await authenticatedWorkspaceClient();
        try {
          // 中文注释：状态和客户端列表复用当前 session 的 WebSocket，只在内层 segment 分类。
          const sessionList = await client.listSessions();
          const clientList = await client.listDaemonClients().catch(() => undefined);
          if (activeServerIdRef.current !== requestServerId) {
            return;
          }
          const canApplyDaemonOrder =
            !pendingSessionReorderRef.current &&
            requestOrderGeneration === sessionOrderGenerationRef.current;
          const nextOrder = canApplyDaemonOrder
            ? sessionOrderFromDaemonList(sessionList.sessions)
            : sessionOrderRef.current;
          if (canApplyDaemonOrder) {
            sessionOrderRef.current = nextOrder;
            setSessionOrder(nextOrder);
          }
          confirmedSessionSizesRef.current = new Map(sessionList.sessions.map((session) => [session.session_id, session.size]));
          setSessions((current) =>
            mergeSessionRefresh(sessionList.sessions, current, [
              renamingSessionIdRef.current,
              attachedSessionRef.current,
            ], nextOrder),
          );
          if (clientList) {
            setDaemonClients(clientList.clients);
          }
        } catch (caught) {
          if (isBrokenWorkspaceConnectionError(caught) && attachClientRef.current === client) {
            // 中文注释：后台列表刷新是旁路 segment；它只能把当前 transport 判为需要重连，
            // 不能自己直接清空 workspace。真正的终端收口统一走 attach 重连状态机，
            // 避免“连接已关闭”后页面停在无 client 状态。
            attachReconnectHandlerRef.current(client, caught);
          }
          throw caught;
        }
      } catch (caught) {
        // 后台 client/session 刷新失败不能把正在使用的 xterm 切到错误态；
        // 主 attach 连接有自己的重连路径，手动 Refresh 仍会显示错误。
        void caught;
      } finally {
        daemonClientsRefreshInFlightRef.current = false;
      }
    },
    [activeServer?.server_id, authenticatedWorkspaceClient],
  );

  const loadDaemonStatus = useCallback(async () => {
    if (isPagePaused()) {
      return;
    }
    if (statusRef.current === "creating" || statusRef.current === "attaching") {
      // 中文注释：状态栏是旁路信息；创建/进入终端期间跳过一轮，
      // 避免 RTT/status 请求在低带宽 relay 上排到 terminal.create 前后。
      return;
    }
    if (daemonStatusRefreshInFlightRef.current) {
      return;
    }
    daemonStatusRefreshInFlightRef.current = true;
    const requestServerId = activeServer?.server_id;
    const requestSeq = daemonStatusRequestSeqRef.current + 1;
    daemonStatusRequestSeqRef.current = requestSeq;
    const isCurrentRequest = () =>
      requestSeq === daemonStatusRequestSeqRef.current &&
      activeServerIdRef.current === requestServerId;
    setDaemonStatusLoading(true);
    setDaemonStatusError(undefined);
    try {
      const client = await authenticatedWorkspaceClient();
      try {
        // 中文注释：状态栏和 RTT 是非终端 segment，仍复用工作台可靠 WebSocket。
        const status = await client.getDaemonStatus();
        const latencyMs = await client.measureLatency().catch(() => undefined);
        if (!isCurrentRequest()) {
          return;
        }
        const nextNetworkSample = networkCounterSampleFromStatus(status, Date.now());
        setDaemonNetworkRate(networkRateFromSamples(daemonNetworkSampleRef.current, nextNetworkSample));
        daemonNetworkSampleRef.current = nextNetworkSample;
        if (latencyMs !== undefined) {
          setDaemonNetworkLatencyMs(latencyMs);
        }
        setDaemonStatus(status);
        // CPU 柱状图只做当前页面内缓存，避免把瞬时监控数据写入浏览器持久状态。
        setDaemonCpuHistory((current) => appendCpuSample(current, status.cpu_percent));
      } catch (caught) {
        if (isBrokenWorkspaceConnectionError(caught) && attachClientRef.current === client) {
          // 中文注释：状态轮询是旁路请求。它发现当前 transport 关闭时，只触发
          // terminal attach 的统一重连流程；不能在这里直接关闭 workspace client，
          // 否则前端会丢掉当前 session 连接并显示“连接已关闭”。
          attachReconnectHandlerRef.current(client, caught);
        }
        throw caught;
      }
    } catch (caught) {
      // 中文注释：daemon.status 是旁路状态轮询。session 切换会主动关闭旧 WebSocket，
      // 大量 terminal 输出也可能让状态 RPC 晚于 5s 返回；这些都不代表当前终端不可用。
      // 真实终端断线由 attach receive loop 和 workspace 刷新链路收口。
      if (isCurrentRequest() && isBackgroundStatusTransientError(caught)) {
        setDaemonStatusError(undefined);
      } else if (isCurrentRequest()) {
        setDaemonStatusError(toSafeError(caught));
      }
      if (isCurrentRequest() && !attachClientRef.current) {
        setDaemonNetworkLatencyMs(undefined);
      }
    } finally {
      daemonStatusRefreshInFlightRef.current = false;
      if (isCurrentRequest()) {
        setDaemonStatusLoading(false);
      }
    }
  }, [activeServer?.server_id, authenticatedWorkspaceClient]);

  const clearNewOutputMark = useCallback((sessionId: UUID) => {
    // 新输出提示只属于本地 UI；用户打开该 session 后立即清除，不回写 daemon。
    setNewOutputSessionIds((current) => {
      if (!current.has(sessionId)) {
        return current;
      }
      const next = new Set(current);
      next.delete(sessionId);
      return next;
    });
  }, []);

  const markNewOutputIfBackground = useCallback((sessionId: UUID) => {
    // 当前 attach 的 session 输出会直接进入 xterm，不需要再用列表颜色提示。
    if (sessionId === attachedSessionRef.current) {
      return;
    }
    setNewOutputSessionIds((current) => {
      if (current.has(sessionId)) {
        return current;
      }
      return new Set(current).add(sessionId);
    });
    maybeNotifyBrowser(
      preferences,
      t("sessions.openNewOutput", {
        name: sessionDisplayName(sessions.find((session) => session.session_id === sessionId) ?? { session_id: sessionId }),
      }),
      lastNotificationAtRef,
    );
  }, [preferences, sessions, t]);

  const applyConfirmedSessionSize = useCallback((sessionId: UUID, size: TerminalSize) => {
    const currentSize = confirmedSessionSizesRef.current.get(sessionId);
    if (currentSize && sameTerminalSize(currentSize, size)) {
      return;
    }
    // 中文注释：terminal snapshot/resize frame 里的 size 是渲染这些字节时的权威尺寸。
    // 先更新本地 session size，避免 TerminalPane 在写 snapshot 时被旧 sessionSize 拉回旧列宽。
    confirmedSessionSizesRef.current.set(sessionId, size);
    setSessions((current) =>
      current.map((session) =>
        session.session_id === sessionId ? { ...session, size } : session,
      ),
    );
    if (sessionId === attachedSessionRef.current) {
      const confirmedResizeKey = terminalSizeKey(sessionId, size);
      if (pendingResizeKeyRef.current === confirmedResizeKey) {
        pendingResizeKeyRef.current = undefined;
      }
    }
  }, []);

  useEffect(() => {
    if (
      !activeServer ||
      !state.device ||
      status !== "idle" ||
      autoCheckedServerRef.current === activeServer.server_id ||
      isPagePaused()
    ) {
      return;
    }
    autoCheckedServerRef.current = activeServer.server_id;
    setStatus("connecting");
    void handleRefresh();
  }, [activeServer, handleRefresh, state.device, status]);

  const startReceiveLoop = useCallback((client: DirectClient) => {
    const loopGeneration = receiveLoopGenerationRef.current + 1;
    receiveLoopGenerationRef.current = loopGeneration;
    receiveLoopActiveRef.current = true;
    const isCurrentLoop = () =>
      receiveLoopActiveRef.current &&
      receiveLoopGenerationRef.current === loopGeneration &&
      attachClientRef.current === client;
    const read = async () => {
      let processedMessages = 0;
      let processedBytes = 0;
      while (isCurrentLoop()) {
        try {
          const inner = await client.receiveInner();
          if (!isCurrentLoop()) {
            return;
          }
          processedMessages += 1;
          if (inner.type === "session_data") {
            const payload = inner.payload as SessionDataPayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES) {
                processedMessages = 0;
                await yieldToEventLoop();
              }
              continue;
            }
            const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
            enqueueTerminalOutput({ kind: "data", bytes });
            processedBytes += bytes.byteLength;
          } else if (inner.type === "terminal_frame") {
            const payload = inner.payload as RenderableTerminalFramePayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES) {
                processedMessages = 0;
                await yieldToEventLoop();
              }
              continue;
            }
            if (payload.kind === "snapshot") {
              applyConfirmedSessionSize(payload.session_id, payload.size);
              const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
              enqueueTerminalOutput({
                kind: "snapshot",
                bytes,
                baseSeq: payload.base_seq,
                size: payload.size,
              });
              processedBytes += bytes.byteLength;
            } else if (payload.kind === "output") {
              const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
              enqueueTerminalOutput({
                kind: "output",
                bytes,
                terminalSeq: payload.terminal_seq,
              });
              processedBytes += bytes.byteLength;
            } else if (payload.kind === "resize") {
              enqueueTerminalOutput({ kind: "resize", terminalSeq: payload.terminal_seq, size: payload.size });
            } else if (payload.kind === "exit") {
              enqueueTerminalOutput({ kind: "exit", terminalSeq: payload.terminal_seq });
            }
          } else if (inner.type === "session_activity") {
            const payload = inner.payload as SessionActivityPayload;
            markNewOutputIfBackground(payload.session_id);
          } else if (inner.type === "session_files_result") {
            const payload = inner.payload as SessionFilesResultPayload;
            // 非跟随模式下只接受当前请求的直接回写，不再让 daemon 的后台推送覆盖手动浏览目录。
            if (payload.session_id === attachedSessionRef.current && sessionFilesFollowTerminalCwdRef.current) {
              setSessionFiles(payload);
              setSessionFilesError(undefined);
              setSessionFilesLoading(false);
            }
          } else if (inner.type === "session_git_result") {
            const payload = inner.payload as SessionGitResultPayload;
            if (payload.session_id === attachedSessionRef.current) {
              setSessionGit(payload);
              setSessionGitError(undefined);
              setSessionGitLoading(false);
            }
          } else if (inner.type === "session_resized") {
            const payload = inner.payload as SessionResizedPayload;
            applyConfirmedSessionSize(payload.session_id, payload.size);
          }
          if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES || processedBytes >= RECEIVE_LOOP_YIELD_BYTES) {
            processedMessages = 0;
            processedBytes = 0;
            await yieldToEventLoop();
          }
        } catch (caught) {
          // 旧 attach 关闭可能晚于新 attach 启动；只有当前 client 的错误才能切到错误态。
          if (isCurrentLoop()) {
            if (attachReconnectHandlerRef.current(client, caught)) {
              return;
            }
            setSafeError(caught);
          }
          return;
        }
      }
    };
    void read();
  }, [applyConfirmedSessionSize, enqueueTerminalOutput, markNewOutputIfBackground, setSafeError]);

  const scheduleAttachReconnect = useCallback((staleClient: DirectClient, caught: unknown, options: AttachReconnectOptions = {}) => {
    if (userDetachedRef.current || !isRetryableConnectionError(caught)) {
      return false;
    }
    const sessionId = options.sessionId ?? attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId;
    if (!sessionId) {
      return false;
    }
    const reconnectKey = options.reconnectKey ?? `${activeServer?.server_id ?? "unknown"}:${sessionId}`;
    if (options.skipCurrentClientClose) {
      // retry catch 已经只清理了本轮重连创建的 pending client；这里按 key 续排，
      // 不能再拿最初的 stale client 去判断“是否属于当前 attach”。
      if (attachReconnectKeyRef.current !== reconnectKey) {
        return true;
      }
    } else if (!closeAttachForReconnect(staleClient)) {
      return true;
    }
    const lastTerminalSeq =
      options.lastTerminalSeq ?? lastRenderedTerminalSeqRef.current.get(sessionId);

    if (attachReconnectKeyRef.current !== reconnectKey) {
      attachReconnectKeyRef.current = reconnectKey;
      attachReconnectAttemptsRef.current = 0;
      attachReconnectLastErrorRef.current = caught;
    } else {
      attachReconnectLastErrorRef.current = caught;
    }

    discardPendingTerminalOutput();
    setError(undefined);

    if (isTerminalTransportPaused()) {
      // 中文注释：offline 期间不主动建新 WebSocket；恢复事件会按当前
      // session 重新进入 handleRetryConnection。hidden/blur 不应暂停 terminal stream。
      setStatus("ready");
      return true;
    }

    if (attachReconnectTimerRef.current !== undefined) {
      return true;
    }

    if (attachReconnectAttemptsRef.current >= ATTACH_RECONNECT_DELAYS_MS.length) {
      const finalError = attachReconnectLastErrorRef.current ?? caught;
      resetAttachReconnectState();
      setSafeError(finalError);
      return true;
    }

    const delayMs = ATTACH_RECONNECT_DELAYS_MS[attachReconnectAttemptsRef.current] ?? ATTACH_RECONNECT_DELAYS_MS.at(-1)!;
    attachReconnectAttemptsRef.current += 1;
    setStatus("attaching");
    attachReconnectTimerRef.current = window.setTimeout(() => {
      attachReconnectTimerRef.current = undefined;
      void (async () => {
        let client: DirectClient | undefined;
        try {
          if (isTerminalTransportPaused()) {
            setStatus("ready");
            return;
          }
          const isCurrentReconnect = () =>
            !userDetachedRef.current && attachReconnectKeyRef.current === reconnectKey;
          const closePendingReconnectClient = () => {
            // 重连计时器可能晚于用户手动切换 session；过期重连只关闭自己创建的连接。
            if (client && pendingAttachClientRef.current === client) {
              pendingAttachClientRef.current = undefined;
            }
            if (pendingTerminalAttachSessionRef.current === sessionId) {
              pendingTerminalAttachSessionRef.current = undefined;
            }
            client?.close();
            client = undefined;
          };
          client = await authenticatedClient(ATTACH_CONNECTION_TIMEOUT_MS);
          if (!isCurrentReconnect()) {
            closePendingReconnectClient();
            return;
          }
          pendingAttachClientRef.current = client;
          pendingTerminalAttachSessionRef.current = sessionId;
          const attached = await client.attachSession(
            sessionId,
            {
              ...(lastTerminalSeq !== undefined ? { lastTerminalSeq } : {}),
              timeoutMs: ATTACH_CONNECTION_TIMEOUT_MS,
            },
          );
          if (!isCurrentReconnect()) {
            client.detachSession(sessionId, "stale_reconnect");
            closePendingReconnectClient();
            return;
          }
          const attachedClient = client;
          client = undefined;
          pendingAttachClientRef.current = undefined;
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          // 中文注释：重连拿到 attach ack 后先发布当前 session。
          // reset 期间用户可能已经能在新 xterm 里输入；输入不能等 snapshot 开始消费后才生效。
          attachClientRef.current = attachedClient;
          attachedSessionRef.current = sessionId;
          sessionPermissionIdsRef.current.add(sessionId);
          confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
          selectSession(sessionId);
          setAttachedSessionId(sessionId);
          setSessions((current) => upsertAttachedSession(current, attached, sessionOrderRef.current));
          clearNewOutputMark(sessionId);
          setStatus("attached");
          if (lastTerminalSeq === undefined) {
            // 普通重连会重放完整 snapshot，必须等 TerminalPane 清屏确认后再启动输出消费；
            // 否则旧 xterm 的异步回调可能把 snapshot 写进旧实例。
            await waitForTerminalOutputResetApplied(clearTerminalOutput());
            if (!isCurrentReconnect() || userDetachedRef.current) {
              attachedClient.close();
              return;
            }
          }
          if (!isCurrentReconnect() || userDetachedRef.current || attachClientRef.current !== attachedClient) {
            attachedClient.close();
            return;
          }
          resetAttachReconnectState();
          startReceiveLoop(attachedClient);
          void loadSessionFiles(sessionId, undefined, { silent: true, source: "initial" });
          void loadSessionGit(sessionId, { silent: true });
          void refreshDaemonClients();
        } catch (retryError) {
          if (client && pendingAttachClientRef.current === client) {
            pendingAttachClientRef.current = undefined;
          }
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          client?.close();
          attachReconnectLastErrorRef.current = retryError;
          if (!attachReconnectHandlerRef.current(staleClient, retryError, {
            lastTerminalSeq,
            sessionId,
            reconnectKey,
            skipCurrentClientClose: true,
          })) {
            resetAttachReconnectState();
            setSafeError(retryError);
          }
        }
      })();
    }, delayMs);

    return true;
  }, [
    activeServer?.server_id,
    attachedSessionId,
    authenticatedClient,
    clearNewOutputMark,
    clearTerminalOutput,
    closeAttachForReconnect,
    discardPendingTerminalOutput,
    loadSessionFiles,
    loadSessionGit,
    refreshDaemonClients,
    resetAttachReconnectState,
    selectedSessionId,
    selectSession,
    setSafeError,
    startReceiveLoop,
    waitForTerminalOutputResetApplied,
  ]);

  attachReconnectHandlerRef.current = scheduleAttachReconnect;

  const handleTerminalResync = useCallback((lastTerminalSeq?: number) => {
    const client = attachClientRef.current;
    if (!client) {
      return;
    }
    const sessionId = attachedSessionRef.current;
    if (sessionId && lastTerminalSeq !== undefined) {
      lastRenderedTerminalSeqRef.current.set(sessionId, lastTerminalSeq);
    }
    scheduleAttachReconnect(
      client,
      new ProtocolClientError("terminal_resync", "terminal stream out of sync"),
      { lastTerminalSeq },
    );
  }, [scheduleAttachReconnect]);

  const handleTerminalSeqRendered = useCallback((terminalSeq: number) => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    lastRenderedTerminalSeqRef.current.set(sessionId, terminalSeq);
  }, []);

  const handleTerminalSizeRendered = useCallback((size: TerminalSize) => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    applyConfirmedSessionSize(sessionId, size);
  }, [applyConfirmedSessionSize]);

  const performAttach = useCallback(
    async (sessionId: UUID, options: AttachUiOptions = {}) => {
      const shouldCloseMobilePanel = options.closeMobilePanel ?? true;
      const closeMobileAttachChrome = () => {
        if (!shouldCloseMobilePanel) {
          return;
        }
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
      };
      if (attachingSessionIdRef.current === sessionId) {
        clearNewOutputMark(sessionId);
        closeMobileAttachChrome();
        return;
      }
      userDetachedRef.current = false;
      setError(undefined);
      setStatus("attaching");
      const attachRequestId = attachRequestIdRef.current + 1;
      attachRequestIdRef.current = attachRequestId;
      attachingSessionIdRef.current = sessionId;
      let outputClient: DirectClient | undefined;
      try {
        const isCurrentAttachRequest = () =>
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId;
        const closePendingAttachClients = () => {
          // 快速点击 session 时，旧连接可能刚完成握手才回到这里；只能关闭自己持有的 client，
          // 不能清掉更新一轮点击已经写入的 pending ref。
          if (outputClient && pendingAttachClientRef.current === outputClient) {
            pendingAttachClientRef.current = undefined;
          }
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          if (outputClient && outputClient !== attachClientRef.current) {
            outputClient.close();
          }
          outputClient = undefined;
        };
        const shouldRefreshCurrentAttach =
          reattachCurrentSessionOnOpenRef.current &&
          attachedSessionRef.current === sessionId &&
          Boolean(attachClientRef.current && !attachClientRef.current.isClosed);
        if (
          attachedSessionRef.current === sessionId &&
          attachClientRef.current &&
          !attachClientRef.current.isClosed &&
          !shouldRefreshCurrentAttach
        ) {
          clearNewOutputMark(sessionId);
          setStatus("attached");
          closeMobileAttachChrome();
          return;
        }
        reattachCurrentSessionOnOpenRef.current = false;
        disconnectAttach({ closeMobilePanel: shouldCloseMobilePanel });
        const resetVersion = clearTerminalOutput();
        outputClient = await authenticatedWorkspaceClient(ATTACH_CONNECTION_TIMEOUT_MS);
        if (!isCurrentAttachRequest()) {
          closePendingAttachClients();
          return;
        }
        pendingAttachClientRef.current = outputClient;
        pendingTerminalAttachSessionRef.current = sessionId;
        const attached = await outputClient.attachSession(sessionId, {
          timeoutMs: ATTACH_CONNECTION_TIMEOUT_MS,
        });
        if (!isCurrentAttachRequest()) {
          outputClient.detachSession(sessionId, "stale_attach");
          closePendingAttachClients();
          return;
        }
        const attachedClient = outputClient;
        outputClient = undefined;
        pendingAttachClientRef.current = undefined;
        if (pendingTerminalAttachSessionRef.current === sessionId) {
          pendingTerminalAttachSessionRef.current = undefined;
        }
        // 中文注释：输入和 resize 属于 terminal segment，必须复用当前 session 的 WebSocket。
        // 到这里 daemon 已确认 attach，先发布 client 和 session id，让 reset 窗口内的键盘输入
        // 能进入正确 stream；输出 receive loop 仍在 reset 确认后才启动，避免 snapshot 写到旧实例。
        attachClientRef.current = attachedClient;
        attachedSessionRef.current = sessionId;
        sessionPermissionIdsRef.current.add(sessionId);
        confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
        selectSession(sessionId);
        setAttachedSessionId(sessionId);
        setSessions((current) => upsertAttachedSession(current, attached, sessionOrderRef.current));
        clearNewOutputMark(sessionId);
        closeMobileAttachChrome();
        setStatus("attached");
        if (isMobileLayout) {
          // 移动端打开历史 session 后主动请求 xterm focus，让软键盘保持在终端下方。
          // 聚焦后的本地尺寸会作为 shared PTY 的权威尺寸上报给 daemon。
          setTerminalFocusRequest((request) => request + 1);
        }
        // 中文注释：DirectClient 的 WebSocket pump 会在 attach response 前后持续收包，
        // 但 App 的 receive loop 只有在这里启动。快速切换多个大输出 session 时，必须先
        // 等 TerminalPane 确认旧 xterm 已经清屏/重建，再把新 snapshot 从 DirectClient
        // 队列排进 xterm；否则新 snapshot 可能先写入旧实例。
        await waitForTerminalOutputResetApplied(resetVersion);
        if (!isCurrentAttachRequest() || userDetachedRef.current) {
          attachedClient.detachSession(sessionId);
          return;
        }
        if (!isCurrentAttachRequest() || userDetachedRef.current || attachClientRef.current !== attachedClient) {
          attachedClient.detachSession(sessionId);
          return;
        }
        startReceiveLoop(attachedClient);
        void loadSessionFiles(sessionId, undefined, { source: "initial" });
        void loadSessionGit(sessionId);
        void refreshDaemonClients();
      } catch (caught) {
        if (
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId
        ) {
          setSafeError(caught);
        }
      } finally {
        if (outputClient && pendingAttachClientRef.current === outputClient) {
          pendingAttachClientRef.current = undefined;
        }
        if (pendingTerminalAttachSessionRef.current === sessionId) {
          pendingTerminalAttachSessionRef.current = undefined;
        }
        if (outputClient && outputClient !== attachClientRef.current) {
          outputClient.close();
        }
        if (
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId
        ) {
          attachingSessionIdRef.current = undefined;
        }
      }
    },
    [
      authenticatedWorkspaceClient,
      clearNewOutputMark,
      clearTerminalOutput,
      disconnectAttach,
      loadSessionFiles,
      loadSessionGit,
      refreshDaemonClients,
      selectSession,
      setSafeError,
      isMobileLayout,
      startReceiveLoop,
      waitForTerminalOutputResetApplied,
    ],
  );

  const handleAttach = useCallback(
    (sessionId: UUID, options: AttachUiOptions = {}) => {
      const attachOptions: Required<AttachUiOptions> = {
        closeMobilePanel: options.closeMobilePanel ?? true,
      };
      userDetachedRef.current = false;
      // 中文注释：点击 session 先只更新 UI 选中态；真正 attach 延迟一个很短窗口。
      // 快速扫过多个大输出 session 时，只有最后停住的 session 会触发 daemon snapshot。
      selectSession(sessionId);
      clearNewOutputMark(sessionId);
      if (attachOptions.closeMobilePanel) {
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
      }

      if (attachingSessionIdRef.current === sessionId) {
        return;
      }
      if (
        attachedSessionRef.current === sessionId &&
        attachClientRef.current &&
        !attachClientRef.current.isClosed &&
        !reattachCurrentSessionOnOpenRef.current
      ) {
        setStatus("attached");
        return;
      }

      cancelScheduledAttachSwitch();
      attachRequestIdRef.current += 1;
      // 中文注释：新 session 一旦被点中，旧的 in-flight attach 立刻失效；
      // 只保留最后停住的目标，避免上一个 session 的大 snapshot 继续占用当前连接。
      if (
        attachedSessionRef.current !== undefined ||
        attachClientRef.current !== undefined ||
        pendingAttachClientRef.current !== undefined ||
        workspaceClientPromiseRef.current !== undefined
      ) {
        // 80ms 合并窗口只延迟“新 session attach”，不能让旧 session 的输出继续进入
        // xterm。否则旧的大 snapshot/持续输出会占住主线程和当前 session 连接。
        disconnectAttach();
      }
      if (pendingAttachClientRef.current && pendingAttachClientRef.current !== attachClientRef.current) {
        pendingAttachClientRef.current.close();
      }
      pendingAttachClientRef.current = undefined;
      attachingSessionIdRef.current = undefined;
      setError(undefined);
      setStatus("attaching");
      const generation = attachSwitchGenerationRef.current;
      attachSwitchTimerRef.current = window.setTimeout(() => {
        attachSwitchTimerRef.current = undefined;
        if (attachSwitchGenerationRef.current !== generation) {
          return;
        }
        void performAttach(sessionId, attachOptions);
      }, ATTACH_SWITCH_COALESCE_DELAY_MS);
    },
    [cancelScheduledAttachSwitch, clearNewOutputMark, performAttach, selectSession],
  );

  const handleOpenWorkspace = useCallback(() => {
    if (!activeServer || !state.device) {
      return;
    }
    setError(undefined);
    setActiveSurface("workspace");
    setConnectionEditorOpen(false);
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
    if (status === "error" || status === "idle" || sessions.length === 0) {
      // 从后台重新进入工作台时允许对当前 daemon 再做一次连通性探测；
      // daemon 切换中的旧刷新结果可能把 session 列表临时置空，打开工作台时要重新确认。
      autoCheckedServerRef.current = undefined;
      setStatus("idle");
    }
  }, [activeServer, sessions.length, state.device, status]);

  useEffect(() => {
    const sessionId = selectedSessionId;
    const shouldReattachCurrentSession =
      activeSurface === "workspace" && reattachCurrentSessionOnOpenRef.current;
    if (
      activeSurface !== "workspace" ||
      !connectionReady ||
      status !== "ready" ||
      !sessionId ||
      attachedSessionRef.current ||
      userDetachedRef.current ||
      (autoAttachAttemptedSessionRef.current === sessionId && !shouldReattachCurrentSession)
    ) {
      return;
    }

    // 首次打开或浏览器刷新后，session_list 只选中了第一行；这里补上真正的 attach。
    autoAttachAttemptedSessionRef.current = sessionId;
    // 从管理页回到工作台的后台 reattach 不能抢走用户刚打开的移动端面板。
    void handleAttach(sessionId, { closeMobilePanel: false });
  }, [activeSurface, connectionReady, handleAttach, selectedSessionId, status]);

  const handleCreateSession = useCallback(async () => {
    userDetachedRef.current = false;
    const createRequestId = sessionCreateRequestIdRef.current + 1;
    sessionCreateRequestIdRef.current = createRequestId;
    setError(undefined);
    disconnectAttach();
    clearTerminalOutput();
    setStatus("creating");
    let outputClient: DirectClient | undefined;
    try {
      const isCurrentCreateRequest = () => sessionCreateRequestIdRef.current === createRequestId;
      outputClient = await authenticatedWorkspaceClient(ATTACH_CONNECTION_TIMEOUT_MS);
      if (!isCurrentCreateRequest()) {
        if (outputClient !== attachClientRef.current) {
          outputClient.close();
        }
        outputClient = undefined;
        return;
      }
      pendingAttachClientRef.current = outputClient;
      // Web 只创建完整的默认 shell 会话，避免把 session 误导成一次性命令执行。
      const created = await outputClient.createSession([], DEFAULT_SESSION_SIZE, {
        // 中文注释：terminal.create 会同时建立新的 terminal stream，属于终端 attach 生命周期。
        // relay 低带宽抖动时不能套用普通 5s RPC 超时，否则响应晚到会被前端丢弃。
        timeoutMs: ATTACH_CONNECTION_TIMEOUT_MS,
      });
      if (!isCurrentCreateRequest()) {
        outputClient.detachSession(created.session_id);
        outputClient = undefined;
        return;
      }
      // 中文注释：terminal.create 本身已经创建并 attach 了 terminal stream。
      // 不能再立刻 terminal.attach 第二条 stream；慢 relay 下第一条 stream 的输出会把
      // 第二个 attach response 挤到普通 2s RPC 超时之后，造成新建 session 失败。
      const attachedClient = outputClient;
      outputClient = undefined;
      pendingAttachClientRef.current = undefined;
      attachClientRef.current = attachedClient;
      attachedSessionRef.current = created.session_id;
      sessionPermissionIdsRef.current.add(created.session_id);
      confirmedSessionSizesRef.current.set(created.session_id, created.size);
      selectSession(created.session_id);
      setAttachedSessionId(created.session_id);
      clearNewOutputMark(created.session_id);
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      const nextOrder = [created.session_id, ...sessionOrderRef.current.filter((sessionId) => sessionId !== created.session_id)];
      sessionOrderRef.current = nextOrder;
      setSessionOrder(nextOrder);
      setSessions((current) => upsertSession(current, created, nextOrder));
      // 新建 session 等价于打开一个新的 SSH shell，应立即把输入焦点交给 xterm。
      // 聚焦客户端会把自己的尺寸同步为 shared PTY 的权威尺寸。
      setTerminalFocusRequest((request) => request + 1);
      setStatus("attached");
      startReceiveLoop(attachedClient);
      void loadSessionFiles(created.session_id, undefined, { source: "initial" });
      void refreshDaemonClients();
    } catch (caught) {
      if (sessionCreateRequestIdRef.current === createRequestId) {
        setSafeError(caught);
      }
    } finally {
      if (outputClient && pendingAttachClientRef.current === outputClient) {
        pendingAttachClientRef.current = undefined;
      }
      if (outputClient && outputClient !== attachClientRef.current) {
        outputClient.close();
      }
    }
  }, [
    authenticatedWorkspaceClient,
    clearNewOutputMark,
    clearTerminalOutput,
    disconnectAttach,
    loadSessionFiles,
    refreshDaemonClients,
    selectSession,
    setSafeError,
    startReceiveLoop,
  ]);

  const handleRetryConnection = useCallback(async () => {
    if (isTerminalTransportPaused()) {
      return;
    }
    const sessionId = attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId;
    if (sessionId) {
      // PWA 从后台恢复时旧 WebSocket 可能已经被系统关闭；先断开旧 attach，
      // 否则 handleAttach 会误以为当前 session 已连接而直接短路返回。
      disconnectAttach();
      await performAttach(sessionId);
      return;
    }

    setError(undefined);
    setActiveSurface("workspace");
    autoCheckedServerRef.current = undefined;
    await handleRefresh();
  }, [attachedSessionId, disconnectAttach, handleRefresh, performAttach, selectedSessionId]);

  const scheduleResumeMetadataRefresh = useCallback(() => {
    window.setTimeout(() => {
      if (isPagePaused() || activeSurfaceRef.current !== "workspace") {
        return;
      }
      // 中文注释：后台恢复时 terminal WebSocket 重建和普通状态轮询是两条语义。
      // 即使恢复入口已经走了 attach 重建，也要补一轮状态刷新，避免后台期间超时的
      // status 请求把状态栏卡在旧采样上。
      void loadDaemonStatus();
      void refreshDaemonClients();
    }, 0);
  }, [loadDaemonStatus, refreshDaemonClients]);

  useEffect(() => {
    if (!error && (status === "ready" || status === "attached")) {
      connectionAutoRetryKeyRef.current = undefined;
      connectionAutoRetryAttemptsRef.current = 0;
    }
  }, [error, status]);

  useEffect(() => {
    if (connectionAutoRetryTimerRef.current !== undefined) {
      window.clearTimeout(connectionAutoRetryTimerRef.current);
      connectionAutoRetryTimerRef.current = undefined;
    }

    if (!error || !hasPairedServer || activeSurface !== "workspace") {
      return undefined;
    }

    const retryKey = [
      activeServer?.server_id ?? "unknown",
      attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId ?? "no-session",
    ].join(":");
    if (connectionAutoRetryKeyRef.current !== retryKey) {
      connectionAutoRetryKeyRef.current = retryKey;
      connectionAutoRetryAttemptsRef.current = 0;
    }
    if (connectionAutoRetryAttemptsRef.current >= CONNECTION_AUTO_RETRY_LIMIT) {
      return undefined;
    }

    connectionAutoRetryTimerRef.current = window.setTimeout(() => {
      connectionAutoRetryTimerRef.current = undefined;
      connectionAutoRetryAttemptsRef.current += 1;
      // 错误态自动恢复只复用手动 Refresh 的路径：有当前 session 就重新 attach，
      // 否则重新刷新 daemon 列表；失败后由新的 error 继续驱动剩余重试次数。
      void handleRetryConnection();
    }, CONNECTION_AUTO_RETRY_DELAY_MS);

    return () => {
      if (connectionAutoRetryTimerRef.current !== undefined) {
        window.clearTimeout(connectionAutoRetryTimerRef.current);
        connectionAutoRetryTimerRef.current = undefined;
      }
    };
  }, [activeServer?.server_id, activeSurface, attachedSessionId, error, handleRetryConnection, hasPairedServer, selectedSessionId]);

  useEffect(() => {
    const pauseOfflineConnection = () => {
      if (activeSurface !== "workspace") {
        return;
      }
      // 中文注释：浏览器切 offline 时，WebSocket 不一定会立刻触发 close。
      // 主动丢弃旧 transport，避免恢复后继续向半开连接写 terminal.attach/input。
      closeWorkspaceClient();
    };

    const resumeVisibleConnection = () => {
      if (isPagePaused() || activeSurface !== "workspace") {
        return;
      }
      // 中文注释：hidden/blur 不能再作为 terminal WebSocket 的断开依据。
      // 如果连接还在，就继续让后台收到的 stdout 留在同一条流里；恢复可见时只补旁路元数据。
      if (error) {
        void handleRetryConnection().finally(scheduleResumeMetadataRefresh);
        return;
      }
      if ((attachedSessionId || selectedSessionId) && (!attachClientRef.current || attachClientRef.current.isClosed)) {
        void handleRetryConnection().finally(scheduleResumeMetadataRefresh);
        return;
      }
      if (activeServer && state.device && (status === "idle" || status === "connecting")) {
        autoCheckedServerRef.current = undefined;
        setStatus("idle");
        void handleRefresh();
        return;
      }
      if (connectionReady) {
        void loadDaemonStatus();
        void refreshDaemonClients();
      }
    };

    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        return;
      }
      resumeVisibleConnection();
    };

    document.addEventListener("visibilitychange", handleVisibilityChange);
    window.addEventListener("focus", resumeVisibleConnection);
    window.addEventListener("offline", pauseOfflineConnection);
    window.addEventListener("online", resumeVisibleConnection);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      window.removeEventListener("focus", resumeVisibleConnection);
      window.removeEventListener("offline", pauseOfflineConnection);
      window.removeEventListener("online", resumeVisibleConnection);
    };
  }, [
    activeServer,
    activeSurface,
    attachedSessionId,
    closeWorkspaceClient,
    connectionReady,
    error,
    handleRefresh,
    handleRetryConnection,
    loadDaemonStatus,
    refreshDaemonClients,
    scheduleResumeMetadataRefresh,
    selectedSessionId,
    state.device,
    status,
  ]);

  const handleStartRename = useCallback((sessionId: UUID, currentName: string) => {
    renamingSessionIdRef.current = sessionId;
    setRenamingSessionId(sessionId);
    setRenameDraft(currentName);
    setRenameOriginalName(currentName);
  }, []);

  const handleCancelRename = useCallback(() => {
    renamingSessionIdRef.current = undefined;
    setRenamingSessionId(undefined);
    setRenameDraft("");
    setRenameOriginalName("");
  }, []);

  const handleSaveRename = useCallback(
    async (sessionId: UUID) => {
      const nextName = renameDraft.trim();
      if (!nextName || nextName === renameOriginalName.trim()) {
        return;
      }
      setError(undefined);
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const renamed = await sessionClient.client.renameSession(sessionId, nextName);
        setSessions((current) =>
          current.map((session) =>
            session.session_id === renamed.session_id ? { ...session, name: renamed.name } : session,
          ),
        );
        handleCancelRename();
      } catch (caught) {
        setSafeError(caught);
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [handleCancelRename, renameDraft, renameOriginalName, resolveSessionScopedClient, setSafeError],
  );

  const handleCloseSession = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      closingSessionIdsRef.current.add(sessionId);
      const wasAttached = attachedSessionRef.current === sessionId;
      const wasSelected = selectedSessionId === sessionId;
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        try {
          await sessionClient.client.closeSession(sessionId);
        } catch (caught) {
          if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
            throw caught;
          }
        }
        if (wasAttached) {
          // 先让 daemon 删除 session，再收口本地 attach，避免旧 cursor / resize 继续往已删除 session 发送。
          disconnectAttach();
          clearTerminalOutput();
        }
        setSessions((current) => current.filter((session) => session.session_id !== sessionId));
        confirmedSessionSizesRef.current.delete(sessionId);
        sessionOrderRef.current = sessionOrderRef.current.filter((candidate) => candidate !== sessionId);
        setSessionOrder(sessionOrderRef.current);
        clearNewOutputMark(sessionId);
        if (wasSelected) {
          selectSession(undefined);
          clearSessionFiles();
        }
        if (wasAttached || wasSelected) {
          setStatus("ready");
        }
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
        void refreshDaemonClients();
      } catch (caught) {
        setSafeError(caught);
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
        // 关闭当前会话时，旧 attach 连接上已经发出的 cursor/resize promise 可能稍后才失败；
        // 短暂保留 closing 标记，避免这些迟到的 session_not_found 覆盖掉成功删除后的 UI。
        window.setTimeout(() => {
          closingSessionIdsRef.current.delete(sessionId);
        }, 1000);
      }
    },
    [
      clearSessionFiles,
      clearTerminalOutput,
      disconnectAttach,
      clearNewOutputMark,
      isIgnoredClosingSessionNotFound,
      refreshDaemonClients,
      selectedSessionId,
      selectSession,
      resolveSessionScopedClient,
      setSafeError,
    ],
  );

  const handleReorderSessions = useCallback(
    (sessionIds: UUID[]) => {
      sessionOrderGenerationRef.current += 1;
      pendingSessionReorderRef.current = true;
      sessionOrderRef.current = sessionIds;
      setSessionOrder(sessionIds);
      setSessions((current) => orderSessions(current, sessionIds));

      void (async () => {
        try {
          const client = await authenticatedWorkspaceClient();
          const reordered = await client.reorderSessions(sessionIds);
          sessionOrderGenerationRef.current += 1;
          pendingSessionReorderRef.current = false;
          sessionOrderRef.current = reordered.session_ids;
          setSessionOrder(reordered.session_ids);
          setSessions((current) => orderSessions(current, reordered.session_ids));
        } catch (caught) {
          sessionOrderGenerationRef.current += 1;
          pendingSessionReorderRef.current = false;
          setSafeError(caught);
          void handleRefresh();
        }
      })();
    },
    [authenticatedWorkspaceClient, handleRefresh, setSafeError],
  );

  const handleForgetOfflineClient = useCallback(
    async (deviceId: UUID) => {
      if (forgettingClientIdsRef.current.has(deviceId)) {
        return;
      }
      setError(undefined);
      forgettingClientIdsRef.current.add(deviceId);
      setForgettingClientIds((current) => new Set(current).add(deviceId));
      try {
        const client = await authenticatedWorkspaceClient();
        await client.forgetDaemonClient(deviceId);
        setDaemonClients((current) => current.filter((candidate) => candidate.device_id !== deviceId));
      } catch (caught) {
        setSafeError(caught);
      } finally {
        forgettingClientIdsRef.current.delete(deviceId);
        setForgettingClientIds((current) => {
          const next = new Set(current);
          next.delete(deviceId);
          return next;
        });
      }
    },
    [authenticatedWorkspaceClient, setSafeError],
  );

  const handleTerminalInput = useCallback(
    async (data: string) => {
      // 中文注释：终端输入必须和终端输出落在当前 session 的同一条可靠 WebSocket 上，靠 segment 顺序
      // 保证 stdin/stdout/resize 的相对顺序；普通 RPC 只是同一连接里的非终端 segment。
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId) {
        return;
      }
      try {
        await client.sendSessionData(sessionId, new TextEncoder().encode(data));
      } catch (caught) {
        if (isRetryableConnectionError(caught) && attachReconnectHandlerRef.current(client, caught)) {
          return;
        }
        if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
          setSafeError(caught);
        }
      }
    },
    [isIgnoredClosingSessionNotFound, setSafeError],
  );

  const handleResize = useCallback(
    (size: { rows: number; cols: number; pixel_width: number; pixel_height: number }) => {
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId) {
        return;
      }
      const currentSize =
        confirmedSessionSizesRef.current.get(sessionId) ??
        sessions.find((session) => session.session_id === sessionId)?.size;
      const nextResizeKey = terminalSizeKey(sessionId, size);
      if (
        (currentSize && sameTerminalSize(currentSize, size)) ||
        pendingResizeKeyRef.current === nextResizeKey
      ) {
        return;
      }
      pendingResizeKeyRef.current = nextResizeKey;
      // 这里仅向 daemon 请求 resize，不乐观改本地 session size，也不等待这个调用读取回执。
      // 中文注释：resize 和输入都在 terminal segment；普通 RPC 是当前 session 连接里的非终端 segment。
      void client.requestSessionResize(sessionId, size).catch((caught) => {
        if (isRetryableConnectionError(caught) && attachReconnectHandlerRef.current(client, caught)) {
          return;
        }
        if (pendingResizeKeyRef.current === nextResizeKey) {
          pendingResizeKeyRef.current = undefined;
        }
        if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
          setSafeError(caught);
        }
      });
    },
    [isIgnoredClosingSessionNotFound, sessions, setSafeError],
  );

  const handleCursorChange = useCallback(
    (presence: SessionCursorPresence) => {
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId) {
        return;
      }
      const nextCursor = `${sessionId}:${presence.row}:${presence.col}:${presence.focused}`;
      if (lastCursorReportRef.current === nextCursor) {
        return;
      }
      lastCursorReportRef.current = nextCursor;
      const focusChanged = lastCursorFocusedRef.current !== presence.focused;
      lastCursorFocusedRef.current = presence.focused;
      void client.sendSessionCursor(sessionId, presence).catch((caught) => {
        if (isRetryableConnectionError(caught) && attachReconnectHandlerRef.current(client, caught)) {
          return;
        }
        if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
          setSafeError(caught);
        }
      });
      if (focusChanged && cursorRefreshTimerRef.current === undefined) {
        cursorRefreshTimerRef.current = window.setTimeout(() => {
          cursorRefreshTimerRef.current = undefined;
          void refreshDaemonClients();
        }, 500);
      }
    },
    [isIgnoredClosingSessionNotFound, refreshDaemonClients, setSafeError],
  );

  useEffect(() => {
    if (!attachedSessionId || !connectionReady) {
      return undefined;
    }
    const refreshTimer = window.setInterval(() => {
      void refreshDaemonClients();
    }, 2000);
    return () => window.clearInterval(refreshTimer);
  }, [attachedSessionId, connectionReady, refreshDaemonClients]);

  useEffect(() => {
    if (!connectionReady) {
      return undefined;
    }
    void loadDaemonStatus();
    const timer = window.setInterval(() => {
      void loadDaemonStatus();
    }, DAEMON_STATUS_POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [connectionReady, loadDaemonStatus]);

  useEffect(() => {
    if (!attachedSessionId || !connectionReady || !sessionFilesFollowTerminalCwd || sessionFilesLoading) {
      return undefined;
    }

    const refreshFromTerminalCwd = () => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId || sessionFilesFollowRefreshInFlightRef.current) {
        return;
      }
      sessionFilesFollowRefreshInFlightRef.current = true;
      // 跟随模式必须不传 path；daemon 会按当前 PTY cwd 返回文件树位置。
      void loadSessionFiles(sessionId, undefined, { silent: true, source: "follow" }).finally(() => {
        sessionFilesFollowRefreshInFlightRef.current = false;
      });
    };

    const timer = window.setInterval(refreshFromTerminalCwd, FILES_CWD_FOLLOW_POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [attachedSessionId, connectionReady, loadSessionFiles, sessionFilesFollowTerminalCwd, sessionFilesLoading]);

  const handleOpenDirectory = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      // 用户开始手动浏览目录时，立即退出自动跟随，避免下一次轮询把目录打回终端 cwd。
      handleSessionFilesFollowTerminalCwdChange(false);
      void loadSessionFiles(sessionId, path, { source: "manual" });
    },
    [handleSessionFilesFollowTerminalCwdChange, loadSessionFiles],
  );

  const handleGoToFilePath = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      // 手动输入目录路径时同样切到浏览模式，避免和“跟随终端 cwd”互相覆盖。
      handleSessionFilesFollowTerminalCwdChange(false);
      void loadSessionFiles(sessionId, resolveRemoteDirectoryPath(sessionFiles?.path ?? "", path), { source: "manual" });
    },
    [handleSessionFilesFollowTerminalCwdChange, loadSessionFiles, sessionFiles?.path],
  );

  const handleRefreshSessionFiles = useCallback(() => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    void loadSessionFiles(sessionId, sessionFilesFollowTerminalCwd ? undefined : sessionFiles?.path, { source: "manual" });
  }, [loadSessionFiles, sessionFiles?.path, sessionFilesFollowTerminalCwd]);

  const handleRefreshSessionGit = useCallback(() => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    void loadSessionGit(sessionId);
  }, [loadSessionGit]);

  const handleSessionGitAction = useCallback(
    async (
      worktree: SessionGitWorktreePayload,
      change: SessionGitFileChangePayload,
      action: SessionGitActionKind,
    ) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionGitLoading(true);
      setSessionGitError(undefined);
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        await sessionClient.client.applySessionGitAction(sessionId, worktree.path, change.path, action);
        await loadSessionGit(sessionId);
      } catch (caught) {
        setSessionGitError(toSafeError(caught));
        setSessionGitLoading(false);
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [loadSessionGit, resolveSessionScopedClient],
  );

  const handleTerminalSearch = useCallback(
    async (query: string): Promise<SessionSearchResultPayload> => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        throw new ProtocolClientError("invalid_state", "no attached session");
      }
      const client = await authenticatedSessionClient(sessionId);
      return client.searchSessionOutput(sessionId, query, { maxResults: 80 });
    },
    [authenticatedSessionClient],
  );

  const handleOpenGitDiff = useCallback(
    async (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged = false) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      const path = change?.path ?? worktree.path;
      setDiffViewer({
        path,
        name: change ? basenameRemotePath(change.path) : t("git.graph"),
        text: "",
        loading: true,
      });
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const diff: SessionGitDiffResultPayload = await sessionClient.client.getSessionGitDiff(sessionId, worktree.path, change?.path, staged);
        setDiffViewer({
          path: diff.file_path ?? diff.worktree_path,
          name: diff.file_path ? basenameRemotePath(diff.file_path) : t("git.graph"),
          text: diff.diff || "\n",
          loading: false,
        });
      } catch (caught) {
        setDiffViewer((current) => ({
          path: current?.path ?? path,
          name: current?.name ?? path,
          text: current?.text ?? "",
          loading: false,
          error: translateSafeErrorMessage(toSafeError(caught), t),
        }));
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [resolveSessionScopedClient, t],
  );

  const handleSessionFilesPanelTabChange = useCallback(
    (tab: "files" | "git") => {
      setSessionFilesPanelTab(tab);
      const sessionId = attachedSessionRef.current;
      if (tab === "git" && sessionId) {
        void loadSessionGit(sessionId);
      }
    },
    [loadSessionGit],
  );

  const handleUploadFile = useCallback(
    async (file: File) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      const transferId = nextFileTransferId();
      const uploadPath = joinRemotePath(sessionFiles?.path ?? "", file.name);
      activeUploadTransferIdRef.current = transferId;
      clearSessionFileUploadProgressTimer();
      setSessionFileUploadProgress({
        sessionId,
        transferId,
        name: file.name,
        offsetBytes: 0,
        sizeBytes: file.size,
        phase: "sending",
        completed: false,
      });
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await openSessionOperationClient(sessionId);
        await sessionClient.client.uploadSessionFile(sessionId, uploadPath, file, {
          onProgress: (progress) => {
            updateUploadProgressForTransfer(transferId, sessionId, {
              name: file.name,
              offsetBytes: progress.offset_bytes,
              sizeBytes: progress.size_bytes,
              phase: progress.eof ? "confirmed" : "committing",
              completed: progress.eof,
            });
          },
          onSentProgress: (sentBytes, sizeBytes) => {
            updateUploadProgressForTransfer(transferId, sessionId, {
              name: file.name,
              offsetBytes: sentBytes,
              sizeBytes,
              phase: sentBytes >= sizeBytes ? "committing" : "sending",
              completed: false,
            });
          },
        });
        const refreshed = await sessionClient.client.listSessionFiles(sessionId, sessionFiles?.path);
        if (attachedSessionRef.current === sessionId) {
          setSessionFiles(refreshed);
          setSessionFilesError(undefined);
        }
      } catch (caught) {
        // 中文注释：上传可能在用户切到其他 session 后才失败；
        // 旧 session 的错误不能污染当前文件 panel。
        if (attachedSessionRef.current === sessionId) {
          setSessionFilesError(toSafeError(caught));
        }
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
        // 完成后保留很短时间，避免小文件上传时进度条一闪而过。
        scheduleUploadProgressClear(transferId);
        if (attachedSessionRef.current === sessionId) {
          setSessionFilesLoading(false);
        }
      }
    },
    [
      clearSessionFileUploadProgressTimer,
      nextFileTransferId,
      openSessionOperationClient,
      scheduleUploadProgressClear,
      sessionFiles?.path,
      updateUploadProgressForTransfer,
    ],
  );

  const handleOpenFile = useCallback(
    async (entry: SessionFileEntryPayload) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId || entry.kind !== "file") {
        return;
      }
      if (entry.size_bytes > TEXT_FILE_EDITOR_MAX_BYTES) {
        setFileEditor({
          path: entry.path,
          name: entry.name,
          text: "",
          loading: false,
          saving: false,
          error: t("error.fileEditTooLarge"),
        });
        return;
      }

      setSessionFilesError(undefined);
      setFileEditor({
        path: entry.path,
        name: entry.name,
        text: "",
        loading: true,
        saving: false,
      });
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const payload = await readEditableSessionFile(sessionClient.client, sessionId, entry.path);
        setFileEditor({
          path: payload.path,
          name: entry.name,
          text: new TextDecoder().decode(payload.bytes),
          loading: false,
          saving: false,
        });
      } catch (caught) {
        setFileEditor((current) => ({
          path: current?.path ?? entry.path,
          name: current?.name ?? entry.name,
          text: current?.text ?? "",
          loading: false,
          saving: false,
          error: translateSafeErrorMessage(toSafeError(caught), t),
        }));
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [resolveSessionScopedClient, t],
  );

  const handleOpenGitFile = useCallback(
    (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => {
      const path = joinRemotePath(worktree.path, change.path);
      void handleOpenFile({
        name: basenameRemotePath(change.path),
        path,
        kind: "file",
        size_bytes: 0,
        modified_at_ms: null,
      });
    },
    [handleOpenFile],
  );

  const handleSaveOpenFile = useCallback(
    async (text: string) => {
      const sessionId = attachedSessionRef.current;
      const editor = fileEditor;
      if (!sessionId || !editor) {
        return;
      }
      setFileEditor({ ...editor, text, saving: true, error: undefined });
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const written = await sessionClient.client.writeSessionFile(sessionId, editor.path, new TextEncoder().encode(text));
        setFileEditor({
          path: written.path,
          name: editor.name,
          text,
          loading: false,
          saving: false,
        });
        await loadSessionFiles(sessionId, sessionFiles?.path, { source: "manual" });
      } catch (caught) {
        setFileEditor({ ...editor, text, loading: false, saving: false, error: translateSafeErrorMessage(toSafeError(caught), t) });
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [fileEditor, loadSessionFiles, resolveSessionScopedClient, sessionFiles?.path, t],
  );

  const handleDownloadFile = useCallback(
    async (entry: { name: string; path: string; size_bytes?: number }) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      const transferId = nextFileTransferId();
      activeDownloadTransferIdRef.current = transferId;
      clearSessionFileDownloadProgressTimer();
      setSessionFileDownloadProgress({
        sessionId,
        transferId,
        name: entry.name,
        offsetBytes: 0,
        sizeBytes: entry.size_bytes ?? 0,
        completed: false,
      });
      setSessionFilesError(undefined);
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await openSessionOperationClient(sessionId);
        await downloadSessionFile(sessionClient.client, sessionId, entry.name, entry.path, (receivedBytes, sizeBytes, completed) => {
          updateDownloadProgressForTransfer(transferId, sessionId, {
            name: entry.name,
            offsetBytes: receivedBytes,
            sizeBytes,
            completed,
          });
        });
      } catch (caught) {
        // 中文注释：下载错误只属于发起下载的 session；切换后不覆盖新 session 文件状态。
        if (attachedSessionRef.current === sessionId) {
          setSessionFilesError(toSafeError(caught));
        }
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
        // 完成或失败后短暂停留，给用户看清最后一次传输状态。
        scheduleDownloadProgressClear(transferId);
      }
    },
    [
      clearSessionFileDownloadProgressTimer,
      nextFileTransferId,
      openSessionOperationClient,
      scheduleDownloadProgressClear,
      updateDownloadProgressForTransfer,
    ],
  );

  const handleDeleteFile = useCallback(
    async (entry: { path: string }) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let sessionClient: { client: DirectClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        await sessionClient.client.deleteSessionFile(sessionId, entry.path);
        await loadSessionFiles(sessionId, sessionFiles?.path, { source: "manual" });
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
        setSessionFilesLoading(false);
      }
    },
    [loadSessionFiles, resolveSessionScopedClient, sessionFiles?.path],
  );

  const requestMobileTerminalFocus = useCallback(() => {
    if (isMobileLayout && attachedSessionId) {
      // 移动端关闭覆盖面板后回到终端输入场景，主动恢复 xterm focus 以保持键盘常驻。
      setTerminalFocusRequest((request) => request + 1);
    }
  }, [attachedSessionId, isMobileLayout]);

  const handleHideFiles = useCallback(() => {
    if (isMobileLayout) {
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      requestMobileTerminalFocus();
      return;
    }
    setFilesPanelOpen(false);
  }, [isMobileLayout, requestMobileTerminalFocus]);

  const handleToggleMobileMenu = useCallback(() => {
    if (!isMobileLayout || !connectionReady) {
      return;
    }
    setMobilePanel(undefined);
    setMobileMenuOpen((open) => !open);
  }, [connectionReady, isMobileLayout]);

  const handleOpenMobileSessions = useCallback(() => {
    setMobileMenuOpen(false);
    setMobilePanel("sessions");
  }, []);

  const resetMobileTitlePull = useCallback(() => {
    mobileTitlePullGestureRef.current = undefined;
    setMobileTitlePullDistance(0);
  }, []);

  const handleMobileTitlePointerDown = useCallback((event: ReactPointerEvent<HTMLButtonElement>) => {
    if (!isMobileLayout || !connectionReady || event.pointerType !== "touch" || event.button !== 0) {
      return;
    }
    mobileTitlePullGestureRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      dragging: false,
    };
    setMobileTitlePullDistance(0);
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // jsdom 和部分旧移动浏览器不支持 pointer capture；手势仍可按当前事件序列工作。
    }
  }, [connectionReady, isMobileLayout]);

  const handleMobileTitlePointerMove = useCallback((event: ReactPointerEvent<HTMLButtonElement>) => {
    const gesture = mobileTitlePullGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId || event.pointerType !== "touch") {
      return;
    }
    const dx = event.clientX - gesture.startX;
    const dy = event.clientY - gesture.startY;
    if (!gesture.dragging) {
      if (dy < MOBILE_TITLE_PULL_START_PX || dy <= Math.abs(dx) * 1.5) {
        return;
      }
      gesture.dragging = true;
      suppressMobileTitleClickRef.current = true;
    }
    event.preventDefault();
    event.stopPropagation();
    setMobileTitlePullDistance(Math.min(MOBILE_TITLE_PULL_MAX_PX, Math.max(0, dy)));
  }, []);

  const handleMobileTitlePointerUp = useCallback((event: ReactPointerEvent<HTMLButtonElement>) => {
    const gesture = mobileTitlePullGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    const shouldRefresh = gesture.dragging && event.clientY - gesture.startY >= MOBILE_TITLE_PULL_REFRESH_PX;
    if (gesture.dragging) {
      event.preventDefault();
      event.stopPropagation();
      suppressMobileTitleClickRef.current = true;
    }
    try {
      event.currentTarget.releasePointerCapture(event.pointerId);
    } catch {
      // pointer capture 不是刷新动作的前置条件；释放失败只说明浏览器没有捕获该 pointer。
    }
    resetMobileTitlePull();
    if (shouldRefresh) {
      void handleRefresh();
    }
  }, [handleRefresh, resetMobileTitlePull]);

  const handleMobileTitlePointerCancel = useCallback((event: ReactPointerEvent<HTMLButtonElement>) => {
    const gesture = mobileTitlePullGestureRef.current;
    if (!gesture || gesture.pointerId !== event.pointerId) {
      return;
    }
    resetMobileTitlePull();
  }, [resetMobileTitlePull]);

  const handleMobileTitleClick = useCallback((event: ReactMouseEvent<HTMLButtonElement>) => {
    if (suppressMobileTitleClickRef.current) {
      event.preventDefault();
      event.stopPropagation();
      suppressMobileTitleClickRef.current = false;
      return;
    }
    handleOpenMobileSessions();
  }, [handleOpenMobileSessions]);

  const handleOpenMobileFiles = useCallback(() => {
    if (!attachedSessionId) {
      return;
    }
    setMobileMenuOpen(false);
    setMobilePanel("files");
  }, [attachedSessionId]);

  const handleOpenMobileNewSession = useCallback(() => {
    setMobileMenuOpen(false);
    void handleCreateSession();
  }, [handleCreateSession]);

  const handleCloseMobilePanel = useCallback(() => {
    setMobilePanel(undefined);
    requestMobileTerminalFocus();
  }, [requestMobileTerminalFocus]);

  const handleFilesPanelResizePointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (isMobileLayout || !showDesktopFilesPanel) {
        return;
      }
      event.preventDefault();
      filesPanelResizeRef.current = {
        pointerId: event.pointerId,
        startX: event.clientX,
        startWidth: filesPanelWidthRef.current,
      };
      setIsFilesPanelResizing(true);
    },
    [isMobileLayout, showDesktopFilesPanel],
  );

  const handleFilesPanelResizeKeyDown = useCallback((event: React.KeyboardEvent<HTMLDivElement>) => {
    const step = event.shiftKey ? 48 : 16;
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    event.preventDefault();
    const delta = event.key === "ArrowLeft" ? step : -step;
    setFilesPanelWidth((current) => clampFilesPanelWidth(current + delta, window.innerWidth));
  }, []);

  if (activeSurface === "admin") {
    return (
      <I18nProvider locale={effectiveLocale}>
      <div className="admin-shell">
        <header className="admin-topbar">
          <div className="admin-brand">
            <Cable size={18} aria-hidden="true" />
            <span>{t("app.adminTitle")}</span>
          </div>
          <div className="admin-topbar-actions">
            <button type="button" className="icon-button" aria-label={t("app.settings")} onClick={() => setSettingsOpen(true)}>
              <Settings size={16} aria-hidden="true" />
            </button>
            <button type="button" onClick={handleOpenWorkspace} disabled={!canOpenWorkspace}>
              <MonitorUp size={16} aria-hidden="true" />
              {t("app.workspace")}
            </button>
          </div>
        </header>
        <main className="admin-main" aria-label={t("app.adminAria")}>
          <section className="admin-summary-band" aria-label={t("app.selectedDaemonAria")}>
            <div className="admin-summary-main">
              <span>{t("app.selectedDaemon")}</span>
              <strong>{activeDaemonLabel}</strong>
              <code>{activeServer?.url ?? t("app.unpaired")}</code>
            </div>
            <button type="button" onClick={handleOpenWorkspace} disabled={!canOpenWorkspace}>
              <MonitorUp size={16} aria-hidden="true" />
              {t("app.openWorkspace")}
            </button>
          </section>
          {error ? (
            <ProtocolErrorAlert
              error={error}
              onRefresh={hasPairedServer ? handleRetryConnection : undefined}
              refreshing={status === "attaching" || status === "connecting" || status === "listing"}
            />
          ) : null}
          <div className="admin-grid">
            <ConnectionPanel
              url={url}
              token={pairingToken}
              status={status}
              canSaveUrl={hasPairedServer}
              onUrlChange={handleUrlChange}
              onTokenChange={setPairingToken}
              onPair={() => void handlePair()}
              onScanQr={() => setQrScannerOpen(true)}
              onSaveUrl={handleSaveConnectionUrl}
              showUrlEditor={connectionEditorOpen || !activeServer}
            />
            <DaemonManagerPanel
              servers={pairedServerOptions}
              activeServerId={activeServer?.server_id}
              renamingServerId={renamingDaemonId}
              renameDraft={daemonRenameDraft}
              onSelect={(serverId) => void handleSelectServer(serverId)}
              onStartRename={handleStartDaemonRename}
              onRenameDraftChange={setDaemonRenameDraft}
              onSaveRename={(serverId) => void handleSaveDaemonRename(serverId)}
              onCancelRename={handleCancelDaemonRename}
              onForget={(serverId) => void handleForgetDaemon(serverId)}
            />
          </div>
          {qrScannerOpen ? (
            <PairingQrScanner
              onDetected={handleQrDetected}
              onClose={() => setQrScannerOpen(false)}
            />
          ) : null}
          <SettingsDialog
            open={settingsOpen}
            preferences={preferences}
            effectiveLocale={effectiveLocale}
            effectiveTheme={effectiveTheme}
            onPreferencesChange={handlePreferencesChange}
            onClose={() => setSettingsOpen(false)}
          />
        </main>
        <StatusBar status={status} error={error} sessionId={attachedSessionId ?? selectedSessionId} />
      </div>
      </I18nProvider>
    );
  }

  return (
    <I18nProvider locale={effectiveLocale}>
    <div
      className={[
        "app-shell",
        "workspace-surface",
        sidebarCollapsed ? "sidebar-is-collapsed" : "",
        connectionReady ? "connection-ready" : "",
        isFilesPanelResizing ? "files-panel-resizing" : "",
        mobileKeyboardOpen ? "mobile-keyboard-open" : "",
        mobileMenuOpen ? "mobile-menu-open" : "",
        mobilePanel ? `mobile-panel-${mobilePanel}` : "",
      ]
        .filter(Boolean)
        .join(" ")}
      style={appShellStyle}
    >
      {mobileMenuOpen ? (
        <button
          type="button"
          className="mobile-backdrop mobile-menu-backdrop"
          aria-label={t("app.closeMobileMenu")}
          onClick={() => setMobileMenuOpen(false)}
        />
      ) : null}
      <aside className={sidebarCollapsed ? "sidebar collapsed-sidebar" : "sidebar"}>
        {sidebarCollapsed ? (
          <>
            <div className="rail-brand">
              <Cable size={18} aria-hidden="true" />
              <button
                type="button"
                className="icon-button"
                aria-label={t("app.expandSidebar")}
                onClick={() => setSidebarCollapsed(false)}
              >
                <PanelLeftOpen size={16} aria-hidden="true" />
              </button>
            </div>
            {connectionReady ? (
              <>
                <div className="rail-actions">
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t("app.newSession")}
                    onClick={handleCreateSession}
                    disabled={status === "creating"}
                  >
                    <Plus size={16} aria-hidden="true" />
                  </button>
                </div>
                <section className="collapsed-session-list" aria-label={t("app.collapsedSessions")}>
                  {orderedSessions.map((session) => (
                    <button
                      type="button"
                      key={session.session_id}
                      className={[
                        "icon-button",
                        session.session_id === selectedSessionId ? "selected-session-dot" : "",
                        newOutputSessionIds.has(session.session_id) ? "has-new-output" : "",
                      ]
                        .filter(Boolean)
                        .join(" ")}
                      aria-label={
                        newOutputSessionIds.has(session.session_id)
                          ? t("sessions.selectNewOutput", { name: sessionDisplayName(session) })
                          : t("sessions.select", { name: sessionDisplayName(session) })
                      }
                      onClick={() => void handleAttach(session.session_id)}
                    >
                      <MonitorUp size={15} aria-hidden="true" />
                    </button>
                  ))}
                </section>
              </>
            ) : null}
          </>
        ) : (
          <>
            <div className="sidebar-fixed-header">
              <div className="brand-row">
                <div className="brand-title">
                  <Cable size={18} aria-hidden="true" />
                  <span>{t("app.termd")}</span>
                </div>
                <button
                  type="button"
                  className="icon-button sidebar-collapse-toggle"
                  aria-label={t("app.collapseSidebar")}
                  onClick={() => setSidebarCollapsed(true)}
                >
                  <PanelLeftClose size={16} aria-hidden="true" />
                </button>
              </div>
              {!isMobileLayout && connectionReady ? (
                <button
                  type="button"
                  className="session-create-button"
                  aria-label={t("app.newSession")}
                  onClick={handleCreateSession}
                  disabled={status === "creating"}
                >
                  <Plus size={16} aria-hidden="true" />
                  {t("app.newSession")}
                </button>
              ) : null}
            </div>
            {!isMobileLayout && connectionReady ? (
              <div className="sidebar-scroll-region">
                <SessionList
                  sessions={orderedSessions}
                  selectedSessionId={selectedSessionId}
                  newOutputSessionIds={newOutputSessionIds}
                  renamingSessionId={renamingSessionId}
                  renameDraft={renameDraft}
                  canSaveRename={canSaveRename}
                  onAttach={handleAttach}
                  onStartRename={handleStartRename}
                  onRenameDraftChange={setRenameDraft}
                  onSaveRename={handleSaveRename}
                  onCancelRename={handleCancelRename}
                  onClose={handleCloseSession}
                  onReorder={handleReorderSessions}
                />
              </div>
            ) : null}
          </>
        )}
      </aside>
      <main className="workspace">
        <div className="toolbar">
          {showMobileWorkspaceMenu ? (
            <button
              type="button"
              className="icon-button mobile-menu-toggle"
              aria-label={t("app.openMobileMenu")}
              aria-expanded={mobileMenuOpen}
              onClick={handleToggleMobileMenu}
            >
              <Menu size={16} aria-hidden="true" />
            </button>
          ) : null}
          {showMobileWorkspaceMenu ? (
            <button
              type="button"
              className={[
                "toolbar-title toolbar-title-button",
                mobileTitlePullDistance > 0 ? "toolbar-title-pulling" : "",
                mobileTitlePullReady ? "toolbar-title-pull-ready" : "",
                status === "listing" ? "toolbar-title-refreshing" : "",
              ].filter(Boolean).join(" ")}
              style={mobileTitlePullStyle}
              aria-label={t("app.openSessionListFromTitle")}
              aria-expanded={showMobileSessionsPanel}
              onPointerDown={handleMobileTitlePointerDown}
              onPointerMove={handleMobileTitlePointerMove}
              onPointerUp={handleMobileTitlePointerUp}
              onPointerCancel={handleMobileTitlePointerCancel}
              onClick={handleMobileTitleClick}
            >
              <MonitorUp size={16} aria-hidden="true" />
              <span>{toolbarSessionName}</span>
              {toolbarSessionSize ? <small>{toolbarSessionSize}</small> : null}
              {toolbarLatency && toolbarLatencyLevel ? (
                <small
                  className={`toolbar-latency ${toolbarLatencyLevel}`}
                  aria-label={`RTT ${toolbarLatency}`}
                  title={`RTT ${toolbarLatency}`}
                >
                  {toolbarLatency}
                </small>
              ) : null}
              <span className="toolbar-title-pull-indicator" aria-hidden="true">
                <RefreshCcw size={13} />
              </span>
            </button>
          ) : (
            <div className="toolbar-title">
              <MonitorUp size={16} aria-hidden="true" />
              <span>{toolbarSessionName}</span>
              {toolbarSessionSize ? <small>{toolbarSessionSize}</small> : null}
              {toolbarLatency && toolbarLatencyLevel ? (
                <small
                  className={`toolbar-latency ${toolbarLatencyLevel}`}
                  aria-label={`RTT ${toolbarLatency}`}
                  title={`RTT ${toolbarLatency}`}
                >
                  {toolbarLatency}
                </small>
              ) : null}
            </div>
          )}
          {connectionReady && attachedSessionId && !isMobileLayout ? (
            <SessionOperatorsBar
              operators={sessionOperators}
              currentDeviceId={state.device?.device_id}
              sessionId={attachedSessionId}
            />
          ) : null}
          {connectionReady && !isMobileLayout ? (
            <div className="toolbar-actions">
              <button
                type="button"
                className="toolbar-clients-button"
                aria-label={t("app.clients")}
                aria-controls="daemon-clients-popover"
                aria-expanded={clientsOpen}
                onClick={() => setClientsOpen((open) => !open)}
              >
                <UsersRound size={16} aria-hidden="true" />
                {t("app.clients")}
              </button>
              {clientsOpen ? (
                <div className="clients-popover toolbar-clients-popover" id="daemon-clients-popover">
                  <DaemonClientsPanel
                    clients={daemonClients}
                    currentDeviceId={state.device?.device_id}
                    forgettingClientIds={forgettingClientIds}
                    onForgetOfflineClient={handleForgetOfflineClient}
                  />
                </div>
              ) : null}
              <button type="button" className="toolbar-admin-button" onClick={() => handleOpenAdmin()}>
                <Server size={16} aria-hidden="true" />
                {t("app.daemons")}
              </button>
              <button type="button" className="icon-button toolbar-settings-button" aria-label={t("app.settings")} onClick={() => setSettingsOpen(true)}>
                <Settings size={16} aria-hidden="true" />
              </button>
            </div>
          ) : null}
        </div>
        <div
          className={
            [
              isMobileLayout
                ? "workspace-body workspace-body-mobile"
                : filesPanelOpen
                  ? "workspace-body"
                  : "workspace-body files-panel-hidden",
              error ? "has-error" : "",
            ]
              .filter(Boolean)
              .join(" ")
          }
          style={desktopWorkspaceStyle}
        >
          {error ? (
            <ProtocolErrorAlert
              error={error}
              onRefresh={hasPairedServer ? handleRetryConnection : undefined}
              refreshing={status === "attaching" || status === "connecting" || status === "listing"}
            />
          ) : null}
          {connectionReady ? (
            <>
              <TerminalPane
                attached={Boolean(attachedSessionId)}
                sessionSize={attachedSession?.size}
                focusRequest={terminalFocusRequest}
                mobileInputMode={isMobileLayout}
                mobileKeyboardOpen={mobileKeyboardOpen}
                mobileViewportHeight={isMobileLayout ? window.innerHeight : undefined}
                mobileViewportOffsetTop={isMobileLayout ? visualViewportMetrics.offsetTop : undefined}
                theme={effectiveTheme}
                outputResetVersion={terminalOutputResetVersion}
                takeOutput={takeTerminalOutput}
                registerOutputDrain={registerTerminalOutputDrain}
                onOutputResetApplied={handleTerminalOutputResetApplied}
                onTerminalResync={handleTerminalResync}
                onTerminalSeqRendered={handleTerminalSeqRendered}
                onTerminalSizeRendered={handleTerminalSizeRendered}
                mobileShortcuts={preferences.mobileShortcuts}
                onSearch={handleTerminalSearch}
                onInput={handleTerminalInput}
                onResize={handleResize}
                onCursorChange={handleCursorChange}
              />
              {showDesktopFilesPanel ? (
                <>
                  <SessionFilesPanel
                    attachedSessionId={attachedSessionId}
                    activeTab={sessionFilesPanelTab}
                    files={sessionFiles}
                    loading={sessionFilesLoading}
                    error={sessionFilesError}
                    uploadProgress={sessionFileUploadProgress}
                    downloadProgress={sessionFileDownloadProgress}
                    git={sessionGit}
                    gitLoading={sessionGitLoading}
                    gitError={sessionGitError}
                    followTerminalCwd={sessionFilesFollowTerminalCwd}
                    onTabChange={handleSessionFilesPanelTabChange}
                    onOpenDirectory={handleOpenDirectory}
                    onOpenFile={handleOpenFile}
                    onOpenGitFile={handleOpenGitFile}
                    onOpenGitDiff={handleOpenGitDiff}
                    onGitAction={handleSessionGitAction}
                    onGoToPath={handleGoToFilePath}
                    onRefresh={handleRefreshSessionFiles}
                    onRefreshGit={handleRefreshSessionGit}
                    onFollowTerminalCwdChange={handleSessionFilesFollowTerminalCwdChange}
                    onUpload={handleUploadFile}
                    onDownload={handleDownloadFile}
                    onDelete={handleDeleteFile}
                    onHide={handleHideFiles}
                    onResizePointerDown={handleFilesPanelResizePointerDown}
                    onResizeKeyDown={handleFilesPanelResizeKeyDown}
                  />
                </>
              ) : !isMobileLayout ? (
                <aside className="files-rail" aria-label={t("app.filesPanelCollapsed")}>
                  <button type="button" className="icon-button" aria-label={t("app.showFilesPanel")} onClick={() => setFilesPanelOpen(true)}>
                    <PanelRightOpen size={16} aria-hidden="true" />
                  </button>
                </aside>
              ) : null}
            </>
          ) : (
            <div className="terminal-pane" aria-label={t("app.terminalUnavailable")}>
              <div className="terminal-placeholder">{t("app.disconnected")}</div>
            </div>
          )}
        </div>
        {showMobileWorkspaceMenu && mobileMenuOpen ? (
          <nav className="mobile-menu-popover" aria-label={t("app.mobileWorkspaceMenu")}>
            <button type="button" onClick={() => handleOpenAdmin()}>
              <Server size={16} aria-hidden="true" />
              {t("app.daemons")}
            </button>
            <button type="button" onClick={handleOpenMobileSessions}>
              <MonitorUp size={16} aria-hidden="true" />
              {t("app.sessions")}
            </button>
            <button type="button" onClick={handleOpenMobileFiles} disabled={!attachedSessionId}>
              <Folder size={16} aria-hidden="true" />
              {t("app.files")}
            </button>
            <button type="button" onClick={handleOpenMobileNewSession} disabled={status === "creating"}>
              <Plus size={16} aria-hidden="true" />
              {t("app.new")}
            </button>
            <button type="button" onClick={() => setSettingsOpen(true)}>
              <Settings size={16} aria-hidden="true" />
              {t("app.settings")}
            </button>
          </nav>
        ) : null}
        {showMobileSessionsPanel ? (
          <section className="mobile-panel mobile-sessions-panel" aria-label={t("app.sessionsPanel")}>
            <header className="mobile-panel-header">
              <div className="mobile-panel-title">
                <MonitorUp size={15} aria-hidden="true" />
                <span>{t("app.sessions")}</span>
              </div>
              <div className="mobile-panel-actions">
                <button
                  type="button"
                  className="icon-button"
                  aria-label={t("sessions.refresh")}
                  onClick={handleRefresh}
                  disabled={status === "listing"}
                >
                  <RefreshCcw size={15} aria-hidden="true" />
                </button>
                <button type="button" className="icon-button" aria-label={t("sessions.closePanel")} onClick={handleCloseMobilePanel}>
                  <X size={15} aria-hidden="true" />
                </button>
              </div>
            </header>
            <div className="mobile-panel-body">
              <SessionList
                sessions={orderedSessions}
                selectedSessionId={selectedSessionId}
                newOutputSessionIds={newOutputSessionIds}
                renamingSessionId={renamingSessionId}
                renameDraft={renameDraft}
                canSaveRename={canSaveRename}
                onAttach={handleAttach}
                onStartRename={handleStartRename}
                onRenameDraftChange={setRenameDraft}
                onSaveRename={handleSaveRename}
                onCancelRename={handleCancelRename}
                onClose={handleCloseSession}
                onReorder={handleReorderSessions}
              />
            </div>
          </section>
        ) : null}
        {showMobileFilesPanel ? (
          <div className="mobile-panel mobile-files-panel">
            <SessionFilesPanel
              attachedSessionId={attachedSessionId}
              activeTab={sessionFilesPanelTab}
              files={sessionFiles}
              loading={sessionFilesLoading}
              error={sessionFilesError}
              uploadProgress={sessionFileUploadProgress}
              downloadProgress={sessionFileDownloadProgress}
              git={sessionGit}
              gitLoading={sessionGitLoading}
              gitError={sessionGitError}
              followTerminalCwd={sessionFilesFollowTerminalCwd}
              onTabChange={handleSessionFilesPanelTabChange}
              onOpenDirectory={handleOpenDirectory}
              onOpenFile={handleOpenFile}
              onOpenGitFile={handleOpenGitFile}
              onOpenGitDiff={handleOpenGitDiff}
              onGitAction={handleSessionGitAction}
              onGoToPath={handleGoToFilePath}
              onRefresh={handleRefreshSessionFiles}
              onRefreshGit={handleRefreshSessionGit}
              onFollowTerminalCwdChange={handleSessionFilesFollowTerminalCwdChange}
              onUpload={handleUploadFile}
              onDownload={handleDownloadFile}
              onDelete={handleDeleteFile}
              onHide={handleHideFiles}
            />
          </div>
        ) : null}
        <FileEditorDialog
          open={Boolean(fileEditor)}
          path={fileEditor?.path ?? ""}
          name={fileEditor?.name}
          initialText={fileEditor?.text ?? ""}
          loading={fileEditor?.loading}
          saving={fileEditor?.saving}
          error={fileEditor?.error}
          language={languageForPath(fileEditor?.path ?? "")}
          theme={effectiveTheme}
          onSave={handleSaveOpenFile}
          onClose={() => setFileEditor(undefined)}
        />
        <FileEditorDialog
          open={Boolean(diffViewer)}
          path={diffViewer?.path ?? ""}
          name={diffViewer?.name}
          initialText={diffViewer?.text ?? ""}
          loading={diffViewer?.loading}
          error={diffViewer?.error}
          language="diff"
          theme={effectiveTheme}
          readOnly
          onSave={() => undefined}
          onClose={() => setDiffViewer(undefined)}
        />
        <SettingsDialog
          open={settingsOpen}
          preferences={preferences}
          effectiveLocale={effectiveLocale}
          effectiveTheme={effectiveTheme}
          onPreferencesChange={handlePreferencesChange}
          onClose={() => setSettingsOpen(false)}
        />
        <DaemonStatusPanel
          status={daemonStatus}
          cpuHistory={daemonCpuHistory}
          networkRate={daemonNetworkRate}
          loading={daemonStatusLoading}
          error={daemonStatusError}
          compact={isMobileLayout}
        />
      </main>
    </div>
    </I18nProvider>
  );
}

function ProtocolErrorAlert(props: {
  error: SafeError;
  onRefresh?: () => void;
  refreshing?: boolean;
}) {
  const { t } = useI18n();
  return (
    <section className="protocol-error-alert" role="alert" aria-label={t("protocolError.title")}>
      <div className="protocol-error-alert-title">
        <CircleAlert size={17} aria-hidden="true" />
        <span>{t("protocolError.title")}</span>
        {props.onRefresh ? (
          <button
            type="button"
            className="protocol-error-refresh"
            onClick={props.onRefresh}
            disabled={props.refreshing}
          >
            <RefreshCcw size={15} aria-hidden="true" />
            {t("protocolError.retry")}
          </button>
        ) : null}
      </div>
      <div className="protocol-error-alert-detail">
        <code>{props.error.code}</code>
        {/* 主体提示只展示 SafeError 字段，避免把 token、签名或密文等原始 payload 泄漏到 UI。 */}
        <span>{translateSafeErrorMessage(props.error, t)}</span>
      </div>
    </section>
  );
}

function SessionOperatorsBar(props: {
  operators: DaemonClientSummaryPayload[];
  currentDeviceId?: UUID;
  sessionId: UUID;
}) {
  const { t } = useI18n();
  return (
    <div className="session-operators" aria-label={t("operators.aria")}>
      <div className="session-operators-title">
        <UsersRound size={15} aria-hidden="true" />
        <span>{props.operators.length}</span>
      </div>
      {props.operators.length === 0 ? (
        <span className="session-operator muted">{t("operators.empty")}</span>
      ) : (
        props.operators.map((client) => {
          const isCurrentDevice = client.device_id === props.currentDeviceId;
          const label = client.name?.trim() || client.peer_ip || t("operators.client");
          const cursor =
            client.cursor_session_id === props.sessionId && client.cursor_row && client.cursor_col
              ? `${client.cursor_row}:${client.cursor_col}`
              : t("operators.cursorUnknown");
          const focus =
            client.cursor_session_id === props.sessionId && client.cursor_focused !== undefined && client.cursor_focused !== null
              ? client.cursor_focused
                ? t("operators.focused")
                : t("operators.blurred")
              : undefined;
          return (
            <span className="session-operator" key={client.client_id} title={label}>
              <span className="status-dot online" aria-hidden="true" />
              <span>{label}</span>
              {isCurrentDevice ? <span>{t("operators.you")}</span> : null}
              <span className="session-operator-cursor">{cursor}</span>
              {focus ? <span className={client.cursor_focused ? "focus-chip focused" : "focus-chip"}>{focus}</span> : null}
            </span>
          );
        })
      )}
    </div>
  );
}

function DaemonStatusPanel(props: {
  status?: DaemonStatusResultPayload;
  cpuHistory: number[];
  networkRate?: DaemonNetworkRate;
  loading: boolean;
  error?: SafeError;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const memoryValue = props.status
    ? props.compact
      ? `${formatBytesTiny(usedBytes(props.status.memory_total_bytes, props.status.memory_available_bytes))}/${formatBytesTiny(props.status.memory_total_bytes)}`
      : `${formatBytesCompact(usedBytes(props.status.memory_total_bytes, props.status.memory_available_bytes))} / ${formatBytesCompact(props.status.memory_total_bytes)}`
    : "-";
  const diskValue = props.status
    ? props.compact
      ? `${formatBytesTiny(usedBytes(props.status.disk_total_bytes, props.status.disk_available_bytes))}/${formatBytesTiny(props.status.disk_total_bytes)}`
      : `${formatBytesCompact(usedBytes(props.status.disk_total_bytes, props.status.disk_available_bytes))} / ${formatBytesCompact(props.status.disk_total_bytes)}`
    : "-";
  const cpuValue = props.status ? `${props.status.cpu_percent.toFixed(1)}%` : props.loading ? "..." : "-";
  const networkValue = formatNetworkMetric(props.networkRate, Boolean(props.compact));

  return (
    <footer
      className={props.compact ? "daemon-status-panel daemon-status-strip compact" : "daemon-status-panel daemon-status-strip"}
      aria-label={t("daemonStatus.aria")}
      role="contentinfo"
    >
      {props.compact ? null : (
        <header className="daemon-status-header">
          <div className="daemon-status-title">
            <Server size={13} aria-hidden="true" />
            <span>{props.status?.host_name ?? t("daemonStatus.fallbackHost")}</span>
          </div>
        </header>
      )}
      {!props.compact && props.error ? (
        <div className="daemon-status-error">
          <code>{props.error.code}</code>
          <span>{translateSafeErrorMessage(props.error, t)}</span>
        </div>
      ) : null}
      <div className="daemon-status-grid">
        <CpuMetric value={cpuValue} history={props.cpuHistory} />
        <Metric label={t("daemonStatus.memory")} value={memoryValue} className="daemon-status-memory" />
        <Metric label={t("daemonStatus.disk")} value={diskValue} className="daemon-status-disk" />
        <Metric label={t("daemonStatus.network")} value={networkValue} className="daemon-status-network" />
        {props.compact ? null : (
          <Metric
            label={t("daemonStatus.load")}
            value={props.status ? props.status.load_avg.map((value) => value.toFixed(2)).join(" ") : "-"}
            className="daemon-status-load"
          />
        )}
        {props.compact ? null : (
          <Metric label={t("daemonStatus.uptime")} value={props.status ? formatDuration(props.status.uptime_seconds) : "-"} className="daemon-status-uptime" />
        )}
      </div>
    </footer>
  );
}

function CpuMetric(props: { value: string; history: number[] }) {
  const { t } = useI18n();
  return (
    <div className="daemon-status-metric daemon-status-cpu">
      <span>{t("daemonStatus.cpu")}</span>
      <strong>{props.value}</strong>
      <CpuBarChart samples={props.history} />
    </div>
  );
}

function CpuBarChart(props: { samples: number[] }) {
  const { t } = useI18n();
  const bars = cpuBarChartRects(props.samples, CPU_BAR_CHART_WIDTH, CPU_BAR_CHART_HEIGHT, CPU_BAR_CHART_COUNT);
  return (
    <svg
      className="daemon-cpu-bar-chart"
      viewBox={`0 0 ${CPU_BAR_CHART_WIDTH} ${CPU_BAR_CHART_HEIGHT}`}
      role="img"
      aria-label={t("daemonStatus.cpuBars")}
    >
      <rect
        className="daemon-cpu-bar-frame"
        x="0.5"
        y="0.5"
        width={CPU_BAR_CHART_WIDTH - 1}
        height={CPU_BAR_CHART_HEIGHT - 1}
        rx="2"
      />
      {bars.map((bar) => (
        <rect
          className="daemon-cpu-bar"
          key={bar.index}
          x={bar.x}
          y={bar.y}
          width={bar.width}
          height={bar.height}
          rx="0.6"
        />
      ))}
    </svg>
  );
}

function Metric(props: { label: string; value: string; className?: string }) {
  const className = props.className ? `daemon-status-metric ${props.className}` : "daemon-status-metric";
  return (
    <div className={className}>
      <span>{props.label}</span>
      <strong>{props.value}</strong>
    </div>
  );
}

function useMobileLayout(): boolean {
  const getSnapshot = () => {
    if (typeof window === "undefined") {
      return false;
    }
    if (typeof window.matchMedia === "function") {
      return window.matchMedia(MOBILE_LAYOUT_QUERY).matches;
    }
    return window.innerWidth <= MOBILE_LAYOUT_BREAKPOINT;
  };

  const [isMobileLayout, setIsMobileLayout] = useState(getSnapshot);

  useEffect(() => {
    if (typeof window === "undefined") {
      return undefined;
    }

    if (typeof window.matchMedia !== "function") {
      const handleResize = () => setIsMobileLayout(window.innerWidth <= MOBILE_LAYOUT_BREAKPOINT);
      handleResize();
      window.addEventListener("resize", handleResize);
      return () => window.removeEventListener("resize", handleResize);
    }

    const mediaQuery = window.matchMedia(MOBILE_LAYOUT_QUERY);
    const handleChange = () => setIsMobileLayout(mediaQuery.matches);
    handleChange();

    if (typeof mediaQuery.addEventListener === "function") {
      mediaQuery.addEventListener("change", handleChange);
      return () => mediaQuery.removeEventListener("change", handleChange);
    }

    mediaQuery.addListener(handleChange);
    return () => mediaQuery.removeListener(handleChange);
  }, []);

  return isMobileLayout;
}

function useSystemTheme(): "dark" | "light" {
  const getSnapshot = () => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
      return "dark" as const;
    }
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  };

  const [systemTheme, setSystemTheme] = useState<"dark" | "light">(getSnapshot);

  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
      return undefined;
    }
    const mediaQuery = window.matchMedia("(prefers-color-scheme: light)");
    const handleChange = () => setSystemTheme(mediaQuery.matches ? "light" : "dark");
    handleChange();
    if (typeof mediaQuery.addEventListener === "function") {
      mediaQuery.addEventListener("change", handleChange);
      return () => mediaQuery.removeEventListener("change", handleChange);
    }
    mediaQuery.addListener(handleChange);
    return () => mediaQuery.removeListener(handleChange);
  }, []);

  return systemTheme;
}

function useVisualViewportMetrics(enabled: boolean): { height: number; offsetTop: number; keyboardInset: number; keyboardOpen: boolean } {
  const metricsFromWindow = () => {
    if (typeof window === "undefined") {
      return { height: 0, offsetTop: 0, keyboardInset: 0, keyboardOpen: false };
    }
    const viewport = window.visualViewport;
    const height = Math.round(viewport?.height ?? window.innerHeight);
    const offsetTop = Math.round(viewport?.offsetTop ?? 0);
    const keyboardInset = Math.max(0, Math.round(window.innerHeight - height - offsetTop));
    // 地址栏收缩也会改变 visualViewport，高度差超过常见工具栏后才按软键盘处理。
    return { height, offsetTop, keyboardInset, keyboardOpen: keyboardInset >= 80 };
  };
  const [metrics, setMetrics] = useState(metricsFromWindow);

  useEffect(() => {
    if (!enabled || typeof window === "undefined") {
      return undefined;
    }
    const viewport = window.visualViewport;
    const updateMetrics = () =>
      setMetrics((current) => {
        const next = metricsFromWindow();
        return current.height === next.height &&
          current.offsetTop === next.offsetTop &&
          current.keyboardInset === next.keyboardInset &&
          current.keyboardOpen === next.keyboardOpen
          ? current
          : next;
      });
    updateMetrics();
    window.addEventListener("resize", updateMetrics);
    viewport?.addEventListener("resize", updateMetrics);
    viewport?.addEventListener("scroll", updateMetrics);
    return () => {
      window.removeEventListener("resize", updateMetrics);
      viewport?.removeEventListener("resize", updateMetrics);
      viewport?.removeEventListener("scroll", updateMetrics);
    };
  }, [enabled]);

  return metrics.height
    ? metrics
    : { height: typeof window === "undefined" ? 0 : window.innerHeight, offsetTop: 0, keyboardInset: 0, keyboardOpen: false };
}

function clampFilesPanelWidth(width: number, viewportWidth: number): number {
  const viewportCap = Math.max(MIN_FILES_PANEL_WIDTH, Math.min(MAX_FILES_PANEL_WIDTH, viewportWidth - 420));
  return Math.max(MIN_FILES_PANEL_WIDTH, Math.min(width, viewportCap));
}

export function defaultWsUrlFromPage(
  location: (Pick<Location, "protocol" | "host"> & Partial<Pick<Location, "pathname">>) | undefined = globalThis.location,
): string {
  if (!location || !location.host || (location.protocol !== "http:" && location.protocol !== "https:")) {
    return FALLBACK_WS_URL;
  }
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}${websocketPathForPage(location.pathname)}`;
}

export function browserReachableWsUrl(
  rawUrl: string,
  page:
    | (Pick<Location, "protocol" | "host" | "hostname"> & Partial<Pick<Location, "pathname">>)
    | undefined = globalThis.location,
): string {
  try {
    const parsed = new URL(rawUrl);
    // 页面从局域网地址打开时，浏览器里的 127.0.0.1 指向用户自己的机器，需要改用页面来源。
    if (page?.hostname && isLoopbackHost(parsed.hostname) && !isLoopbackHost(page.hostname)) {
      return defaultWsUrlFromPage(page);
    }
    return rawUrl;
  } catch {
    return rawUrl;
  }
}

export function pairingWsUrlCandidates(
  rawUrl: string,
  serverId: UUID,
  page:
    | (Pick<Location, "protocol" | "host" | "hostname"> & Partial<Pick<Location, "pathname">>)
    | undefined = globalThis.location,
): string[] {
  const candidates: string[] = [];
  const inviteUrl = rawUrl.trim();
  const pageUrl = defaultWsUrlFromPage(page);
  const relayToken = relayTokenFromUrl(inviteUrl);

  if (page?.hostname && !isLoopbackHost(page.hostname)) {
    addCandidate(candidates, routeWsUrlForKnownServer(withRelayToken(pageUrl, relayToken), serverId));
  }

  addCandidate(candidates, routeWsUrlForKnownServer(browserReachableWsUrl(inviteUrl, page), serverId));

  return candidates;
}

export async function connectPairingClient(
  candidateUrls: string[],
  routeServerId: UUID,
  deviceId: UUID,
  daemonPublicKey: string,
  timeoutMs = APP_CONNECTION_TIMEOUT_MS,
): Promise<{ client: DirectClient; effectiveUrl: string }> {
  if (!routeServerId) {
    throw new ProtocolClientError("pairing_server_unknown", "pairing requires a known daemon server id");
  }
  let lastError: unknown;
  for (const candidateUrl of candidateUrls) {
    try {
      const client = await DirectClient.connect(candidateUrl, routeServerId, deviceId, {
        expectedDaemonPublicKey: daemonPublicKey,
        timeoutMs,
      });
      if (client.serverId !== routeServerId) {
        client.close();
        lastError = new ProtocolClientError(
          "pairing_payload_server_mismatch",
          "pairing payload does not match the connected daemon",
        );
        continue;
      }
      return { client, effectiveUrl: candidateUrl };
    } catch (caught) {
      lastError = caught;
    }
  }

  throw normalizePairingRouteError(lastError) ??
    new ProtocolClientError("empty_pairing_candidates", "no pairing URL candidates");
}

function normalizePairingRouteError(error: unknown): unknown {
  if (
    error instanceof ProtocolClientError &&
    (error.code === "invalid_route_prelude" || error.code === "route_server_mismatch")
  ) {
    return new ProtocolClientError(
      "pairing_payload_server_mismatch",
      "pairing payload does not match the connected daemon",
    );
  }
  return error;
}

function routeWsUrlForKnownServer(rawUrl: string, serverId: UUID): string | undefined {
  const normalizedUrl = normalizeRouteWsUrl(rawUrl, serverId);
  try {
    const parsed = new URL(normalizedUrl);
    if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
      return undefined;
    }

    const normalizedPath = parsed.pathname.replace(/\/+$/, "");
    if (!normalizedPath.endsWith("/ws")) {
      return undefined;
    }

    parsed.pathname = normalizedPath;
    return parsed.toString();
  } catch {
    return undefined;
  }
}

function websocketPathForPage(pathname: string | undefined): string {
  const rawPath = pathname?.trim() || "/";
  const path = rawPath.startsWith("/") ? rawPath : `/${rawPath}`;
  const basePath = path.endsWith("/")
    ? path.replace(/\/+$/, "")
    : path
        .split("/")
        .slice(0, -1)
        .join("/");
  // Web 被反向代理到 `/prefix/` 时，WS 也应使用同一个公开前缀：`/prefix/ws`。
  return `${basePath || ""}/ws`;
}

function relayTokenFromUrl(rawUrl: string): string | undefined {
  try {
    const token = new URL(rawUrl).searchParams.get("relay_token")?.trim();
    return token || undefined;
  } catch {
    return undefined;
  }
}

function withRelayToken(rawUrl: string, relayToken: string | undefined): string {
  if (!relayToken) {
    return rawUrl;
  }
  try {
    const parsed = new URL(rawUrl);
    if (!parsed.searchParams.has("relay_token")) {
      parsed.searchParams.set("relay_token", relayToken);
    }
    return parsed.toString();
  } catch {
    return rawUrl;
  }
}

function addCandidate(candidates: string[], candidate: string | undefined): void {
  const clean = candidate?.trim();
  if (clean && !candidates.includes(clean)) {
    candidates.push(clean);
  }
}

function isLoopbackHost(hostname: string): boolean {
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "::1" || hostname === "[::1]";
}

function daemonDisplayLabel(server: PairedServerState, index: number, t: Translate): string {
  const name = server.name?.trim();
  if (name) {
    return name;
  }
  try {
    const parsed = new URL(server.url);
    return t("daemons.fallbackHostName", { index: index + 1, host: parsed.host });
  } catch {
    return t("daemons.fallbackName", { index: index + 1 });
  }
}

function daemonAddressForTitle(rawUrl: string): string {
  try {
    const parsed = new URL(rawUrl);
    parsed.search = "";
    parsed.hash = "";
    return parsed.toString();
  } catch {
    return rawUrl.split("?")[0]?.split("#")[0] ?? rawUrl;
  }
}

function terminalSizeDisplay(size: TerminalSize): string {
  return `${size.cols}x${size.rows}`;
}

function maybeNotifyBrowser(
  preferences: BrowserPreferences,
  body: string,
  lastNotificationAtRef: React.MutableRefObject<number>,
): void {
  if (preferences.notifications === "off" || typeof Notification === "undefined" || Notification.permission !== "granted") {
    return;
  }
  const now = Date.now();
  if (now - lastNotificationAtRef.current < 3000) {
    return;
  }
  lastNotificationAtRef.current = now;
  try {
    new Notification("Termd", {
      body,
      tag: "termd-session-activity",
      silent: true,
    });
  } catch {
    // 浏览器通知失败不应影响终端主链路。
  }
}

function formatBytesCompact(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value >= 10 || unitIndex === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[unitIndex]}`;
}

function formatBytesTiny(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0B";
  }
  const units = ["B", "K", "M", "G", "T"];
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value >= 10 || unitIndex === 0 ? value.toFixed(0) : value.toFixed(1)}${units[unitIndex]}`;
}

function usedBytes(totalBytes: number, availableBytes: number): number {
  if (!Number.isFinite(totalBytes) || !Number.isFinite(availableBytes)) {
    return 0;
  }
  return Math.max(0, totalBytes - Math.max(0, availableBytes));
}

function formatNetworkRate(rate: DaemonNetworkRate, compact = false): string {
  if (compact) {
    return `↓${formatBytesTiny(rate.rxBytesPerSecond)} ↑${formatBytesTiny(rate.txBytesPerSecond)}`;
  }
  return `↓${formatBytesPerSecond(rate.rxBytesPerSecond)} ↑${formatBytesPerSecond(rate.txBytesPerSecond)}`;
}

function formatNetworkMetric(rate?: DaemonNetworkRate, compact = false): string {
  return rate ? formatNetworkRate(rate, compact) : "-";
}

function formatLatency(latencyMs?: number): string | undefined {
  if (latencyMs === undefined || !Number.isFinite(latencyMs) || latencyMs < 0) {
    return undefined;
  }
  if (latencyMs < 1000) {
    return `${Math.max(1, Math.round(latencyMs))}ms`;
  }
  if (latencyMs < 10_000) {
    return `${(latencyMs / 1000).toFixed(1)}s`;
  }
  return `${Math.round(latencyMs / 1000)}s`;
}

export function latencyLevelClass(latencyMs?: number): "latency-good" | "latency-warning" | "latency-danger" | undefined {
  if (latencyMs === undefined || !Number.isFinite(latencyMs) || latencyMs < 0) {
    return undefined;
  }
  // RTT 使用固定分档：50ms 内绿色，50-150ms 黄色，超过 150ms 红色，便于标题栏快速扫视。
  const roundedMs = Math.round(latencyMs);
  if (roundedMs <= 50) {
    return "latency-good";
  }
  if (roundedMs <= 150) {
    return "latency-warning";
  }
  return "latency-danger";
}

function formatBytesPerSecond(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond < 0) {
    return "-";
  }
  return `${formatBytesTiny(bytesPerSecond)}/s`;
}

function networkCounterSampleFromStatus(
  status: DaemonStatusResultPayload,
  sampledAtMs: number,
): DaemonNetworkCounterSample | undefined {
  const rxBytes = normalizedNetworkCounter(status.network_rx_bytes);
  const txBytes = normalizedNetworkCounter(status.network_tx_bytes);
  if (rxBytes === undefined || txBytes === undefined) {
    return undefined;
  }
  return { rxBytes, txBytes, sampledAtMs };
}

function normalizedNetworkCounter(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value) || value < 0) {
    return undefined;
  }
  return value;
}

export function networkRateFromSamples(
  previous: DaemonNetworkCounterSample | undefined,
  next: DaemonNetworkCounterSample | undefined,
): DaemonNetworkRate | undefined {
  if (!previous || !next) {
    return undefined;
  }
  const elapsedSeconds = (next.sampledAtMs - previous.sampledAtMs) / 1000;
  if (!Number.isFinite(elapsedSeconds) || elapsedSeconds <= 0) {
    return undefined;
  }
  const rxDelta = next.rxBytes - previous.rxBytes;
  const txDelta = next.txBytes - previous.txBytes;
  // 网卡计数器会在 daemon 重启、网卡重置或溢出时回退；这种采样直接丢弃。
  if (rxDelta < 0 || txDelta < 0) {
    return undefined;
  }
  return {
    rxBytesPerSecond: rxDelta / elapsedSeconds,
    txBytesPerSecond: txDelta / elapsedSeconds,
  };
}

function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) {
    return "-";
  }
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) {
    return `${days}d ${hours}h`;
  }
  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  return `${minutes}m`;
}

function appendCpuSample(samples: number[], sample: number): number[] {
  const boundedSample = Number.isFinite(sample) ? Math.max(0, Math.min(100, sample)) : 0;
  return [...samples, boundedSample].slice(-CPU_HISTORY_LIMIT);
}

function cpuBarChartRects(samples: number[], width: number, height: number, count: number) {
  const padding = 2;
  const gap = 1;
  const innerHeight = height - padding * 2;
  const barWidth = Math.max(1, (width - padding * 2 - gap * (count - 1)) / count);
  const recentSamples = samples.slice(-count);
  const paddedSamples = [...Array(Math.max(0, count - recentSamples.length)).fill(0), ...recentSamples];
  return paddedSamples.map((sample, index) => {
    const boundedSample = Number.isFinite(sample) ? Math.max(0, Math.min(100, sample)) : 0;
    const barHeight = boundedSample <= 0 ? 0 : Math.max(1, (boundedSample / 100) * innerHeight);
    return {
      index,
      x: Number((padding + index * (barWidth + gap)).toFixed(2)),
      y: Number((height - padding - barHeight).toFixed(2)),
      width: Number(barWidth.toFixed(2)),
      height: Number(barHeight.toFixed(2)),
    };
  });
}

function sortSessionsNewestFirst(sessions: SessionSummaryPayload[]): SessionSummaryPayload[] {
  return [...sessions].sort((left, right) => sessionCreatedAt(right) - sessionCreatedAt(left));
}

function mergeSessionRefresh(
  remoteSessions: SessionSummaryPayload[],
  currentSessions: SessionSummaryPayload[],
  preserveSessionIds: Array<UUID | undefined>,
  sessionOrder: UUID[] = [],
): SessionSummaryPayload[] {
  const currentById = new Map(currentSessions.map((session) => [session.session_id, session]));
  const remoteIds = new Set(remoteSessions.map((session) => session.session_id));
  const next: SessionSummaryPayload[] = remoteSessions.map((remote) => {
    const current = currentById.get(remote.session_id);
    return {
      ...remote,
      // 旧的异步刷新可能带回缺字段的列表；本地已有的展示元数据不能因此抖动。
      name: remote.name ?? current?.name ?? null,
      files_path: remote.files_path ?? current?.files_path ?? null,
      created_at_ms: remote.created_at_ms ?? current?.created_at_ms ?? null,
    };
  });

  const preservedIds = new Set<UUID>();
  for (const sessionId of preserveSessionIds) {
    if (!sessionId || remoteIds.has(sessionId)) {
      continue;
    }
    if (preservedIds.has(sessionId)) {
      continue;
    }
    preservedIds.add(sessionId);
    const current = currentById.get(sessionId);
    if (current) {
      // 正在编辑或 attach 的 session 可能被更早发出的旧 session_list 暂时漏掉；
      // 先保留本地行，下一次权威刷新或保存/关闭结果会再收敛。
      next.push(current);
    }
  }

  return orderSessions(sortSessionsNewestFirst(next), sessionOrder);
}

function orderSessions(
  sessions: SessionSummaryPayload[],
  sessionOrder: UUID[],
): SessionSummaryPayload[] {
  if (sessionOrder.length === 0) {
    return sessions;
  }
  const sessionById = new Map(sessions.map((session) => [session.session_id, session]));
  const ordered = sessionOrder
    .map((sessionId) => sessionById.get(sessionId))
    .filter((session): session is SessionSummaryPayload => Boolean(session));
  const orderedIds = new Set(ordered.map((session) => session.session_id));
  // 新 session 还没有用户排序偏好，保留 daemon 刷新后的稳定顺序并放在已排序区前面。
  const unordered = sessions.filter((session) => !orderedIds.has(session.session_id));
  return [...unordered, ...ordered];
}

function applyLocalSessionOrder(
  sessions: SessionSummaryPayload[],
  sessionOrder: UUID[],
): SessionSummaryPayload[] {
  return orderSessions(sessions, sessionOrder);
}

function sessionOrderFromDaemonList(sessions: SessionSummaryPayload[]): UUID[] {
  // session_list 现在由 daemon 按持久化 display_order 返回；刷新时必须把它当权威顺序。
  // 否则另一个客户端或重启后的新顺序会被当前浏览器里的旧数组覆盖。
  return sessions.map((session) => session.session_id);
}

function sessionCreatedAt(session: SessionSummaryPayload): number {
  return session.created_at_ms ?? 0;
}

function joinRemotePath(directory: string, name: string): string {
  const cleanName = name.replace(/^\/+/, "");
  if (!directory || directory === "/") {
    return `/${cleanName}`;
  }
  return `${directory.replace(/\/+$/, "")}/${cleanName}`;
}

function basenameRemotePath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index >= 0 ? trimmed.slice(index + 1) || trimmed : trimmed;
}

function resolveRemoteDirectoryPath(currentDirectory: string, input: string): string {
  const requested = input.trim();
  if (!requested) {
    return normalizeRemotePath(currentDirectory || "/");
  }

  // Web 文件面板里的相对路径按当前浏览目录解析，避免用户在 /tmp 输入 work 时回到 session 启动目录。
  if (requested.startsWith("/")) {
    return normalizeRemotePath(requested);
  }
  return normalizeRemotePath(joinRemotePath(currentDirectory || "/", requested));
}

function normalizeRemotePath(path: string): string {
  const absolute = path.startsWith("/");
  const parts: string[] = [];

  for (const part of path.split("/")) {
    if (!part || part === ".") {
      continue;
    }
    if (part === "..") {
      parts.pop();
      continue;
    }
    parts.push(part);
  }

  if (absolute) {
    return `/${parts.join("/")}`.replace(/\/+$/, "") || "/";
  }
  return parts.join("/") || ".";
}

function remoteParentPath(path: string): string {
  const normalized = normalizeRemotePath(path || "/");
  const index = normalized.lastIndexOf("/");
  if (index <= 0) {
    return "/";
  }
  return normalized.slice(0, index);
}

async function getSessionFileEntry(
  client: DirectClient,
  sessionId: UUID,
  path: string,
): Promise<SessionFileEntryPayload | undefined> {
  const normalized = normalizeRemotePath(path);
  const files = await client.listSessionFiles(sessionId, remoteParentPath(normalized));
  return files.entries.find((entry) => entry.path === normalized);
}

async function readEditableSessionFile(
  client: DirectClient,
  sessionId: UUID,
  path: string,
): Promise<{ path: string; bytes: Uint8Array }> {
  const payload = await client.readSessionFile(sessionId, path, { maxBytes: TEXT_FILE_EDITOR_MAX_BYTES });
  if (payload.size_bytes > TEXT_FILE_EDITOR_MAX_BYTES) {
    throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
  }
  const bytes = sessionDataFromBase64(payload.data_base64);
  if (bytes.byteLength > TEXT_FILE_EDITOR_MAX_BYTES) {
    throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
  }
  if (bytes.includes(0)) {
    throw new ProtocolClientError("binary_file", "binary files cannot be edited in browser");
  }
  return { path: payload.path, bytes };
}

async function downloadSessionFile(
  client: DirectClient,
  sessionId: UUID,
  name: string,
  path: string,
  onProgress?: (receivedBytes: number, sizeBytes: number, completed: boolean) => void,
): Promise<void> {
  const writer = await createDownloadWriter(name);
  if (writer) {
    let completed = false;
    let lastReceivedBytes = 0;
    let lastSizeBytes = 0;
    try {
      await client.downloadSessionFile(sessionId, path, {
        collectBytes: false,
        onChunk: (bytes) => writer.write(bytes),
        onProgress: (receivedBytes, sizeBytes) => {
          lastReceivedBytes = receivedBytes;
          lastSizeBytes = sizeBytes;
          // 中文注释：showSaveFilePicker 的 close 才是真正提交文件；
          // 最后一帧先不显示 100%，避免 close 失败时误导用户已经保存成功。
          if (receivedBytes < sizeBytes) {
            onProgress?.(receivedBytes, sizeBytes, false);
          }
        },
      });
      completed = true;
    } finally {
      // 中文注释：streaming writer 的 close 会提交文件；下载校验失败或网络中断时必须 abort，
      // 否则浏览器可能把半截文件落盘。
      if (completed) {
        try {
          await writer.close();
        } catch (error) {
          // 中文注释：close 失败表示浏览器提交文件失败；尽量 abort 让底层 writer 回滚，
          // 但保留原始 close 错误给调用方展示。
          if (writer.abort) {
            try {
              await writer.abort();
            } catch {
              // 保留 close 的原始错误。
            }
          }
          throw error;
        }
        onProgress?.(lastSizeBytes || lastReceivedBytes, lastSizeBytes || lastReceivedBytes, true);
      } else if (writer.abort) {
        try {
          await writer.abort();
        } catch {
          // 中文注释：abort 是清理动作；下载/写入的原始错误才是用户需要看到的失败原因。
        }
      }
    }
    return;
  }

  const entry = await getSessionFileEntry(client, sessionId, path);
  if (!entry || entry.kind !== "file") {
    throw new ProtocolClientError("file_not_found", "file not found");
  }
  if (entry.size_bytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
    throw new ProtocolClientError("file_too_large", "browser streaming download is unavailable for this file");
  }
  const chunks: Uint8Array[] = [];
  let lastReceivedBytes = 0;
  let lastSizeBytes = entry.size_bytes;
  await client.downloadSessionFile(sessionId, path, {
    collectBytes: false,
    onChunk: (bytes, receivedBytes, sizeBytes) => {
      if (sizeBytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES || receivedBytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
        throw new ProtocolClientError("file_too_large", "browser streaming download is unavailable for this file");
      }
      chunks.push(bytes);
    },
    onProgress: (receivedBytes, sizeBytes) => {
      lastReceivedBytes = receivedBytes;
      lastSizeBytes = sizeBytes;
      if (receivedBytes < sizeBytes) {
        onProgress?.(receivedBytes, sizeBytes, false);
      }
    },
  });
  triggerBrowserDownload(name, concatUint8Arrays(chunks));
  onProgress?.(lastSizeBytes || lastReceivedBytes, lastSizeBytes || lastReceivedBytes, true);
}

function concatUint8Arrays(chunks: Uint8Array[]): Uint8Array {
  const length = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

type DownloadWriter = {
  write: (bytes: Uint8Array) => Promise<void>;
  close: () => Promise<void>;
  abort?: () => Promise<void>;
};

async function createDownloadWriter(name: string): Promise<DownloadWriter | undefined> {
  const picker = (globalThis as {
    showSaveFilePicker?: (options?: { suggestedName?: string }) => Promise<{
      createWritable: () => Promise<{
        write: (data: Uint8Array) => Promise<void>;
        close: () => Promise<void>;
        abort?: () => Promise<void>;
      }>;
    }>;
  }).showSaveFilePicker;
  if (!picker) {
    return undefined;
  }
  let handle: Awaited<ReturnType<NonNullable<typeof picker>>>;
  try {
    handle = await picker({ suggestedName: name || "download" });
  } catch (caught) {
    if (caught instanceof DOMException && caught.name === "AbortError") {
      throw new ProtocolClientError("download_cancelled", "download was cancelled");
    }
    throw caught;
  }
  let writable: Awaited<ReturnType<typeof handle.createWritable>>;
  try {
    writable = await handle.createWritable();
  } catch (caught) {
    if (caught instanceof DOMException && caught.name === "AbortError") {
      throw new ProtocolClientError("download_cancelled", "download was cancelled");
    }
    // 中文注释：用户已选择保存目标后，createWritable 失败属于真实保存失败；
    // 不能静默改走内存下载，否则 UI 会显示完成但文件没有写到用户选择的位置。
    throw caught;
  }
  return {
    write: (bytes) => writable.write(bytes),
    close: () => writable.close(),
    abort: writable.abort ? () => writable.abort!() : undefined,
  };
}

function concatByteChunks(chunks: Uint8Array[]): Uint8Array {
  const length = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

function yieldToEventLoop(): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, 0));
}

function triggerBrowserDownload(name: string, bytes: Uint8Array): void {
  if (typeof navigator !== "undefined" && navigator.userAgent.toLowerCase().includes("jsdom")) {
    return;
  }
  if (typeof URL.createObjectURL !== "function") {
    return;
  }
  const buffer = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(buffer).set(bytes);
  const blob = new Blob([buffer], { type: "application/octet-stream" });
  const href = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = href;
  link.download = name || "download";
  link.style.display = "none";
  document.body.append(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(href);
}

function languageForPath(path: string): string | undefined {
  const extension = path.split(".").pop()?.toLowerCase();
  switch (extension) {
    case "js":
    case "jsx":
      return "javascript";
    case "ts":
    case "tsx":
      return "typescript";
    case "json":
      return "json";
    case "rs":
      return "rust";
    case "md":
      return "markdown";
    case "py":
      return "python";
    case "sh":
    case "bash":
      return "shell";
    case "css":
      return "css";
    case "html":
      return "html";
    case "yml":
    case "yaml":
      return "yaml";
    default:
      return undefined;
  }
}

function upsertSession(
  current: SessionSummaryPayload[],
  session: SessionCreatedPayload,
  sessionOrder: UUID[] = [],
): SessionSummaryPayload[] {
  const next = {
    session_id: session.session_id,
    name: session.name ?? null,
    state: session.state,
    size: session.size,
    created_at_ms: Date.now(),
  };
  return orderSessions(
    sortSessionsNewestFirst([next, ...current.filter((candidate) => candidate.session_id !== session.session_id)]),
    sessionOrder,
  );
}

function upsertAttachedSession(
  current: SessionSummaryPayload[],
  attached: SessionAttachedPayload,
  sessionOrder: UUID[] = [],
): SessionSummaryPayload[] {
  const existing = current.find((candidate) => candidate.session_id === attached.session_id);
  const next: SessionSummaryPayload = {
    session_id: attached.session_id,
    name: existing?.name ?? null,
    state: attached.state,
    size: attached.size,
    files_path: existing?.files_path ?? null,
    created_at_ms: existing?.created_at_ms ?? null,
  };
  return orderSessions(
    [next, ...current.filter((candidate) => candidate.session_id !== attached.session_id)],
    sessionOrder,
  );
}

function sameTerminalSize(a: TerminalSize, b: TerminalSize): boolean {
  return (
    a.rows === b.rows &&
    a.cols === b.cols &&
    a.pixel_width === b.pixel_width &&
    a.pixel_height === b.pixel_height
  );
}

function terminalSizeKey(sessionId: UUID, size: TerminalSize): string {
  return `${sessionId}:${size.rows}:${size.cols}:${size.pixel_width}:${size.pixel_height}`;
}
