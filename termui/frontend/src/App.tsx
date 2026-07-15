import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from "react";
import packageMetadata from "../package.json";
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
import { V070Client } from "./protocol/v070-client";
import { ProtocolClientError, toSafeError } from "./protocol/errors";
import { migrateDeviceCertificate, pairDeviceOverHttp } from "./protocol/pairing-client";
import { parsePairingQrPayload } from "./protocol/pairing-payload";
import type {
  BrowserState,
  DaemonClientSummaryPayload,
  DaemonStatusResultPayload,
  EffectiveTheme,
  PairedServerState,
  SafeError,
  SessionCreatedPayload,
  SessionAttachedPayload,
  SessionFileEntryPayload,
  SessionGitFileChangePayload,
  SessionGitWorktreePayload,
  SessionSearchResultPayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "./protocol/types";
import {
  defaultServer,
  DEFAULT_BROWSER_PREFERENCES,
  ensureDevice,
  loadBrowserState,
  normalizeRouteWsUrl,
  forgetDaemon,
  recordPairing,
  recordDeviceCertificate,
  recordServerUrl,
  renameDaemon,
  saveBrowserPreferences,
  selectDefaultServer,
} from "./state/browser-state";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { CollapsedSessionButton, SessionList } from "./components/SessionList";
import { StatusBar } from "./components/StatusBar";
import { TerminalPane } from "./components/TerminalPane";
import type { TerminalOutputItem, TerminalResyncOptions } from "./components/terminal/types";
import { useWorkspaceAutoRetry, useWorkspaceConnection } from "./hooks/useWorkspaceConnection";
import {
  useTerminalAttach,
  useTerminalReceiveLoop,
  useTerminalReconnectScheduler,
} from "./hooks/useTerminalAttach";
import {
  useSessionFiles,
  useSessionFileEditor,
  useSessionFileLoaders,
  useSessionMutationActions,
  useSessionFilesPanelActions,
  useSessionGitDiffViewer,
} from "./hooks/useSessionFiles";
import { sessionDisplayName } from "./session-names";
import { createTranslator, I18nProvider, resolveLocale, translateSafeErrorMessage, useI18n, type Translate } from "./i18n";
import { resolveTheme } from "./theme";
import type { BrowserPreferences } from "./protocol/types";
import { recordTermdDiagnostic } from "./diagnostics";
import { displayUrlWithoutQueryOrFragment, stripSensitiveUrlParts } from "./protocol/url";

const DaemonClientsPanel = lazy(() => import("./components/DaemonClientsPanel").then((module) => ({ default: module.DaemonClientsPanel })));
const DaemonManagerPanel = lazy(() => import("./components/DaemonManagerPanel").then((module) => ({ default: module.DaemonManagerPanel })));
const SessionFilesPanel = lazy(() => import("./components/SessionFilesPanel").then((module) => ({ default: module.SessionFilesPanel })));
const FileEditorDialog = lazy(() => import("./components/FileEditorDialog").then((module) => ({ default: module.FileEditorDialog })));
const PairingQrScanner = lazy(() => import("./components/PairingQrScanner").then((module) => ({ default: module.PairingQrScanner })));
const SettingsDialog = lazy(() => import("./components/SettingsDialog").then((module) => ({ default: module.SettingsDialog })));

function LazyPanelFallback({ className = "panel" }: { className?: string }) {
  // 中文注释：冷路径 chunk 加载通常很短；fallback 只占位，避免闪出无意义文案。
  return <div className={className} aria-hidden="true" />;
}

function LazyModalFallback({ className }: { className: string }) {
  return (
    <div className="modal-backdrop" role="presentation" aria-hidden="true">
      <div className={className} />
    </div>
  );
}

const FALLBACK_WS_URL = "ws://127.0.0.1:8765/ws";
const DEFAULT_SESSION_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
const DEFAULT_FILES_PANEL_WIDTH = 286;
const MIN_FILES_PANEL_WIDTH = 240;
const MAX_FILES_PANEL_WIDTH = 640;
const CONNECTION_AUTO_RETRY_DELAY_MS = 1500;
const CONNECTION_AUTO_RETRY_LIMIT = 3;
const ATTACH_RECONNECT_DELAYS_MS = [250, 1000, 2500, 5000, 10000, 20000];
const ATTACH_SWITCH_COALESCE_DELAY_MS = 80;
const DAEMON_METADATA_RETRY_DELAY_MS = 1500;
const TEXT_FILE_EDITOR_MAX_BYTES = 1024 * 1024;
const FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES = 16 * 1024 * 1024;
const TERMINAL_INPUT_BUFFER_MAX_BYTES = 1024 * 1024;
const TERMINAL_INPUT_CHUNK_MAX_BYTES = 64 * 1024;
const MOBILE_LAYOUT_QUERY = "(max-width: 760px)";
const MOBILE_LAYOUT_BREAKPOINT = 760;
const MOBILE_TITLE_PULL_START_PX = 8;
const MOBILE_TITLE_PULL_REFRESH_PX = 52;
const MOBILE_TITLE_PULL_MAX_PX = 72;
const CPU_HISTORY_LIMIT = 48;
const CPU_BAR_CHART_WIDTH = 56;
const CPU_BAR_CHART_HEIGHT = 18;
const CPU_BAR_CHART_COUNT = 18;
export const APP_VERSION = packageMetadata.version;
export const DAEMON_LATENCY_POLL_INTERVAL_MS = 1000;
// 普通前端操作走同一条可靠 WebSocket；relay 下终端输出可能排在 RPC 响应前面。
// 5 秒给公网 relay 和浏览器调度留出缓冲，避免把短暂排队误报成“操作超时”。
export const APP_CONNECTION_TIMEOUT_MS = 5000;
// WebSocket 新建连接偶发卡住时，不能把整个 session attach 挂到 15 秒。
// terminal snapshot 仍有自己的 attach timeout；这里只约束 socket/route/hello 建连阶段。
const APP_SOCKET_CONNECT_TIMEOUT_MS = 3000;
// 中文注释：真实故障里慢点出现在 socket open 的 TCP/TLS/WebSocket 阶段。
// 该阶段单独快速失败并重试，避免一次半卡住的 TLS 握手拖慢整个 relay attach。
const APP_SOCKET_OPEN_TIMEOUT_MS = 1200;
const APP_SOCKET_OPEN_HEDGE_DELAY_MS = 300;
const APP_SOCKET_CONNECT_ATTEMPTS = 4;
const APP_SOCKET_CONNECT_RETRY_DELAY_MS = 80;
const TERMINAL_LIVENESS_TIMEOUT_MS = 1200;
const PAIRING_CONNECTION_TIMEOUT_MS = 5000;
const ATTACH_CONNECTION_TIMEOUT_MS = 15000;
type AppSurface = "admin" | "workspace";

interface AttachUiOptions {
  closeMobilePanel?: boolean;
  preservePendingInput?: boolean;
}

interface MobileTitlePullGesture {
  pointerId: number;
  startX: number;
  startY: number;
  dragging: boolean;
}

export function isExclusiveMetadataClient(
  client: V070Client | undefined,
  currentMetadataClient: V070Client | undefined,
  workspaceClient: V070Client | undefined,
  attachClient: V070Client | undefined,
): client is V070Client {
  return Boolean(
    client &&
    currentMetadataClient === client &&
    workspaceClient !== client &&
    attachClient !== client
  );
}

interface PendingTerminalInputChunk {
  data: string;
  byteLength: number;
}

interface PendingTerminalInputQueue {
  sessionId: UUID;
  chunks: PendingTerminalInputChunk[];
  byteLength: number;
  flushPromise?: Promise<void>;
}

function utf8CodePointByteLength(value: string): number {
  const codePoint = value.codePointAt(0) ?? 0;
  if (codePoint <= 0x7f) {
    return 1;
  }
  if (codePoint <= 0x7ff) {
    return 2;
  }
  if (codePoint <= 0xffff) {
    return 3;
  }
  return 4;
}

function boundedTerminalInputChunks(data: string, maxBytes: number): {
  chunks: PendingTerminalInputChunk[];
  byteLength: number;
  overflowed: boolean;
} {
  const chunks: PendingTerminalInputChunk[] = [];
  let currentCharacters: string[] = [];
  let currentBytes = 0;
  let acceptedBytes = 0;
  let overflowed = false;
  const flushCurrent = () => {
    if (currentCharacters.length === 0) {
      return;
    }
    chunks.push({ data: currentCharacters.join(""), byteLength: currentBytes });
    currentCharacters = [];
    currentBytes = 0;
  };

  for (const character of data) {
    const characterBytes = utf8CodePointByteLength(character);
    if (acceptedBytes + characterBytes > maxBytes) {
      overflowed = true;
      break;
    }
    if (currentBytes + characterBytes > TERMINAL_INPUT_CHUNK_MAX_BYTES) {
      flushCurrent();
    }
    currentCharacters.push(character);
    currentBytes += characterBytes;
    acceptedBytes += characterBytes;
  }
  flushCurrent();
  return { chunks, byteLength: acceptedBytes, overflowed };
}

const RETRYABLE_CONNECTION_ERROR_CODES = new Set([
  "connection_closed",
  "connection_error",
  "connect_timeout",
  "response_timeout",
  "route_prelude_timeout",
  "relay_daemon_offline",
  "relay_state_unavailable",
  "relay_tunnel_failed",
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

const IGNORED_CLOSING_SESSION_ERROR_CODES = new Set([
  "session_not_found",
]);

const IGNORED_CLOSED_SESSION_ERROR_CODES = new Set([
  "session_not_found",
  "connection_closed",
  "stale_connection",
  "receive_interrupted",
]);

function isLocallySupersededConnectionError(caught: unknown): boolean {
  return LOCALLY_SUPERSEDED_CONNECTION_ERROR_CODES.has(toSafeError(caught).code);
}

function isBackgroundStatusTransientError(caught: unknown): boolean {
  const code = toSafeError(caught).code;
  return code === "response_timeout" || isLocallySupersededConnectionError(caught);
}

function isTerminalSidecarTimeout(caught: unknown): boolean {
  return toSafeError(caught).code === "response_timeout";
}

function isTerminalSidecarTransientError(caught: unknown): boolean {
  const safeError = toSafeError(caught);
  if (safeError.code === "response_timeout") {
    return true;
  }
  if (safeError.code === "http_file_transfer_failed") {
    // 中文注释：真实 relay/浏览器在 HTTP control 瞬断时，不一定抛 fetch TypeError；
    // 旁路网关、半开连接或空响应也可能被传输层稳定归一成这个错误码。
    // 对 resize/cursor 这类终端 sidecar，这仍只代表本次辅助 ack 失败，不能卸载当前 xterm。
    return true;
  }
  // 中文注释：真实浏览器在 relay/HTTP 控制面瞬断时，fetch 往往只给 TypeError，
  // `toSafeError()` 会把它归成 client_error。对于 resize/cursor 这类终端辅助 sidecar，
  // 这种瞬时 transport 失败只能丢掉本次辅助 ack，不能升级成全局 Connection error。
  return safeError.code === "client_error" && safeError.message === "Failed to fetch";
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

function isDocumentAnimationFrameUnsafe(): boolean {
  return typeof document !== "undefined" && (
    document.visibilityState === "hidden" ||
    (typeof document.hasFocus === "function" && !document.hasFocus())
  );
}

interface DeferredTerminalFrameTestHook {
  schedule: (callback: () => void) => number;
  cancel: (handle: number) => void;
}

function terminalOutputFlushFrameTestHook(): DeferredTerminalFrameTestHook | undefined {
  return (globalThis as {
    __TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__?: DeferredTerminalFrameTestHook;
  }).__TERMD_TEST_HOLD_TERMINAL_OUTPUT_FLUSH_RAF__;
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
  const sessionsRef = useRef<SessionSummaryPayload[]>([]);
  const [sessionOrder, setSessionOrder] = useState<UUID[]>([]);
  const sessionOrderRef = useRef<UUID[]>([]);
  const sessionOrderGenerationRef = useRef(0);
  const pendingSessionReorderRef = useRef(false);
  const terminalCreateOwnsAttachRef = useRef(false);
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
  const [metadataReady, setMetadataReady] = useState(false);
  const [metadataRetryNonce, setMetadataRetryNonce] = useState(0);
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [attachedSessionId, setAttachedSessionId] = useState<UUID | undefined>();
  const [renamingSessionId, setRenamingSessionId] = useState<UUID | undefined>();
  const [renameDraft, setRenameDraft] = useState("");
  const [renameOriginalName, setRenameOriginalName] = useState("");
  const [terminalOutputResetVersion, setTerminalOutputResetVersion] = useState(0);
  const [terminalFocusRequest, setTerminalFocusRequest] = useState(0);
  const sessionFilesController = useSessionFiles();
  const terminalAttachController = useTerminalAttach();
  const activeServer = useMemo<PairedServerState | undefined>(() => defaultServer(state), [state]);
  const resolveWorkspaceRouteUrls = useCallback(
    (server: PairedServerState) => knownServerWsUrlCandidates(server.url, server.server_id),
    [],
  );
  const handleBrokenAttachedClient = useCallback(
    (client: V070Client, caught: unknown) => terminalAttachController.attachReconnectHandlerRef.current(client, caught),
    [terminalAttachController.attachReconnectHandlerRef],
  );
  const handleDeviceCertificateMigrated = useCallback(async (
    serverId: UUID,
    deviceCertificate: string,
  ) => {
    const nextState = await recordDeviceCertificate(serverId, deviceCertificate);
    setState(nextState);
  }, []);
  const workspaceConnection = useWorkspaceConnection({
    activeServer,
    device: state.device,
    attachedSessionRef: terminalAttachController.attachedSessionRef,
    pendingTerminalAttachSessionRef: terminalAttachController.pendingTerminalAttachSessionRef,
    receiveLoopActiveRef: terminalAttachController.receiveLoopActiveRef,
    receiveLoopGenerationRef: terminalAttachController.receiveLoopGenerationRef,
    isTerminalTransportPaused,
    isRetryableConnectionError,
    resolveServerRouteUrls: resolveWorkspaceRouteUrls,
    onBrokenAttachedClient: handleBrokenAttachedClient,
    onDeviceCertificateMigrated: handleDeviceCertificateMigrated,
    requestTimeoutMs: APP_CONNECTION_TIMEOUT_MS,
    defaultWorkspaceTimeoutMs: ATTACH_CONNECTION_TIMEOUT_MS,
    socketConnectTimeoutMs: APP_SOCKET_CONNECT_TIMEOUT_MS,
    socketOpenTimeoutMs: APP_SOCKET_OPEN_TIMEOUT_MS,
    socketOpenHedgeDelayMs: APP_SOCKET_OPEN_HEDGE_DELAY_MS,
    socketConnectAttempts: APP_SOCKET_CONNECT_ATTEMPTS,
    socketConnectRetryDelayMs: APP_SOCKET_CONNECT_RETRY_DELAY_MS,
  });
  const {
    sessionFiles,
    setSessionFiles,
    sessionFilesLoading,
    setSessionFilesLoading,
    sessionFilesError,
    setSessionFilesError,
    sessionFilesFollowTerminalCwd,
    setSessionFileUploadProgress,
    setSessionFileDownloadProgress,
    sessionFilesPanelTab,
    setSessionFilesPanelTab,
    sessionGit,
    setSessionGit,
    sessionGitLoading,
    setSessionGitLoading,
    sessionGitError,
    setSessionGitError,
    fileEditor,
    setFileEditor,
    diffViewer,
    setDiffViewer,
    sessionFilesFollowTerminalCwdRef,
    sessionFilesLastManualPathRef,
    activeUploadTransferIdRef,
    activeDownloadTransferIdRef,
    visibleProgressForSession,
    nextFileTransferId,
    clearSessionFileUploadProgressTimer,
    clearSessionFileDownloadProgressTimer,
    updateUploadProgressForTransfer,
    updateDownloadProgressForTransfer,
    scheduleUploadProgressClear,
    scheduleDownloadProgressClear,
    handleSessionFilesFollowTerminalCwdChange,
    clearFileTransferProgressTimers,
    clearSessionFilesState,
  } = sessionFilesController;
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
  // 每个 workspace 固定复用一条 metadata WebSocket；当前 session 的 PTY 数据只走
  // 另一条 terminal WebSocket。HTTP 控制请求复用同一个 Access Token。
  const {
    attachClientRef,
    pendingAttachClientRef,
    workspaceClientPromiseRef,
    workspaceClientRef,
    claimAttachClient,
    connectionAutoRetryTimerRef,
    closeAttachClient,
    closeWorkspaceMetadataClient,
    closeWorkspaceClient,
    authenticatedClient,
    authenticatedWorkspaceClient,
    authenticatedSessionClient,
    resolveSessionClient,
    openSessionOperationClient,
  } = workspaceConnection;
  const {
    pendingTerminalAttachSessionRef,
    pendingTerminalAttachAbortControllerRef,
    attachedSessionRef,
    autoAttachAttemptedSessionRef,
    attachingSessionIdRef,
    attachRequestIdRef,
    sessionCreateRequestIdRef,
    attachSwitchTimerRef,
    attachSwitchGenerationRef,
    reattachCurrentSessionOnOpenRef,
    userDetachedRef,
    pendingResizeKeyRef,
    confirmedSessionSizesRef,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    terminalOutputQueueRef,
    lastRenderedTerminalSeqRef,
    terminalOutputResetVersionRef,
    terminalOutputAppliedResetVersionRef,
    terminalOutputResetWaitersRef,
    terminalOutputFlushFrameRef,
    terminalOutputFlushTimerRef,
    terminalOutputDrainRef,
    terminalSnapshotRevealHistoryTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    attachReconnectTimerRef,
    attachReconnectKeyRef,
    attachReconnectAttemptsRef,
    attachReconnectLastErrorRef,
    attachReconnectHandlerRef,
  } = terminalAttachController;
  const mobileTitlePullGestureRef = useRef<MobileTitlePullGesture | undefined>(undefined);
  const suppressMobileTitleClickRef = useRef(false);
  const closingSessionIdsRef = useRef<Set<UUID>>(new Set());
  const closedSessionIdsRef = useRef<Set<UUID>>(new Set());
  const forgettingClientIdsRef = useRef<Set<UUID>>(new Set());
  const renamingSessionIdRef = useRef<UUID | undefined>(undefined);
  const filesPanelWidthRef = useRef(DEFAULT_FILES_PANEL_WIDTH);
  const filesPanelResizeRef = useRef<{
    pointerId: number;
    startX: number;
    startWidth: number;
  } | null>(null);
  const urlTouchedRef = useRef(false);
  const autoCheckedServerRef = useRef<UUID | undefined>(undefined);
  const selectedSessionIdRef = useRef<UUID | undefined>(undefined);
  const activeSurfaceRef = useRef<AppSurface>(activeSurface);
  const statusRef = useRef(status);
  const daemonNetworkSampleRef = useRef<DaemonNetworkCounterSample | undefined>(undefined);
  const daemonStatusRefreshInFlightRef = useRef(false);
  const pendingTerminalInputQueueRef = useRef<PendingTerminalInputQueue | undefined>(undefined);
  const retryConnectionHandlerRef = useRef<(() => Promise<void> | undefined) | undefined>(undefined);
  const daemonStatusRequestSeqRef = useRef(0);
  const metadataClientRef = useRef<V070Client | undefined>(undefined);
  const metadataClientAbortControllerRef = useRef<AbortController | undefined>(undefined);
  const metadataClientGenerationRef = useRef(0);
  const metadataRetryTimerRef = useRef<number | undefined>(undefined);
  const retryConnectionTaskRef = useRef<Promise<void> | undefined>(undefined);
  const terminalTransportFrozenRef = useRef(false);
  const terminalWasHiddenRef = useRef(false);
  const terminalResumePendingRef = useRef(false);
  const terminalResumeTaskRef = useRef<Promise<void> | undefined>(undefined);
  const terminalResumeMountedRef = useRef(false);
  const lastNotificationAtRef = useRef(0);
  const fileEditorResetRef = useRef<() => void>(() => {});
  const isMobileLayout = useMobileLayout();
  const mobileTerminalInputMode = useMobileTerminalInputMode(isMobileLayout);
  const visualViewportMetrics = useVisualViewportMetrics(mobileTerminalInputMode && activeSurface === "workspace");
  const systemTheme = useSystemTheme();
  const preferences = state.preferences ?? DEFAULT_BROWSER_PREFERENCES;
  const effectiveTheme = resolveTheme(preferences.theme, systemTheme);
  const effectiveLocale = resolveLocale(preferences.language);
  const t = useMemo(() => createTranslator(effectiveLocale), [effectiveLocale]);
  const visibleFileTransferProgress = visibleProgressForSession(attachedSessionId);
  const clearSessionFiles = useCallback(() => {
    fileEditorResetRef.current();
    clearSessionFilesState();
  }, [clearSessionFilesState]);

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
    terminalResumeMountedRef.current = true;
    return () => {
      terminalResumeMountedRef.current = false;
      terminalResumePendingRef.current = false;
    };
  }, []);

  useEffect(() => {
    return () => {
      if (terminalOutputFlushFrameRef.current !== undefined) {
        const flushFrameTestHook = terminalOutputFlushFrameTestHook();
        if (flushFrameTestHook) {
          flushFrameTestHook.cancel(terminalOutputFlushFrameRef.current);
        } else {
          window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
        }
        terminalOutputFlushFrameRef.current = undefined;
      }
      if (connectionAutoRetryTimerRef.current !== undefined) {
        window.clearTimeout(connectionAutoRetryTimerRef.current);
        connectionAutoRetryTimerRef.current = undefined;
      }
      if (terminalOutputFlushTimerRef.current !== undefined) {
        window.clearTimeout(terminalOutputFlushTimerRef.current);
        terminalOutputFlushTimerRef.current = undefined;
      }
      if (attachReconnectTimerRef.current !== undefined) {
        window.clearTimeout(attachReconnectTimerRef.current);
        attachReconnectTimerRef.current = undefined;
      }
      clearFileTransferProgressTimers();
      closeWorkspaceClient();
    };
  }, [clearFileTransferProgressTimers, closeWorkspaceClient]);

  useEffect(() => {
    if (!workspaceClientRef.current || activeSurface === "workspace") {
      return;
    }
    // 中文注释：空工作台保留已认证 WebSocket，New session 可直接提升它为 terminal
    // transport，避免 relay 再做一次 route/hello/auth。离开 workspace 时才回收。
    closeWorkspaceMetadataClient();
  }, [activeSurface, closeWorkspaceMetadataClient, workspaceClientRef]);

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
    sessionsRef.current = sessions;
  }, [sessions]);

  const clearTerminalSnapshotRevealHistory = useCallback((sessionId?: UUID, snapshotToken?: number) => {
    if (sessionId) {
      const revealToken = terminalSnapshotRevealHistoryTokensRef.current.get(sessionId);
      if (snapshotToken === undefined || revealToken === snapshotToken) {
        terminalSnapshotRevealHistoryTokensRef.current.delete(sessionId);
      }
      const pendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
      if (snapshotToken === undefined || pendingSnapshot?.token === snapshotToken) {
        terminalSnapshotPendingFullSnapshotTokensRef.current.delete(sessionId);
      }
      return;
    }
    terminalSnapshotRevealHistoryTokensRef.current.clear();
    terminalSnapshotPendingFullSnapshotTokensRef.current.clear();
  }, [terminalSnapshotPendingFullSnapshotTokensRef, terminalSnapshotRevealHistoryTokensRef]);

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

  const activeServerIdRef = useRef<UUID | undefined>(activeServer?.server_id);
  useEffect(() => {
    activeServerIdRef.current = activeServer?.server_id;
  }, [activeServer?.server_id]);
  const hasPairedServer = Boolean(activeServer && state.device);
  const showConnectionStatus = hasPairedServer && !error && status !== "pairing";
  // session 列表刷新只是旁路请求，不能把正在显示的 xterm 卸载成 disconnected。
  const connectionReady = showConnectionStatus && status !== "idle" && status !== "connecting";
  useEffect(() => {
    recordTermdDiagnostic("app_connection_state", {
      status,
      activeSurface,
      connectionReady,
      showConnectionStatus,
      hasPairedServer,
      attachedSessionId,
      selectedSessionId,
      errorCode: error?.code,
    });
  }, [activeSurface, attachedSessionId, connectionReady, error?.code, hasPairedServer, selectedSessionId, showConnectionStatus, status]);
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

    // 浏览器标题只使用 daemon 地址和当前 session 名称，不复制 URL query/fragment。
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
  const mobileKeyboardOpen = mobileTerminalInputMode && activeSurface === "workspace" && visualViewportMetrics.keyboardOpen;
  const appShellStyle = isMobileLayout
    ? ({
        "--termd-layout-viewport-width": `${visualViewportMetrics.width}px`,
        "--termd-visual-viewport-width": `${visualViewportMetrics.width}px`,
        "--termd-layout-viewport-height": `${visualViewportMetrics.height}px`,
        "--termd-visual-viewport-height": `${visualViewportMetrics.height}px`,
        "--termd-visual-viewport-offset-left": `${visualViewportMetrics.offsetLeft}px`,
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

  const closeMetadataClient = useCallback(() => {
    metadataClientGenerationRef.current += 1;
    if (metadataRetryTimerRef.current !== undefined) {
      window.clearTimeout(metadataRetryTimerRef.current);
      metadataRetryTimerRef.current = undefined;
    }
    metadataClientAbortControllerRef.current?.abort();
    metadataClientAbortControllerRef.current = undefined;
    const client = metadataClientRef.current;
    if (isExclusiveMetadataClient(
      client,
      metadataClientRef.current,
      workspaceClientRef.current,
      attachClientRef.current,
    )) {
      client.close();
    }
    metadataClientRef.current = undefined;
    setMetadataReady(false);
  }, []);

  const applyDaemonClientsSnapshot = useCallback((clients: DaemonClientSummaryPayload[]) => {
    setDaemonClients(clients);
  }, []);

  const applyDaemonStatusSnapshot = useCallback((status: DaemonStatusResultPayload) => {
    const nextNetworkSample = networkCounterSampleFromStatus(status, Date.now());
    setDaemonNetworkRate(networkRateFromSamples(daemonNetworkSampleRef.current, nextNetworkSample));
    daemonNetworkSampleRef.current = nextNetworkSample;
    setDaemonStatus(status);
    // CPU 历史只保留当前页面内缓存，避免把监控数据写入持久状态。
    setDaemonCpuHistory((current) => appendCpuSample(current, status.cpu_percent));
    setDaemonStatusLoading(false);
    setDaemonStatusError(undefined);
  }, []);

  useEffect(() => {
    return () => closeMetadataClient();
  }, [closeMetadataClient]);

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

  const isIgnoredClosingSessionError = useCallback((sessionId: UUID, caught: unknown) => {
    const code = toSafeError(caught).code;
    if (closingSessionIdsRef.current.has(sessionId)) {
      return IGNORED_CLOSING_SESSION_ERROR_CODES.has(code);
    }
    if (closedSessionIdsRef.current.has(sessionId)) {
      // 中文注释：session 已确认关闭后，旧 attach 上迟到的 input/resize/cursor promise
      // 只是在汇报“那条 transport 已经结束”。这类尾部 connection_closed/stale_connection
      // 不能把一个已经成功完成的 close 操作重新升级成全局连接错误。
      return IGNORED_CLOSED_SESSION_ERROR_CODES.has(code);
    }
    return false;
  }, []);

  const discardPendingTerminalOutput = useCallback(() => {
    // 终端输出由 xterm 自己维护 scrollback；React 只保留尚未写入 xterm 的短队列。
    terminalOutputQueueRef.current = [];
    if (terminalOutputFlushFrameRef.current !== undefined) {
      const flushFrameTestHook = terminalOutputFlushFrameTestHook();
      if (flushFrameTestHook) {
        flushFrameTestHook.cancel(terminalOutputFlushFrameRef.current);
      } else {
        window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
      }
      terminalOutputFlushFrameRef.current = undefined;
    }
    if (terminalOutputFlushTimerRef.current !== undefined) {
      window.clearTimeout(terminalOutputFlushTimerRef.current);
      terminalOutputFlushTimerRef.current = undefined;
    }
  }, []);

  const clearTerminalOutput = useCallback(() => {
    const currentSessionId = attachedSessionRef.current;
    const nextResetVersion = terminalOutputResetVersionRef.current + 1;
    recordTermdDiagnostic("app_clear_terminal_output", {
      sessionId: currentSessionId,
      resetVersion: nextResetVersion,
      queuedItems: terminalOutputQueueRef.current.length,
    }, { stack: true });
    if (currentSessionId) {
      lastRenderedTerminalSeqRef.current.delete(currentSessionId);
    }
    discardPendingTerminalOutput();
    terminalOutputResetVersionRef.current = nextResetVersion;
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

  const clearPendingTerminalInput = useCallback((sessionId?: UUID) => {
    if (sessionId !== undefined && pendingTerminalInputQueueRef.current?.sessionId !== sessionId) {
      return;
    }
    pendingTerminalInputQueueRef.current = undefined;
  }, []);

  const queuePendingTerminalInput = useCallback((sessionId: UUID, data: string) => {
    let queue = pendingTerminalInputQueueRef.current;
    if (queue?.sessionId !== sessionId) {
      queue = { sessionId, chunks: [], byteLength: 0 };
      pendingTerminalInputQueueRef.current = queue;
    }
    const availableBytes = TERMINAL_INPUT_BUFFER_MAX_BYTES - queue.byteLength;
    const encoded = boundedTerminalInputChunks(data, availableBytes);
    const firstChunk = encoded.chunks[0];
    const lastQueuedChunk = queue.chunks.at(-1);
    if (
      firstChunk &&
      lastQueuedChunk &&
      !queue.flushPromise &&
      lastQueuedChunk.byteLength + firstChunk.byteLength <= TERMINAL_INPUT_CHUNK_MAX_BYTES
    ) {
      lastQueuedChunk.data += firstChunk.data;
      lastQueuedChunk.byteLength += firstChunk.byteLength;
      queue.chunks.push(...encoded.chunks.slice(1));
    } else {
      queue.chunks.push(...encoded.chunks);
    }
    queue.byteLength += encoded.byteLength;
    recordTermdDiagnostic("app_terminal_input_queued", {
      sessionId,
      acceptedBytes: encoded.byteLength,
      bufferedBytes: queue.byteLength,
      overflowed: encoded.overflowed,
    });
    if (encoded.overflowed) {
      setError({
        code: "terminal_input_overflow",
        message: `terminal input buffer is limited to ${TERMINAL_INPUT_BUFFER_MAX_BYTES} bytes; excess input was not queued`,
      });
    }
  }, []);

  const flushPendingTerminalInput = useCallback(async (client: V070Client, sessionId: UUID) => {
    const queue = pendingTerminalInputQueueRef.current;
    if (queue?.sessionId !== sessionId || queue.chunks.length === 0) {
      return;
    }
    if (queue.flushPromise) {
      await queue.flushPromise;
      return;
    }
    const flushPromise = (async () => {
      while (pendingTerminalInputQueueRef.current === queue && queue.chunks.length > 0) {
        const chunk = queue.chunks[0];
        await client.sendSessionData(sessionId, new TextEncoder().encode(chunk.data));
        queue.chunks.shift();
        queue.byteLength -= chunk.byteLength;
        recordTermdDiagnostic("app_terminal_input_chunk_flushed", {
          sessionId,
          chunkBytes: chunk.byteLength,
          bufferedBytes: queue.byteLength,
        });
      }
      if (pendingTerminalInputQueueRef.current === queue && queue.chunks.length === 0) {
        pendingTerminalInputQueueRef.current = undefined;
      }
    })();
    queue.flushPromise = flushPromise;
    try {
      await flushPromise;
    } finally {
      if (queue.flushPromise === flushPromise) {
        queue.flushPromise = undefined;
      }
    }
  }, []);

  const resolveTerminalInputSessionId = useCallback(() => {
    // 中文注释：恢复窗口里 transport 可能已经被断开并清掉 attachedSessionRef，
    // 但用户眼里的“当前终端”仍然是正在重新 attach 的那条 session。
    // 这里按“已附着 -> 正在 attach -> UI 当前选中”的优先级兜底，避免恢复首个按键丢失。
    return (
      attachedSessionRef.current ??
      attachingSessionIdRef.current ??
      pendingTerminalAttachSessionRef.current ??
      selectedSessionId
    );
  }, [attachedSessionRef, attachingSessionIdRef, pendingTerminalAttachSessionRef, selectedSessionId]);

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

  useEffect(() => {
    return () => cancelScheduledAttachSwitch();
  }, [cancelScheduledAttachSwitch]);

  const closeAttachForReconnect = useCallback((client?: V070Client) => {
    const belongsToCurrentAttach =
      !client ||
      attachClientRef.current === client ||
      pendingAttachClientRef.current === client;
    recordTermdDiagnostic("app_close_attach_for_reconnect", {
      belongsToCurrentAttach,
      attachedSessionId: attachedSessionRef.current,
      hasAttachClient: Boolean(attachClientRef.current),
      hasPendingAttachClient: Boolean(pendingAttachClientRef.current),
    });
    if (!belongsToCurrentAttach) {
      // 中文注释：旧 attach client 的异步 RPC 可能在用户已经切到新 session 后才失败。
      // 这类 stale 错误只关闭旧 client，不能取消新的 attach 计时器，也不能触发旧 session 重连。
      client?.close();
      return false;
    }
    pendingTerminalAttachAbortControllerRef.current?.abort();
    pendingTerminalAttachAbortControllerRef.current = undefined;
    cancelScheduledAttachSwitch();
    closeAttachClient();
    pendingResizeKeyRef.current = undefined;
    return true;
  }, [cancelScheduledAttachSwitch, closeAttachClient, pendingTerminalAttachAbortControllerRef]);

  const hasLiveAttachedTransport = useCallback(() => {
    const client = attachClientRef.current;
    return Boolean(client && !client.isClosed && attachedSessionRef.current);
  }, [attachClientRef, attachedSessionRef]);

  const isTerminalRecoveryInProgress = useCallback(() => {
    if (attachingSessionIdRef.current) {
      return true;
    }
    if (pendingTerminalAttachSessionRef.current || pendingTerminalAttachAbortControllerRef.current) {
      return true;
    }
    if (pendingAttachClientRef.current || attachReconnectTimerRef.current !== undefined) {
      return true;
    }
    // 中文注释：`attachedSessionRef` 只说明“用户还在这条 session 上”，不等于 transport 仍活着。
    // 一旦 session 还挂着但 attach client 已断开，就属于恢复中的半附着态，不能再把页面
    // 当成 attached/ready。
    return Boolean(attachedSessionRef.current && !hasLiveAttachedTransport());
  }, [
    attachReconnectTimerRef,
    attachedSessionRef,
    attachingSessionIdRef,
    hasLiveAttachedTransport,
    pendingAttachClientRef,
    pendingTerminalAttachAbortControllerRef,
    pendingTerminalAttachSessionRef,
  ]);

  const resolveWorkspaceConnectionStatus = useCallback(() => {
    if (statusRef.current === "creating") {
      return "creating" as const;
    }
    if (isTerminalRecoveryInProgress()) {
      return "attaching" as const;
    }
    return hasLiveAttachedTransport() ? "attached" as const : "ready" as const;
  }, [hasLiveAttachedTransport, isTerminalRecoveryInProgress]);

  const flushTerminalOutput = useCallback(() => {
    terminalOutputFlushFrameRef.current = undefined;
    terminalOutputFlushTimerRef.current = undefined;
    // 这一帧里累积的 session_data 直接交给 xterm drain，避免每帧输出都触发 React 重渲染。
    terminalOutputDrainRef.current?.();
  }, []);

  const rescuePendingTerminalOutputFlush = useCallback((force = false) => {
    if (
      typeof document === "undefined" ||
      (!force && !isDocumentAnimationFrameUnsafe()) ||
      terminalOutputFlushFrameRef.current === undefined ||
      terminalOutputFlushTimerRef.current !== undefined
    ) {
      return;
    }
    // 中文注释：如果 stdout 到来时页面还在前台，flush 会先排进 rAF。
    // 用户随后立刻切后台/切窗口时，这个已排队的 rAF 可能被浏览器冻结；这里要主动
    // 把 pending flush 改挂到 timer，避免 React 队列里的输出就此卡住。
    const flushFrameTestHook = terminalOutputFlushFrameTestHook();
    if (flushFrameTestHook) {
      flushFrameTestHook.cancel(terminalOutputFlushFrameRef.current);
    } else {
      window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
    }
    terminalOutputFlushFrameRef.current = undefined;
    terminalOutputFlushTimerRef.current = window.setTimeout(() => {
      flushTerminalOutput();
    }, 0);
  }, [flushTerminalOutput]);

  const scheduleTerminalOutputFlush = useCallback(() => {
    if (
      terminalOutputFlushFrameRef.current !== undefined ||
      terminalOutputFlushTimerRef.current !== undefined
    ) {
      return;
    }
    if (isDocumentAnimationFrameUnsafe()) {
      // 中文注释：后台标签页、失焦窗口里 requestAnimationFrame 可能被浏览器暂停或重度节流。
      // terminal stdout 不能因此卡在 React 队列里，所以这类状态直接退回 timer flush。
      terminalOutputFlushTimerRef.current = window.setTimeout(() => {
        flushTerminalOutput();
      }, 0);
      return;
    }
    const flushFrameTestHook = terminalOutputFlushFrameTestHook();
    if (flushFrameTestHook) {
      terminalOutputFlushFrameRef.current = flushFrameTestHook.schedule(() => {
        flushTerminalOutput();
      });
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
    const preservePendingInput = options.preservePendingInput ?? false;
    recordTermdDiagnostic("app_disconnect_attach", {
      attachedSessionId: attachedSessionRef.current,
      shouldCloseMobilePanel,
      hasAttachClient: Boolean(attachClientRef.current),
    }, { stack: true });
    cancelScheduledAttachSwitch();
    resetAttachReconnectState();
    resolveTerminalOutputResetWaiters();
    receiveLoopActiveRef.current = false;
    receiveLoopGenerationRef.current += 1;
    // 中文注释：切换 session、主动断开、恢复重连都以 WebSocket 生命周期作为边界。
    // V070Client.close 会先尽力 cancel 已知 terminal stream，再关闭 transport；即使 cancel
    // 没送达，daemon/relay 也能通过 WebSocket close 清掉旧 client context。
    pendingTerminalAttachAbortControllerRef.current?.abort();
    pendingTerminalAttachAbortControllerRef.current = undefined;
    closeAttachClient();
    if (attachedSessionRef.current) {
      lastRenderedTerminalSeqRef.current.delete(attachedSessionRef.current);
      clearTerminalSnapshotRevealHistory(attachedSessionRef.current);
    }
    if (!preservePendingInput) {
      clearPendingTerminalInput(attachedSessionRef.current);
    }
    attachedSessionRef.current = undefined;
    pendingResizeKeyRef.current = undefined;
    confirmedSessionSizesRef.current.clear();
    setAttachedSessionId(undefined);
    clearTerminalOutput();
    clearSessionFiles();
    if (shouldCloseMobilePanel) {
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
    }
  }, [cancelScheduledAttachSwitch, clearPendingTerminalInput, clearSessionFiles, clearTerminalOutput, clearTerminalSnapshotRevealHistory, closeAttachClient, resetAttachReconnectState, resolveTerminalOutputResetWaiters]);

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
    closingSessionIdsRef.current.clear();
    closedSessionIdsRef.current.clear();
    clearTerminalSnapshotRevealHistory();
    closeMetadataClient();
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
    pendingTerminalAttachAbortControllerRef.current?.abort();
    pendingTerminalAttachAbortControllerRef.current = undefined;
    setNewOutputSessionIds(new Set());
    lastRenderedTerminalSeqRef.current.clear();
    attachedSessionRef.current = undefined;
    pendingAttachClientRef.current = undefined;
    pendingTerminalAttachSessionRef.current = undefined;
    pendingResizeKeyRef.current = undefined;
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
  }, [cancelScheduledAttachSwitch, clearSessionFiles, clearTerminalOutput, clearTerminalSnapshotRevealHistory, closeMetadataClient, closeWorkspaceClient, resolveTerminalOutputResetWaiters, selectSession]);

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
    async (serverId: UUID, draftValue?: string) => {
      try {
        const nextState = await renameDaemon(serverId, draftValue ?? daemonRenameDraft);
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
      const { accepted, effectiveUrl } = await pairDeviceOverHttp(
        candidateUrls,
        routeServerId,
        daemonPublicKey,
        token,
        device,
      );
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
    let client: V070Client | undefined;
    try {
      let effectiveServer = { ...server, url: effectiveUrl };
      if (!effectiveServer.device_certificate) {
        const deviceCertificate = await migrateDeviceCertificate(effectiveServer, device);
        effectiveServer = { ...effectiveServer, device_certificate: deviceCertificate };
        setState(await recordDeviceCertificate(server.server_id, deviceCertificate));
      }
      client = await V070Client.connect(effectiveServer, device);
      await client.authenticate();
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
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      autoCheckedServerRef.current = undefined;
      const nextState = await selectDefaultServer(target.server_id);
      // IndexedDB 等待期间 admin detach effect 仍可能提交旧 session 的 reattach/status。
      // 发布新 daemon 前统一清空，避免新 server 与旧 session 状态组合后误 attach。
      resetWorkspaceState();
      setState(nextState);
      setUrl(browserReachableWsUrl(target.url));
      setConnectionEditorOpen(false);
      setActiveSurface("admin");
      setStatus("idle");
    },
    [activeServer?.server_id, resetWorkspaceState, state.pairedServers],
  );

  const {
    loadSessionFiles,
    loadSessionGit,
    requestFollowSessionFilesRefresh,
    handlePassiveSessionFilesResult,
  } = useSessionFileLoaders(sessionFilesController, {
    authenticatedSessionClient,
    activeServerId: activeServer?.server_id,
    activeServerIdRef,
    attachedSessionRef,
    attachedSessionId,
    connectionReady,
  });
  const {
    handleOpenDirectory,
    handleGoToFilePath,
    handleRefreshSessionFiles,
    handleRefreshSessionGit,
    handleSessionFilesPanelTabChange,
  } = useSessionFilesPanelActions({
    sessionFilesPath: sessionFiles?.path,
    sessionFilesLastManualPathRef,
    sessionFilesFollowTerminalCwd,
    setSessionFilesPanelTab,
    handleSessionFilesFollowTerminalCwdChange,
    attachedSessionRef,
    loadSessionFiles,
    loadSessionGit,
    resolveDirectoryPath: resolveRemoteDirectoryPath,
  });
  const { handleCloseGitDiff, handleOpenGitDiff } = useSessionGitDiffViewer({
    attachedSessionId,
    attachedSessionRef,
    setDiffViewer,
    resolveSessionClient,
    basenamePath: basenameRemotePath,
    gitGraphLabel: t("git.graph"),
    translateError: (caught) => translateSafeErrorMessage(toSafeError(caught), t),
  });
  const refreshVisibleDirectory = useCallback(
    async (sessionId: UUID) => {
      await loadSessionFiles(sessionId, sessionFiles?.path ?? sessionFilesLastManualPathRef.current, { source: "manual" });
    },
    [loadSessionFiles, sessionFiles?.path, sessionFilesLastManualPathRef],
  );
  const sessionFilesAutoRefreshPath = useCallback(
    () => {
      // 中文注释：自动刷新只有在 Follow 开启时才允许无 path 读取 terminal cwd。
      // Follow 关闭时返回当前文件面板路径，让 attach/reconnect 保留用户浏览位置。
      return sessionFilesFollowTerminalCwdRef.current
        ? undefined
        : (sessionFiles?.path ?? sessionFilesLastManualPathRef.current);
    },
    [sessionFiles?.path, sessionFilesFollowTerminalCwdRef, sessionFilesLastManualPathRef],
  );
  const {
    handleOpenFile,
    handleSaveOpenFile,
    resetFileEditor,
    openRemoteFile,
  } = useSessionFileEditor({
    attachedSessionId,
    attachedSessionRef,
    fileEditor,
    setFileEditor,
    setSessionFilesError,
    resolveSessionClient,
    refreshVisibleDirectory,
    translateError: (caught) => translateSafeErrorMessage(toSafeError(caught), t),
    textFileMaxBytes: TEXT_FILE_EDITOR_MAX_BYTES,
  });
  const {
    handleDeleteFile,
    handleSessionGitAction,
  } = useSessionMutationActions({
    attachedSessionRef,
    sessionFilesPath: sessionFiles?.path,
    loadSessionFiles,
    loadSessionGit,
    setSessionGitLoading,
    setSessionGitError,
    setSessionFilesLoading,
    setSessionFilesError,
    resolveSessionClient,
  });
  fileEditorResetRef.current = resetFileEditor;

  const handleRefresh = useCallback(async (options: { bootstrap?: boolean } = {}) => {
    if (isPagePaused()) {
      return;
    }
    const requestServerId = activeServer?.server_id;
    setError(undefined);
    setStatus("listing");
    const isBootstrapRefresh = Boolean(options.bootstrap);
    const requestOrderGeneration = sessionOrderGenerationRef.current;
    const requestCreateGeneration = sessionCreateRequestIdRef.current;
    let sessionListApplied = false;
    try {
      const client = await authenticatedWorkspaceClient();
      const needsBootstrapBudget =
        options.bootstrap ||
        (!attachedSessionRef.current && !attachClientRef.current && !attachingSessionIdRef.current);
      const sessionListTimeoutMs = needsBootstrapBudget
        ? ATTACH_CONNECTION_TIMEOUT_MS
        : APP_CONNECTION_TIMEOUT_MS;
      // 中文注释：只要当前 workspace 里还没有 attach 中的终端流，session.list 就仍是
      // 用户可见主路径，应该沿用 terminal 级长预算。已经 attach 之后，手动刷新和后台
      // 元数据刷新继续保持普通 5s 请求预算，避免非关键刷新拖太久。
      const list = await client.listSessions();
      if (
        activeServerIdRef.current !== requestServerId ||
        requestCreateGeneration !== sessionCreateRequestIdRef.current
      ) {
        return;
      }
      if (statusRef.current === "creating") {
        // 中文注释：terminal.create 自己会负责把新 session 写入本地列表、选中并接管 attach。
        // 创建中的旁路 session.list 只能看到 daemon 端“半完成”的新 session，不能反向驱动
        // 工作台状态，否则会抢跑发出第二条 terminal.attach。
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
      clearConfirmedPendingResizeFromSessions(
        list.sessions,
        attachedSessionRef.current,
        pendingResizeKeyRef.current,
        pendingResizeKeyRef,
        confirmedSessionSizesRef.current,
      );
      confirmedSessionSizesRef.current = new Map(list.sessions.map((session) => [session.session_id, session.size]));
      const visibleSessions = list.sessions.filter((session) => !closedSessionIdsRef.current.has(session.session_id));
      const localKnownSessionIds = new Set([
        ...sessionsRef.current.map((session) => session.session_id),
        ...sessionOrderRef.current,
      ]);
      const preserveSessionIds = [
        renamingSessionIdRef.current,
        pendingTerminalAttachSessionRef.current,
        attachingSessionIdRef.current,
        selectedSessionIdRef.current,
        attachedSessionRef.current,
      ];
      const stickySessionId =
        attachingSessionIdRef.current ??
        pendingTerminalAttachSessionRef.current ??
        selectedSessionIdRef.current ??
        attachedSessionRef.current;
      const nextSelectedSessionId = resolveVisibleSelectedSessionId({
        userDetached: userDetachedRef.current,
        stickySessionId,
        renamingSessionId: renamingSessionIdRef.current,
        attachedSessionId: attachedSessionRef.current,
        visibleSessions,
        sessionOrder: nextOrder,
        localKnownSessionIds,
        closedSessionIds: closedSessionIdsRef.current,
      });
      setSessions((current) =>
        // 中文注释：旧 session.list 可能晚于本地创建、点击切换或 attach 返回。
        // 正在本地操作的 session 先以当前 React 状态为准，下一轮 daemon 权威列表会再收敛。
        mergeSessionRefresh(
          visibleSessions,
          current,
          preserveSessionIds,
          nextOrder,
          closedSessionIdsRef.current,
        ),
      );
      // 列表刷新可能晚于用户点击 session 返回；不能用“第一行”覆盖用户刚选择/正在 attach 的目标。
      selectSession(nextSelectedSessionId);
      // session 列表刷新可能来自后台轮询或 cursor 同步；已有 attach 时保留右侧文件树，
      // 避免用户刷新 session 列表后文件 panel 被短暂清空。
      if (!attachedSessionRef.current) {
        clearSessionFiles();
      }
      if (statusRef.current !== "creating") {
        setStatus(resolveWorkspaceConnectionStatus());
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
        setStatus(resolveWorkspaceConnectionStatus());
        return;
      }
      if (
        activeSurfaceRef.current === "workspace" &&
        (
          attachedSessionRef.current ||
          attachClientRef.current ||
          statusRef.current === "ready" ||
          selectedSessionIdRef.current !== undefined ||
          (!isBootstrapRefresh && sessionsRef.current.length === 0)
        )
      ) {
        // 中文注释：一旦用户已经稳定停在 workspace，后续 session.list 就退化成旁路刷新，
        // 即使当前是空工作台或只剩移动端的 session 面板刷新也是如此。relay/control 链路
        // 的瞬时失败只能让这一次刷新作废，不能把页面切回 admin 或升级成全局断线。
        // 但 bootstrap 的首个列表请求不是旁路刷新；失败时必须暴露错误，避免假空列表。
        setStatus(resolveWorkspaceConnectionStatus());
        return;
      }
      setActiveSurface("admin");
      setSafeError(caught);
    } finally {
    }
  }, [activeServer?.server_id, authenticatedWorkspaceClient, clearSessionFiles, resolveWorkspaceConnectionStatus, selectSession, setSafeError]);

  const loadDaemonStatus = useCallback(async () => {
    if (isPagePaused()) {
      return;
    }
    if (statusRef.current === "creating" || isTerminalRecoveryInProgress()) {
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
        // 中文注释：状态栏是非终端 segment，仍复用工作台可靠 WebSocket。
        const status = await client.getDaemonStatus();
        if (!isCurrentRequest()) {
          return;
        }
        applyDaemonStatusSnapshot(status);
      } catch (caught) {
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
  }, [activeServer?.server_id, applyDaemonStatusSnapshot, authenticatedWorkspaceClient, isTerminalRecoveryInProgress]);

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
    const confirmedResizeKey = terminalSizeKey(sessionId, size);
    const currentSize = confirmedSessionSizesRef.current.get(sessionId);
    if (sessionId === attachedSessionRef.current && pendingResizeKeyRef.current === confirmedResizeKey) {
      pendingResizeKeyRef.current = undefined;
    } else if (
      sessionId === attachedSessionRef.current &&
      pendingResizeKeyRef.current &&
      currentSize &&
      !sameTerminalSize(currentSize, size)
    ) {
      // 中文注释：另一个客户端或 daemon snapshot 已确认了不同 grid 时，旧 pending resize
      // 不再代表当前世界；继续保留会挡住本客户端后续把尺寸改回来的请求。
      pendingResizeKeyRef.current = undefined;
    }
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
  }, []);

  useEffect(() => {
    if (
      activeSurface !== "workspace" ||
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
    void handleRefresh({ bootstrap: true });
  }, [activeServer, activeSurface, handleRefresh, state.device, status]);

  const startReceiveLoop = useTerminalReceiveLoop(terminalAttachController, {
    attachClientRef,
    sessionFilesFollowTerminalCwdRef,
    applyConfirmedSessionSize,
    enqueueTerminalOutput,
    isIgnoredClosingSessionError,
    markNewOutputIfBackground,
    setSafeError,
    setSessionFiles,
    setSessionFilesError,
    setSessionFilesLoading,
    setSessionGit,
    setSessionGitError,
    setSessionGitLoading,
    handlePassiveSessionFilesResult,
    loadSessionFiles,
    requestFollowSessionFilesRefresh,
  });

  const scheduleAttachReconnect = useTerminalReconnectScheduler(terminalAttachController, {
    attachClientRef,
    pendingAttachClientRef,
    activeServerId: activeServer?.server_id,
    attachedSessionId,
    selectedSessionId,
    authenticatedClient,
    attachConnectionTimeoutMs: ATTACH_CONNECTION_TIMEOUT_MS,
    reconnectDelaysMs: ATTACH_RECONNECT_DELAYS_MS,
    isRetryableConnectionError,
    isTerminalTransportPaused,
    closeAttachForReconnect,
    discardPendingTerminalOutput,
    resetAttachReconnectState,
    setError,
    setStatus,
    setSafeError,
    setAttachedSessionId,
    setSessions,
    sessionOrderRef,
    clearNewOutputMark,
    clearTerminalOutput,
    clearTerminalSnapshotRevealHistory,
    waitForTerminalOutputResetApplied,
    selectSession,
    startReceiveLoop,
    loadSessionFiles,
    sessionFilesAutoRefreshPath,
    loadSessionGit,
    claimAttachClient,
    onAttachTransportReady: flushPendingTerminalInput,
    upsertAttachedSession,
  });

  attachReconnectHandlerRef.current = scheduleAttachReconnect;

  const handleTerminalResync = useCallback((lastTerminalSeq?: number, options?: TerminalResyncOptions) => {
    const sessionId = attachedSessionRef.current;
    if (sessionId && lastTerminalSeq === undefined && options?.revealHistory) {
      // 中文注释：自动 full snapshot 可能已经启动并关闭当前 attach client。
      // 用户随后上滚时只升级那一次已在路上的 full snapshot token，不能污染后续普通 snapshot。
      const pendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
      if (pendingSnapshot) {
        terminalSnapshotRevealHistoryTokensRef.current.set(sessionId, pendingSnapshot.token);
      }
    }
    const client = attachClientRef.current;
    if (!client) {
      return;
    }
    recordTermdDiagnostic("app_terminal_resync", {
      sessionId,
      lastTerminalSeq,
      forceFullSnapshot: lastTerminalSeq === undefined,
      revealHistory: options?.revealHistory,
    }, { stack: true });
    if (sessionId && lastTerminalSeq !== undefined) {
      lastRenderedTerminalSeqRef.current.set(sessionId, lastTerminalSeq);
    }
    scheduleAttachReconnect(
      client,
      new ProtocolClientError("terminal_resync", "terminal stream out of sync"),
      lastTerminalSeq === undefined
        ? { forceFullSnapshot: true, revealHistory: options?.revealHistory }
        : { lastTerminalSeq },
    );
  }, [scheduleAttachReconnect, terminalSnapshotPendingFullSnapshotTokensRef, terminalSnapshotRevealHistoryTokensRef]);

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
      if (
        closingSessionIdsRef.current.has(sessionId) ||
        closedSessionIdsRef.current.has(sessionId)
      ) {
        return;
      }
      if (
        pendingTerminalInputQueueRef.current !== undefined &&
        pendingTerminalInputQueueRef.current.sessionId !== sessionId
      ) {
        clearPendingTerminalInput();
      }
      clearTerminalSnapshotRevealHistory(sessionId);
      userDetachedRef.current = false;
    setError(undefined);
    setStatus("attaching");
    const attachRequestId = attachRequestIdRef.current + 1;
    attachRequestIdRef.current = attachRequestId;
    attachingSessionIdRef.current = sessionId;
    let outputClient: V070Client | undefined;
    let attachAbortController: AbortController | undefined;
    try {
        const isCurrentAttachRequest = () =>
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId;
        const closePendingAttachClients = () => {
          // 快速点击 session 时，旧连接可能刚完成握手才回到这里；只能关闭自己持有的 client，
          // 不能清掉更新一轮点击已经写入的 pending ref。
          const ownsPendingAttach = Boolean(
            outputClient &&
            pendingAttachClientRef.current === outputClient &&
            pendingTerminalAttachSessionRef.current === sessionId,
          );
          if (ownsPendingAttach) {
            pendingAttachClientRef.current = undefined;
          }
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          if (
            outputClient &&
            outputClient !== attachClientRef.current &&
            outputClient !== workspaceClientRef.current &&
            outputClient !== pendingAttachClientRef.current
          ) {
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
        disconnectAttach({
          closeMobilePanel: shouldCloseMobilePanel,
          preservePendingInput: pendingTerminalInputQueueRef.current?.sessionId === sessionId,
        });
        const resetVersion = clearTerminalOutput();
        attachAbortController = new AbortController();
        pendingTerminalAttachAbortControllerRef.current = attachAbortController;
        outputClient = await authenticatedWorkspaceClient(ATTACH_CONNECTION_TIMEOUT_MS);
        if (!isCurrentAttachRequest()) {
          closePendingAttachClients();
          return;
        }
        pendingAttachClientRef.current = outputClient;
        pendingTerminalAttachSessionRef.current = sessionId;
        const attached = await outputClient.attachSession(sessionId);
        if (!isCurrentAttachRequest()) {
          outputClient.detachSession(sessionId);
          closePendingAttachClients();
          return;
        }
        const attachedClient = outputClient;
        outputClient = undefined;
        pendingAttachClientRef.current = undefined;
        if (
          attachAbortController &&
          pendingTerminalAttachAbortControllerRef.current === attachAbortController
        ) {
          pendingTerminalAttachAbortControllerRef.current = undefined;
        }
        if (pendingTerminalAttachSessionRef.current === sessionId) {
          pendingTerminalAttachSessionRef.current = undefined;
        }
        // 中文注释：输入和 resize 属于 terminal segment，必须复用当前 session 的 WebSocket。
        // 到这里 daemon 已确认 attach，先发布 client 和 session id，让 reset 窗口内的键盘输入
        // 能进入正确 stream；输出 receive loop 仍在 reset 确认后才启动，避免 snapshot 写到旧实例。
        claimAttachClient(attachedClient);
        await flushPendingTerminalInput(attachedClient, sessionId);
        attachedSessionRef.current = sessionId;
        confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
        selectSession(sessionId);
        setAttachedSessionId(sessionId);
        setSessions((current) => upsertAttachedSession(current, attached, sessionOrderRef.current));
        clearNewOutputMark(sessionId);
        closeMobileAttachChrome();
        setStatus("attached");
        // 打开历史 session 后主动请求 xterm focus。桌面端用它补发真实容器尺寸；
        // 移动端也靠它让软键盘保持在终端下方。TerminalPane 会保护 toolbar/files 焦点。
        setTerminalFocusRequest((request) => request + 1);
        // 中文注释：V070Client 的 WebSocket pump 会在 attach response 前后持续收包，
        // 但 App 的 receive loop 只有在这里启动。快速切换多个大输出 session 时，必须先
        // 等 TerminalPane 确认旧 xterm 已经清屏/重建，再把新 snapshot 从 V070Client
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
        // 中文注释：Follow 关闭时，重新 attach 不能用 no-path initial refresh 读取 terminal cwd；
        // 应保留用户当前浏览的文件面板路径，避免切回 workspace 后目录被打回 shell cwd。
        void loadSessionFiles(
          sessionId,
          sessionFilesAutoRefreshPath(),
          { source: "initial" },
        );
        void loadSessionGit(sessionId);
      } catch (caught) {
        if (isIgnoredClosingSessionError(sessionId, caught)) {
          // 中文注释：用户可能在自动 attach 尚未完成时关闭同一个 session；
          // daemon 若先删掉它，晚到的 attach session_not_found 只说明关闭已经生效。
          return;
        }
        if (
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId
        ) {
          setSafeError(caught);
        }
      } finally {
        if (
          attachAbortController &&
          pendingTerminalAttachAbortControllerRef.current === attachAbortController
        ) {
          pendingTerminalAttachAbortControllerRef.current = undefined;
        }
        const ownsPendingAttach = Boolean(
          outputClient &&
          pendingAttachClientRef.current === outputClient &&
          pendingTerminalAttachSessionRef.current === sessionId,
        );
        if (ownsPendingAttach) {
          pendingAttachClientRef.current = undefined;
        }
        if (pendingTerminalAttachSessionRef.current === sessionId) {
          pendingTerminalAttachSessionRef.current = undefined;
        }
        if (
          outputClient &&
          outputClient !== attachClientRef.current &&
          outputClient !== workspaceClientRef.current &&
          outputClient !== pendingAttachClientRef.current
        ) {
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
      clearNewOutputMark,
      clearTerminalSnapshotRevealHistory,
      clearTerminalOutput,
      claimAttachClient,
      disconnectAttach,
      activeServer?.device_certificate,
      authenticatedClient,
      authenticatedWorkspaceClient,
      flushPendingTerminalInput,
      isIgnoredClosingSessionError,
      loadSessionFiles,
      loadSessionGit,
      selectSession,
      sessionFilesAutoRefreshPath,
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
        preservePendingInput: options.preservePendingInput ?? false,
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
      if (
        closingSessionIdsRef.current.has(sessionId) ||
        closedSessionIdsRef.current.has(sessionId)
      ) {
        return;
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
        pendingAttachClientRef.current !== undefined
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
    // 从后台进入工作台必须以当前 daemon 的权威列表重新开始。daemon 切换期间旧 attach
    // 可能留下 ready/attached 和非空 sessions，不能让这些状态跳过新 daemon 的 bootstrap。
    autoCheckedServerRef.current = undefined;
    setStatus("idle");
  }, [activeServer, state.device]);

  useEffect(() => {
    const sessionId = selectedSessionId;
    const shouldReattachCurrentSession =
      activeSurface === "workspace" && reattachCurrentSessionOnOpenRef.current;
    if (
      activeSurface !== "workspace" ||
      !connectionReady ||
      status !== "ready" ||
      !sessionId ||
      closingSessionIdsRef.current.has(sessionId) ||
      closedSessionIdsRef.current.has(sessionId) ||
      terminalCreateOwnsAttachRef.current ||
      pendingAttachClientRef.current ||
      pendingTerminalAttachAbortControllerRef.current ||
      hasLiveAttachedTransport() ||
      isTerminalRecoveryInProgress() ||
      userDetachedRef.current ||
      (autoAttachAttemptedSessionRef.current === sessionId && !shouldReattachCurrentSession)
    ) {
      return;
    }

    // 首次打开或浏览器刷新后，session_list 只选中了第一行；这里补上真正的 attach。
    autoAttachAttemptedSessionRef.current = sessionId;
    // 从管理页回到工作台的后台 reattach 不能抢走用户刚打开的移动端面板。
    void handleAttach(sessionId, { closeMobilePanel: false });
  }, [activeSurface, connectionReady, handleAttach, hasLiveAttachedTransport, isTerminalRecoveryInProgress, selectedSessionId, status]);

  const handleCreateSession = useCallback(async () => {
    userDetachedRef.current = false;
    // 中文注释：`terminal.create` 自己就会建立并接管 watched terminal stream。
    // 在它完成接管前，任何自动 attach 都属于重复链路，只会把 create stream cancel 掉。
    terminalCreateOwnsAttachRef.current = true;
    const createRequestId = sessionCreateRequestIdRef.current + 1;
    sessionCreateRequestIdRef.current = createRequestId;
    setError(undefined);
    disconnectAttach();
    const resetVersion = clearTerminalOutput();
    setStatus("creating");
    let outputClient: V070Client | undefined;
    let attachAbortController: AbortController | undefined;
    try {
      const isCurrentCreateRequest = () => sessionCreateRequestIdRef.current === createRequestId;
      attachAbortController = new AbortController();
      pendingTerminalAttachAbortControllerRef.current = attachAbortController;
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
      const created = await outputClient.createSession([], DEFAULT_SESSION_SIZE);
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
      if (
        attachAbortController &&
        pendingTerminalAttachAbortControllerRef.current === attachAbortController
      ) {
        pendingTerminalAttachAbortControllerRef.current = undefined;
      }
      claimAttachClient(attachedClient);
      attachedSessionRef.current = created.session_id;
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
      // 中文注释：terminal.create 和 terminal.attach 一样会切换 xterm 实例。
      // create stream 的 snapshot/output 可能已经在 V070Client 队列里；必须等
      // TerminalPane 确认旧实例清理、新实例 ready 后再启动 receive loop，否则首屏
      // 可能写进旧实例或跨后续切换被重复回放，表现成切回时多一个 shell 回显。
      await waitForTerminalOutputResetApplied(resetVersion);
      if (!isCurrentCreateRequest() || userDetachedRef.current || attachClientRef.current !== attachedClient) {
        attachedClient.detachSession(created.session_id);
        return;
      }
      startReceiveLoop(attachedClient);
      terminalCreateOwnsAttachRef.current = false;
      // 中文注释：新建 session 没有既有文件面板路径，只有 Follow 开启时才读取 terminal cwd。
      // 关闭 Follow 时跳过自动 no-path 刷新，直到用户手动选择目录。
      void loadSessionFiles(
        created.session_id,
        sessionFilesAutoRefreshPath(),
        { source: "initial" },
      );
    } catch (caught) {
      if (sessionCreateRequestIdRef.current === createRequestId) {
        setSafeError(caught);
      }
    } finally {
      if (
        attachAbortController &&
        pendingTerminalAttachAbortControllerRef.current === attachAbortController
      ) {
        pendingTerminalAttachAbortControllerRef.current = undefined;
      }
      if (outputClient && pendingAttachClientRef.current === outputClient) {
        pendingAttachClientRef.current = undefined;
      }
      if (outputClient && outputClient !== attachClientRef.current) {
        outputClient.close();
      }
      if (sessionCreateRequestIdRef.current === createRequestId) {
        terminalCreateOwnsAttachRef.current = false;
      }
    }
  }, [
    clearNewOutputMark,
    clearTerminalOutput,
    authenticatedWorkspaceClient,
    claimAttachClient,
    disconnectAttach,
    loadSessionFiles,
    selectSession,
    sessionFilesAutoRefreshPath,
    setSafeError,
    startReceiveLoop,
    waitForTerminalOutputResetApplied,
  ]);

  const handleRetryConnection = useCallback(async () => {
    if (isTerminalTransportPaused()) {
      return;
    }
    if (retryConnectionTaskRef.current) {
      return retryConnectionTaskRef.current;
    }
    const sessionId = attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId;
    const task = (async () => {
      if (sessionId) {
        if (
          attachingSessionIdRef.current === sessionId ||
          pendingTerminalAttachSessionRef.current === sessionId ||
          pendingTerminalAttachAbortControllerRef.current !== undefined ||
          pendingAttachClientRef.current !== undefined ||
          attachReconnectTimerRef.current !== undefined
        ) {
          return;
        }
        // 中文注释：performAttach 会先把状态推进到 attaching，再按当前 request id
        // 废弃旧 attach。这里不能额外先 disconnectAttach，否则 focus/online/自动重试
        // 叠加时，后一轮恢复会把前一轮尚未完成的 attach 直接打断，造成重复 snapshot。
        await performAttach(sessionId);
        return;
      }

      setError(undefined);
      setActiveSurface("workspace");
      autoCheckedServerRef.current = undefined;
      await handleRefresh({ bootstrap: true });
    })();
    const trackedTask = task.finally(() => {
      if (retryConnectionTaskRef.current === trackedTask) {
        retryConnectionTaskRef.current = undefined;
      }
    });
    retryConnectionTaskRef.current = trackedTask;
    return trackedTask;
  }, [
    attachReconnectTimerRef,
    attachedSessionId,
    attachedSessionRef,
    attachingSessionIdRef,
    handleRefresh,
    isTerminalTransportPaused,
    pendingAttachClientRef,
    pendingTerminalAttachAbortControllerRef,
    pendingTerminalAttachSessionRef,
    performAttach,
    selectedSessionId,
  ]);

  useEffect(() => {
    retryConnectionHandlerRef.current = handleRetryConnection;
  }, [handleRetryConnection]);

  useWorkspaceAutoRetry(workspaceConnection, {
    error,
    status,
    activeSurface,
    hasPairedServer,
    activeServerId: activeServer?.server_id,
    attachedSessionId,
    selectedSessionId,
    currentAttachedSessionRef: attachedSessionRef,
    retryDelayMs: CONNECTION_AUTO_RETRY_DELAY_MS,
    retryLimit: CONNECTION_AUTO_RETRY_LIMIT,
    onRetryConnection: handleRetryConnection,
  });

  const scheduleResumeMetadataRefresh = useCallback(() => {
    window.setTimeout(() => {
      if (isPagePaused() || activeSurfaceRef.current !== "workspace") {
        return;
      }
      // 中文注释：后台恢复时 terminal WebSocket 重建和普通状态轮询是两条语义。
      // 即使恢复入口已经走了 attach 重建，也要补一轮状态刷新，避免后台期间超时的
      // status 请求把状态栏卡在旧采样上。
      if (!metadataClientRef.current || metadataClientRef.current.isClosed) {
        setMetadataRetryNonce((current) => current + 1);
      }
      void loadDaemonStatus();
    }, 0);
  }, [loadDaemonStatus]);

  useEffect(() => {
    const pauseOfflineConnection = () => {
      if (activeSurface !== "workspace") {
        return;
      }
      // 中文注释：浏览器切 offline 时，WebSocket 不一定会立刻触发 close。
      // 主动丢弃旧 transport，避免恢复后继续向半开连接写 terminal.attach/input。
      closeMetadataClient();
      closeWorkspaceClient();
    };

    const invalidateFrozenTerminalConnection = () => {
      if (
        activeSurfaceRef.current !== "workspace" ||
        !attachedSessionRef.current ||
        !attachClientRef.current ||
        attachClientRef.current.isClosed
      ) {
        return;
      }
      terminalTransportFrozenRef.current = true;
      rescuePendingTerminalOutputFlush(true);
      closeWorkspaceClient();
    };

    const resumeVisibleConnection = () => {
      if (isPagePaused() || activeSurfaceRef.current !== "workspace") {
        return;
      }
      terminalResumePendingRef.current = true;
      if (terminalResumeTaskRef.current) return;
      const runResumePass = async () => {
        const wasFrozen = terminalTransportFrozenRef.current;
        const shouldProbeTerminal = terminalWasHiddenRef.current;
        terminalTransportFrozenRef.current = false;
        terminalWasHiddenRef.current = false;
        if (wasFrozen) {
          if (attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId) {
            await handleRetryConnection();
          }
          if (!terminalResumeMountedRef.current) return;
          scheduleResumeMetadataRefresh();
          return;
        }
        const probeClient = attachClientRef.current;
        const probeSessionId = attachedSessionRef.current;
        if (shouldProbeTerminal && probeClient && probeSessionId && !probeClient.isClosed) {
          const probeSize = confirmedSessionSizesRef.current.get(probeSessionId)
            ?? sessionsRef.current.find((session) => session.session_id === probeSessionId)?.size
            ?? DEFAULT_SESSION_SIZE;
          try {
            await probeClient.probeTerminalLiveness(
              probeSessionId,
              probeSize,
              TERMINAL_LIVENESS_TIMEOUT_MS,
            );
          } catch {
            if (!terminalResumeMountedRef.current) return;
            if (
              attachClientRef.current === probeClient &&
              attachedSessionRef.current === probeSessionId
            ) {
              closeWorkspaceClient();
              await handleRetryConnection();
            }
            scheduleResumeMetadataRefresh();
            return;
          }
        }
        if (!terminalResumeMountedRef.current) return;
        if (error) {
          await handleRetryConnection();
          if (!terminalResumeMountedRef.current) return;
          scheduleResumeMetadataRefresh();
          return;
        }
        if ((attachedSessionId || selectedSessionId) && (!attachClientRef.current || attachClientRef.current.isClosed)) {
          await handleRetryConnection();
          if (!terminalResumeMountedRef.current) return;
          scheduleResumeMetadataRefresh();
          return;
        }
        if (activeServer && state.device && (status === "idle" || status === "connecting")) {
          autoCheckedServerRef.current = undefined;
          setStatus("idle");
          await handleRefresh({ bootstrap: true });
          return;
        }
        if (connectionReady) {
          if (!metadataClientRef.current || metadataClientRef.current.isClosed) {
            setMetadataRetryNonce((current) => current + 1);
          }
          await loadDaemonStatus();
        }
      };
      const task = (async () => {
        try {
          while (terminalResumeMountedRef.current && terminalResumePendingRef.current) {
            terminalResumePendingRef.current = false;
            if (isPagePaused() || activeSurfaceRef.current !== "workspace") return;
            try {
              await runResumePass();
            } catch {
              if (!terminalResumeMountedRef.current) return;
            }
          }
        } finally {
          terminalResumeTaskRef.current = undefined;
        }
      })();
      terminalResumeTaskRef.current = task;
      void task.catch(() => undefined);
    };

    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        terminalWasHiddenRef.current = true;
        rescuePendingTerminalOutputFlush(true);
        return;
      }
      resumeVisibleConnection();
    };

    document.addEventListener("visibilitychange", handleVisibilityChange);
    document.addEventListener("freeze", invalidateFrozenTerminalConnection);
    const handleWindowBlur = () => {
      rescuePendingTerminalOutputFlush(true);
    };
    window.addEventListener("blur", handleWindowBlur);
    window.addEventListener("focus", resumeVisibleConnection);
    window.addEventListener("pagehide", invalidateFrozenTerminalConnection);
    window.addEventListener("pageshow", resumeVisibleConnection);
    window.addEventListener("offline", pauseOfflineConnection);
    window.addEventListener("online", resumeVisibleConnection);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      document.removeEventListener("freeze", invalidateFrozenTerminalConnection);
      window.removeEventListener("blur", handleWindowBlur);
      window.removeEventListener("focus", resumeVisibleConnection);
      window.removeEventListener("pagehide", invalidateFrozenTerminalConnection);
      window.removeEventListener("pageshow", resumeVisibleConnection);
      window.removeEventListener("offline", pauseOfflineConnection);
      window.removeEventListener("online", resumeVisibleConnection);
    };
  }, [
    activeServer,
    activeSurface,
    attachedSessionId,
    closeMetadataClient,
    closeWorkspaceClient,
    connectionReady,
    error,
    handleRefresh,
    handleRetryConnection,
    loadDaemonStatus,
    rescuePendingTerminalOutputFlush,
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
    async (sessionId: UUID, draftValue?: string) => {
      // 中文注释：点击保存和最后一个按键可能发生在同一事件批次里；
      // 提交时优先使用当前 input 传进来的值，避免 React state 晚一拍导致最后一个字符丢失。
      const nextName = (draftValue ?? renameDraft).trim();
      if (!nextName || nextName === renameOriginalName.trim()) {
        return;
      }
      setError(undefined);
      let sessionClient: { client: V070Client; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionClient(sessionId);
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
    [handleCancelRename, renameDraft, renameOriginalName, resolveSessionClient, setSafeError],
  );

  const handleCloseSession = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      closingSessionIdsRef.current.add(sessionId);
      const wasAttached = attachedSessionRef.current === sessionId;
      const wasSelected = selectedSessionId === sessionId;
      const previousUserDetached = userDetachedRef.current;
      const previousSessions = sessionsRef.current;
      const previousSessionOrder = sessionOrderRef.current;
      const removeClosedLocally = () => {
        clearTerminalSnapshotRevealHistory(sessionId);
        const nextSessionOrder = previousSessionOrder.filter((candidate) => candidate !== sessionId);
        setSessions((current) => current.filter((session) => session.session_id !== sessionId));
        confirmedSessionSizesRef.current.delete(sessionId);
        sessionOrderRef.current = nextSessionOrder;
        setSessionOrder(nextSessionOrder);
        clearNewOutputMark(sessionId);
        if (wasSelected) {
          // 关闭当前 session 是显式离开终端；其余 session 只保留在列表中，
          // 必须等用户再次点击后才 attach，不能自动打开第一项。
          selectSession(undefined);
          clearSessionFiles();
        }
        if (wasAttached || wasSelected) {
          setStatus("ready");
        }
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
        closedSessionIdsRef.current.add(sessionId);
      };
      try {
        // 中文注释：关闭当前 attach session 时先声明“这是一次有意断开”。
        // 这样旧 terminal WebSocket 若在 daemon close 收尾期间报 connection_closed，
        // receive loop / reconnect 都会把它当作预期行为，而不是重新 attach 回已删除 session。
        if (wasAttached || wasSelected) {
          userDetachedRef.current = true;
        }
        if (wasAttached) {
          disconnectAttach();
          clearTerminalOutput();
        }
        if (wasSelected) {
          cancelScheduledAttachSwitch();
        }
        if (attachingSessionIdRef.current === sessionId) {
          attachRequestIdRef.current += 1;
          attachingSessionIdRef.current = undefined;
        }
        if (pendingTerminalAttachSessionRef.current === sessionId) {
          pendingTerminalAttachSessionRef.current = undefined;
        }
        removeClosedLocally();
        const client = await authenticatedWorkspaceClient();
        try {
          await client.closeSession(sessionId);
        } catch (caught) {
          if (!isIgnoredClosingSessionError(sessionId, caught)) {
            throw caught;
          }
        }
      } catch (caught) {
        if (isIgnoredClosingSessionError(sessionId, caught)) {
          return;
        }
        closedSessionIdsRef.current.delete(sessionId);
        sessionOrderRef.current = previousSessionOrder;
        setSessionOrder(previousSessionOrder);
        setSessions(previousSessions);
        if (wasSelected) selectSession(sessionId);
        if (wasAttached || wasSelected) {
          userDetachedRef.current = previousUserDetached;
        }
        setSafeError(caught);
      } finally {
        closingSessionIdsRef.current.delete(sessionId);
      }
    },
    [
      clearSessionFiles,
      clearTerminalOutput,
      clearTerminalSnapshotRevealHistory,
      disconnectAttach,
      clearNewOutputMark,
      isIgnoredClosingSessionError,
      cancelScheduledAttachSwitch,
      selectedSessionId,
      selectSession,
      authenticatedWorkspaceClient,
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
      const sessionId = resolveTerminalInputSessionId();
      recordTermdDiagnostic("app_terminal_input_received", {
        sessionId,
        attachedSessionId: attachedSessionRef.current,
        attachingSessionId: attachingSessionIdRef.current,
        selectedSessionId,
        hasClient: Boolean(client),
        clientClosed: client?.isClosed ?? false,
        chunkLength: data.length,
      });
      if (!sessionId) {
        recordTermdDiagnostic("app_terminal_input_drop_no_session", {
          chunkLength: data.length,
        });
        return;
      }
      if (!client || client.isClosed) {
        // 中文注释：恢复窗口里 UI 可能仍显示当前 session，但 attach transport 已经被
        // offline/reconnect/sidecar close 清掉。这里不能静默丢字，先按 session 排队，
        // 再触发当前恢复链路把这段输入补发到新 transport。
        queuePendingTerminalInput(sessionId, data);
        void retryConnectionHandlerRef.current?.();
        return;
      }
      queuePendingTerminalInput(sessionId, data);
      try {
        await flushPendingTerminalInput(client, sessionId);
        recordTermdDiagnostic("app_terminal_input_sent", {
          sessionId,
          chunkLength: data.length,
        });
      } catch (caught) {
        recordTermdDiagnostic("app_terminal_input_send_error", {
          sessionId,
          chunkLength: data.length,
          error: toSafeError(caught),
        });
        if (isRetryableConnectionError(caught) && attachReconnectHandlerRef.current(client, caught)) {
          return;
        }
        if (!isIgnoredClosingSessionError(sessionId, caught)) {
          setSafeError(caught);
        }
      }
    },
    [attachClientRef, attachReconnectHandlerRef, attachedSessionRef, attachingSessionIdRef, flushPendingTerminalInput, isIgnoredClosingSessionError, queuePendingTerminalInput, resolveTerminalInputSessionId, selectedSessionId, setSafeError],
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
        if (isTerminalSidecarTransientError(caught)) {
          // 中文注释：resize ack 既可能被持续 stdout 挤到超时，也可能在 relay/HTTP
          // 控制面瞬断时直接收到 fetch TypeError。这两类都只代表本次辅助 ack 失败，
          // 不能升级成全局错误或 attach reconnect。这里要在 retryable transport 分支前拦下。
          if (pendingResizeKeyRef.current === nextResizeKey) {
            pendingResizeKeyRef.current = undefined;
          }
          recordTermdDiagnostic("app_terminal_sidecar_timeout_ignored", {
            kind: "resize",
            sessionId,
          });
          return;
        }
        if (isRetryableConnectionError(caught) && attachReconnectHandlerRef.current(client, caught)) {
          return;
        }
        if (pendingResizeKeyRef.current === nextResizeKey) {
          pendingResizeKeyRef.current = undefined;
        }
        if (!isIgnoredClosingSessionError(sessionId, caught)) {
          setSafeError(caught);
        }
      });
    },
    [isIgnoredClosingSessionError, sessions, setSafeError],
  );

  useEffect(() => {
    if (!connectionReady || !activeServer || !state.device || isPagePaused()) {
      closeMetadataClient();
      return undefined;
    }

    const requestServerId = activeServer.server_id;
    const generation = metadataClientGenerationRef.current + 1;
    metadataClientGenerationRef.current = generation;
    const abortController = new AbortController();
    metadataClientAbortControllerRef.current = abortController;

    const isCurrentMetadataClient = () =>
      metadataClientGenerationRef.current === generation &&
      activeServerIdRef.current === requestServerId &&
      !abortController.signal.aborted;

    const scheduleMetadataRetry = () => {
      if (metadataRetryTimerRef.current !== undefined || isPagePaused()) {
        return;
      }
      metadataRetryTimerRef.current = window.setTimeout(() => {
        metadataRetryTimerRef.current = undefined;
        setMetadataRetryNonce((current) => current + 1);
      }, DAEMON_METADATA_RETRY_DELAY_MS);
    };

    void (async () => {
      let client: V070Client | undefined;
      let unsubscribeMetadata: (() => void) | undefined;
      let latencyTimer: number | undefined;
      try {
        client = await authenticatedWorkspaceClient(APP_CONNECTION_TIMEOUT_MS);
        if (!isCurrentMetadataClient()) {
          return;
        }
        metadataClientRef.current = client;
        await client.subscribeMetadata();
        if (!isCurrentMetadataClient()) {
          return;
        }
        setMetadataReady(true);
        unsubscribeMetadata = client.watchMetadata((_revision: number, metadata: any) => {
          if (!isCurrentMetadataClient()) return;
          if (Array.isArray(metadata.clients)) {
            applyDaemonClientsSnapshot(metadata.clients as DaemonClientSummaryPayload[]);
          }
          if (metadata.daemon && typeof metadata.daemon === "object") {
            applyDaemonStatusSnapshot(metadata.daemon as DaemonStatusResultPayload);
          }
          if (Array.isArray(metadata.sessions)) {
            const visibleSessions = (metadata.sessions as SessionSummaryPayload[])
              .filter((session) => !closedSessionIdsRef.current.has(session.session_id));
            const currentAttachedSession = visibleSessions.find(
              (session) => session.session_id === attachedSessionRef.current,
            );
            const previousAttachedSession = sessionsRef.current.find(
              (session) => session.session_id === attachedSessionRef.current,
            );
            if (currentAttachedSession?.files_path !== previousAttachedSession?.files_path) {
              void requestFollowSessionFilesRefresh(currentAttachedSession?.session_id);
            }
            const nextOrder = sessionOrderFromDaemonList(visibleSessions);
            sessionOrderRef.current = nextOrder;
            setSessionOrder(nextOrder);
            setSessions(orderSessions(sortSessionsNewestFirst(visibleSessions), nextOrder));
          }
        });
        const latencyClient = client;
        const scheduleNextLatencyMeasurement = () => {
          if (!isCurrentMetadataClient()) {
            return;
          }
          latencyTimer = window.setTimeout(() => {
            latencyTimer = undefined;
            if (isPagePaused()) {
              scheduleNextLatencyMeasurement();
              return;
            }
            void measureLatency();
          }, DAEMON_LATENCY_POLL_INTERVAL_MS);
        };
        const measureLatency = async () => {
          if (isPagePaused() || !isCurrentMetadataClient()) {
            return;
          }
          try {
            const latencyMs = await latencyClient.measureLatency();
            if (isCurrentMetadataClient()) {
              setDaemonNetworkLatencyMs(latencyMs);
              scheduleNextLatencyMeasurement();
            }
          } catch {
            // An unanswered ping must not start another measurement on the same connection.
          }
        };
        void measureLatency();
        await new Promise<void>((resolve) => {
          if (abortController.signal.aborted) {
            resolve();
            return;
          }
          abortController.signal.addEventListener("abort", () => resolve(), { once: true });
        });
      } catch (caught) {
        if (!isCurrentMetadataClient()) {
          return;
        }
        const safeError = toSafeError(caught);
        if (safeError.code !== "receive_interrupted" && safeError.code !== "connection_closed") {
          recordTermdDiagnostic("app_metadata_sidecar_closed", {
            code: safeError.code,
          });
        }
        setMetadataReady(false);
        scheduleMetadataRetry();
      } finally {
        if (latencyTimer !== undefined) {
          window.clearTimeout(latencyTimer);
        }
        unsubscribeMetadata?.();
        if (metadataClientAbortControllerRef.current === abortController) {
          metadataClientAbortControllerRef.current = undefined;
        }
        if (metadataClientRef.current === client) {
          metadataClientRef.current = undefined;
        }
      }
    })();

    return () => closeMetadataClient();
  }, [
    activeServer?.server_id,
    applyDaemonClientsSnapshot,
    applyDaemonStatusSnapshot,
    authenticatedClient,
    authenticatedWorkspaceClient,
    closeMetadataClient,
    connectionReady,
    metadataRetryNonce,
    requestFollowSessionFilesRefresh,
    state.device?.device_id,
  ]);

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

  const handleCloseFileEditor = useCallback(() => {
    resetFileEditor();
  }, [resetFileEditor]);

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
      let sessionClient: { client: V070Client; ownsClient: boolean } | undefined;
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

  const handleOpenGitFile = useCallback(
    (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => {
      void openRemoteFile({
        name: basenameRemotePath(change.path),
        path: joinRemotePath(worktree.path, change.path),
        sizeBytes: 0,
      });
    },
    [openRemoteFile],
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
      let sessionClient: { client: V070Client; ownsClient: boolean } | undefined;
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
            <span className="app-version">v{APP_VERSION}</span>
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
              <code>{activeServer ? displayUrlWithoutQueryOrFragment(activeServer.url) : t("app.unpaired")}</code>
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
            <Suspense fallback={<LazyPanelFallback />}>
              <DaemonManagerPanel
                servers={pairedServerOptions}
                activeServerId={activeServer?.server_id}
                renamingServerId={renamingDaemonId}
                renameDraft={daemonRenameDraft}
                onSelect={(serverId) => void handleSelectServer(serverId)}
                onStartRename={handleStartDaemonRename}
                onRenameDraftChange={setDaemonRenameDraft}
                onSaveRename={(serverId, nextName) => void handleSaveDaemonRename(serverId, nextName)}
                onCancelRename={handleCancelDaemonRename}
                onForget={(serverId) => void handleForgetDaemon(serverId)}
              />
            </Suspense>
          </div>
          {qrScannerOpen ? (
            <Suspense fallback={<LazyModalFallback className="qr-scanner-dialog" />}>
              <PairingQrScanner
                onDetected={handleQrDetected}
                onClose={() => setQrScannerOpen(false)}
              />
            </Suspense>
          ) : null}
          {settingsOpen ? (
            <Suspense fallback={<LazyModalFallback className="settings-dialog" />}>
              <SettingsDialog
                open={settingsOpen}
                preferences={preferences}
                effectiveLocale={effectiveLocale}
                effectiveTheme={effectiveTheme}
                onPreferencesChange={handlePreferencesChange}
                onClose={() => setSettingsOpen(false)}
              />
            </Suspense>
          ) : null}
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
                    <CollapsedSessionButton
                      key={session.session_id}
                      session={session}
                      selected={session.session_id === selectedSessionId}
                      hasNewOutput={newOutputSessionIds.has(session.session_id)}
                      onAttach={(sessionId) => void handleAttach(sessionId)}
                    />
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
                  <span className="app-version">v{APP_VERSION}</span>
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
                  creating={status === "creating"}
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
                  <Suspense fallback={<LazyPanelFallback className="daemon-clients" />}>
                    <DaemonClientsPanel
                      clients={daemonClients}
                      currentDeviceId={state.device?.device_id}
                      forgettingClientIds={forgettingClientIds}
                      onForgetOfflineClient={handleForgetOfflineClient}
                    />
                  </Suspense>
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
                mobileInputMode={mobileTerminalInputMode}
                mobileKeyboardOpen={mobileKeyboardOpen}
                mobileViewportWidth={mobileTerminalInputMode ? visualViewportMetrics.width : undefined}
                mobileViewportHeight={mobileTerminalInputMode ? visualViewportMetrics.height : undefined}
                mobileViewportOffsetTop={mobileTerminalInputMode ? visualViewportMetrics.offsetTop : undefined}
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
              />
              {showDesktopFilesPanel ? (
                <>
                  <Suspense fallback={<LazyPanelFallback className="files-panel" />}>
                    <SessionFilesPanel
                      attachedSessionId={attachedSessionId}
                      activeTab={sessionFilesPanelTab}
                      files={sessionFiles}
                      loading={sessionFilesLoading}
                      error={sessionFilesError}
                      uploadProgress={visibleFileTransferProgress.uploadProgress}
                      downloadProgress={visibleFileTransferProgress.downloadProgress}
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
                  </Suspense>
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
                  onClick={() => {
                    void handleRefresh();
                  }}
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
                creating={status === "creating"}
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
            <Suspense fallback={<LazyPanelFallback className="files-panel" />}>
              <SessionFilesPanel
                attachedSessionId={attachedSessionId}
                activeTab={sessionFilesPanelTab}
                files={sessionFiles}
                loading={sessionFilesLoading}
                error={sessionFilesError}
                uploadProgress={visibleFileTransferProgress.uploadProgress}
                downloadProgress={visibleFileTransferProgress.downloadProgress}
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
            </Suspense>
          </div>
        ) : null}
        {fileEditor ? (
          <Suspense fallback={<LazyModalFallback className="file-editor-dialog" />}>
            <FileEditorDialog
              open
              path={fileEditor.path}
              name={fileEditor.name}
              initialText={fileEditor.text}
              loading={fileEditor.loading}
              saving={fileEditor.saving}
              error={fileEditor.error}
              language={languageForPath(fileEditor.path)}
              theme={effectiveTheme}
              onSave={handleSaveOpenFile}
              onClose={handleCloseFileEditor}
            />
          </Suspense>
        ) : null}
        {diffViewer ? (
          <Suspense fallback={<LazyModalFallback className="file-editor-dialog" />}>
            <FileEditorDialog
              open
              path={diffViewer.path}
              name={diffViewer.name}
              initialText={diffViewer.text}
              loading={diffViewer.loading}
              error={diffViewer.error}
              language="diff"
              theme={effectiveTheme}
              readOnly
              onSave={() => undefined}
              onClose={handleCloseGitDiff}
            />
          </Suspense>
        ) : null}
        {settingsOpen ? (
          <Suspense fallback={<LazyModalFallback className="settings-dialog" />}>
            <SettingsDialog
              open={settingsOpen}
              preferences={preferences}
              effectiveLocale={effectiveLocale}
              effectiveTheme={effectiveTheme}
              onPreferencesChange={handlePreferencesChange}
              onClose={() => setSettingsOpen(false)}
            />
          </Suspense>
        ) : null}
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
          return (
            <span className="session-operator" key={client.client_id} title={label}>
              <span className="status-dot online" aria-hidden="true" />
              <span>{label}</span>
              {isCurrentDevice ? <span>{t("operators.you")}</span> : null}
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
  const showCpuHistory = props.value !== "..." && props.history.length > 0;
  return (
    <div className="daemon-status-metric daemon-status-cpu">
      <span>{t("daemonStatus.cpu")}</span>
      <strong>{props.value}</strong>
      {showCpuHistory ? <CpuBarChart samples={props.history} /> : null}
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
      <title>{t("daemonStatus.cpuBars")}</title>
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

function useMobileTerminalInputMode(isMobileLayout: boolean): boolean {
  const getSnapshot = () => {
    if (isMobileLayout) {
      return true;
    }
    if (typeof window === "undefined") {
      return false;
    }
    const navigatorLike = window.navigator as Navigator & { maxTouchPoints?: number };
    const hasTouchPoints = (navigatorLike.maxTouchPoints ?? 0) > 0;
    // 中文注释：测试和部分桌面运行时可能保留 `ontouchstart` 属性但值为 undefined；
    // 这种占位属性不能当成触摸能力，否则桌面窗口 blur 会被误判成移动端软键盘抖动。
    const hasTouchEvent = (window as Window & { ontouchstart?: unknown }).ontouchstart !== undefined;
    const hasCoarsePointer =
      typeof window.matchMedia === "function" &&
      window.matchMedia("(pointer: coarse)").matches;
    // 中文注释：布局是否进入移动版只看宽度；但软键盘输入保护应覆盖横屏手机、
    // 折叠屏和平板这类宽屏触摸设备，否则 window.blur 会按桌面逻辑主动收起键盘。
    return hasTouchPoints || hasTouchEvent || hasCoarsePointer;
  };

  const [mobileInputMode, setMobileInputMode] = useState(getSnapshot);

  useEffect(() => {
    if (isMobileLayout) {
      setMobileInputMode(true);
      return undefined;
    }
    if (typeof window === "undefined") {
      setMobileInputMode(false);
      return undefined;
    }

    const coarsePointerQuery =
      typeof window.matchMedia === "function"
        ? window.matchMedia("(pointer: coarse)")
        : undefined;
    const update = () => setMobileInputMode(getSnapshot());
    update();
    window.addEventListener("resize", update);
    if (coarsePointerQuery) {
      if (typeof coarsePointerQuery.addEventListener === "function") {
        coarsePointerQuery.addEventListener("change", update);
      } else {
        coarsePointerQuery.addListener(update);
      }
    }
    return () => {
      window.removeEventListener("resize", update);
      if (!coarsePointerQuery) {
        return;
      }
      if (typeof coarsePointerQuery.removeEventListener === "function") {
        coarsePointerQuery.removeEventListener("change", update);
      } else {
        coarsePointerQuery.removeListener(update);
      }
    };
  }, [isMobileLayout]);

  return mobileInputMode;
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

function useVisualViewportMetrics(enabled: boolean): { width: number; height: number; offsetLeft: number; offsetTop: number; keyboardInset: number; keyboardOpen: boolean } {
  const metricsFromWindow = () => {
    if (typeof window === "undefined") {
      return { width: 0, height: 0, offsetLeft: 0, offsetTop: 0, keyboardInset: 0, keyboardOpen: false };
    }
    const viewport = window.visualViewport;
    const width = Math.round(viewport?.width ?? window.innerWidth);
    const height = Math.round(viewport?.height ?? window.innerHeight);
    const offsetLeft = Math.round(viewport?.offsetLeft ?? 0);
    const offsetTop = Math.round(viewport?.offsetTop ?? 0);
    const keyboardInset = Math.max(0, Math.round(window.innerHeight - height - offsetTop));
    // 地址栏收缩也会改变 visualViewport，高度差超过常见工具栏后才按软键盘处理。
    return { width, height, offsetLeft, offsetTop, keyboardInset, keyboardOpen: keyboardInset >= 80 };
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
        return current.width === next.width &&
          current.height === next.height &&
          current.offsetLeft === next.offsetLeft &&
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
    : {
        width: typeof window === "undefined" ? 0 : window.innerWidth,
        height: typeof window === "undefined" ? 0 : window.innerHeight,
        offsetLeft: 0,
        offsetTop: 0,
        keyboardInset: 0,
        keyboardOpen: false,
      };
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
  return knownServerWsUrlCandidates(rawUrl, serverId, page);
}

export function knownServerWsUrlCandidates(
  rawUrl: string,
  serverId: UUID,
  page:
    | (Pick<Location, "protocol" | "host" | "hostname"> & Partial<Pick<Location, "pathname">>)
    | undefined = globalThis.location,
): string[] {
  const candidates: string[] = [];
  const savedUrl = stripSensitiveUrlParts(rawUrl);
  const pageUrl = defaultWsUrlFromPage(page);
  const savedCandidate = routeWsUrlForKnownServer(browserReachableWsUrl(savedUrl, page), serverId);
  const pageCandidate =
    page?.hostname && !isLoopbackHost(page.hostname)
      ? routeWsUrlForKnownServer(pageUrl, serverId)
      : undefined;

  if (shouldPreferPageWsUrl(savedUrl, page)) {
    // 从 relay Web 页面打开时优先使用当前 origin，避免旧 IndexedDB 里的历史 relay host
    // 让用户刷新后继续连到过期地址。
    addCandidate(candidates, pageCandidate);
    addCandidate(candidates, savedCandidate);
  } else {
    // 中文注释：开发/临时环境常见 Web 和 relay 使用同一主机的不同端口。
    // 用户显式保存的 relay 端口必须优先，否则 attach 会误连到 Vite/Web 静态服务的 /ws。
    addCandidate(candidates, savedCandidate);
    addCandidate(candidates, pageCandidate);
  }

  return candidates;
}

function shouldPreferPageWsUrl(
  savedUrl: string,
  page:
    | (Pick<Location, "host" | "hostname"> & Partial<Pick<Location, "protocol" | "pathname">>)
    | undefined,
): boolean {
  if (!page?.hostname || isLoopbackHost(page.hostname)) {
    return false;
  }
  try {
    const parsed = new URL(savedUrl);
    // 页面来源和保存地址是不同主机时，通常表示用户从新的 relay Web 入口打开了旧状态。
    if (!isLoopbackHost(parsed.hostname) && parsed.hostname !== page.hostname) {
      return true;
    }
    if (
      page.protocol === "https:" &&
      !isLoopbackHost(parsed.hostname) &&
      parsed.hostname === page.hostname
    ) {
      const pageWsUrl = new URL(defaultWsUrlFromPage({
        protocol: page.protocol,
        host: page.host,
        pathname: page.pathname,
      }));
      // 中文注释：HTTPS relay Web 是用户当前真实入口。同 hostname 但端口或公开 path
      // 已变化时，继续优先 IndexedDB 里的旧 URL 会让重新配对后仍连到旧 relay。
      return parsed.host !== pageWsUrl.host ||
        parsed.pathname.replace(/\/+$/, "") !== pageWsUrl.pathname.replace(/\/+$/, "");
    }
    return false;
  } catch {
    return false;
  }
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
  closedSessionIds: Set<UUID> = new Set(),
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
    if (!sessionId || remoteIds.has(sessionId) || closedSessionIds.has(sessionId)) {
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

function isVisibleSelectedSessionCandidate(
  sessionId: UUID | undefined,
  visibleSessionIds: Set<UUID>,
  localKnownSessionIds: Set<UUID>,
  closedSessionIds: Set<UUID>,
): boolean {
  if (!sessionId || closedSessionIds.has(sessionId)) {
    return false;
  }
  return visibleSessionIds.has(sessionId) || localKnownSessionIds.has(sessionId);
}

function resolveVisibleSelectedSessionId(input: {
  userDetached: boolean;
  stickySessionId?: UUID;
  renamingSessionId?: UUID;
  attachedSessionId?: UUID;
  visibleSessions: SessionSummaryPayload[];
  sessionOrder: UUID[];
  localKnownSessionIds: Set<UUID>;
  closedSessionIds: Set<UUID>;
}): UUID | undefined {
  if (input.userDetached) {
    return undefined;
  }
  const visibleSessionIds = new Set(input.visibleSessions.map((session) => session.session_id));
  if (
    isVisibleSelectedSessionCandidate(
      input.stickySessionId,
      visibleSessionIds,
      input.localKnownSessionIds,
      input.closedSessionIds,
    )
  ) {
    return input.stickySessionId;
  }
  const firstVisibleSessionId = orderSessions(
    sortSessionsNewestFirst(input.visibleSessions),
    input.sessionOrder,
  ).at(0)?.session_id;
  if (firstVisibleSessionId) {
    return firstVisibleSessionId;
  }
  if (
    isVisibleSelectedSessionCandidate(
      input.renamingSessionId,
      visibleSessionIds,
      input.localKnownSessionIds,
      input.closedSessionIds,
    )
  ) {
    return input.renamingSessionId;
  }
  if (
    isVisibleSelectedSessionCandidate(
      input.attachedSessionId,
      visibleSessionIds,
      input.localKnownSessionIds,
      input.closedSessionIds,
    )
  ) {
    return input.attachedSessionId;
  }
  return undefined;
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
  client: V070Client,
  sessionId: UUID,
  path: string,
): Promise<SessionFileEntryPayload | undefined> {
  const normalized = normalizeRemotePath(path);
  const files = await client.listSessionFiles(sessionId, remoteParentPath(normalized));
  return files.entries.find((entry: SessionFileEntryPayload) => entry.path === normalized);
}

async function downloadSessionFile(
  client: V070Client,
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
  // 中文注释：PTY 的真实尺寸身份只有 rows/cols。pixel_width/height 只是本地布局快照，
  // 刷新和字体 metrics 稳定前会抖动，不能用来决定是否发起或确认 resize。
  return a.rows === b.rows && a.cols === b.cols;
}

function terminalSizeKey(sessionId: UUID, size: TerminalSize): string {
  return `${sessionId}:${size.rows}:${size.cols}`;
}

function clearConfirmedPendingResizeFromSessions(
  sessions: SessionSummaryPayload[],
  attachedSessionId: UUID | undefined,
  pendingResizeKey: string | undefined,
  pendingResizeKeyRef: { current: string | undefined },
  currentSizes: Map<UUID, TerminalSize>,
): void {
  if (!attachedSessionId || !pendingResizeKey) {
    return;
  }
  const attachedSession = sessions.find((session) => session.session_id === attachedSessionId);
  if (!attachedSession) {
    return;
  }
  // 中文注释：session.list 可能先于 session_resized ack 返回同一个新尺寸；
  // 如果不在这里清理 pending key，后续用户重新聚焦也可能被旧 pending resize 挡掉。
  if (terminalSizeKey(attachedSessionId, attachedSession.size) === pendingResizeKey) {
    pendingResizeKeyRef.current = undefined;
    return;
  }
  const currentSize = currentSizes.get(attachedSessionId);
  if (currentSize && !sameTerminalSize(currentSize, attachedSession.size)) {
    // 中文注释：session.list 确认了别的 grid，说明旧 pending 已经不是当前会话状态；
    // 继续保留会把后续本地 resize 请求挡掉。
    pendingResizeKeyRef.current = undefined;
  }
}
