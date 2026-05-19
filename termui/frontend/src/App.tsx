import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent } from "react";
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
  Unplug,
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
const FILES_CWD_FOLLOW_POLL_INTERVAL_MS = 1000;
const TEXT_FILE_EDITOR_MAX_BYTES = 1024 * 1024;
const FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES = 16 * 1024 * 1024;
const FILE_DOWNLOAD_CHUNK_BYTES = 256 * 1024;
const FILE_UPLOAD_MAX_BYTES = 16 * 1024 * 1024;
const MOBILE_LAYOUT_QUERY = "(max-width: 760px)";
const MOBILE_LAYOUT_BREAKPOINT = 760;
const CPU_HISTORY_LIMIT = 48;
const CPU_BAR_CHART_WIDTH = 56;
const CPU_BAR_CHART_HEIGHT = 18;
const CPU_BAR_CHART_COUNT = 18;
export const DAEMON_STATUS_POLL_INTERVAL_MS = 1000;
const APP_CONNECTION_TIMEOUT_MS = 2000;
const PAIRING_CONNECTION_TIMEOUT_MS = 5000;
const ATTACH_CONNECTION_TIMEOUT_MS = 15000;
type AppSurface = "admin" | "workspace";

const RETRYABLE_CONNECTION_ERROR_CODES = new Set([
  "connection_closed",
  "connection_error",
  "connect_timeout",
  "route_prelude_timeout",
  "relay_daemon_offline",
  "relay_state_unavailable",
  "handshake_timeout",
  "response_timeout",
  "terminal_resync",
]);

function isRetryableConnectionError(caught: unknown): boolean {
  return RETRYABLE_CONNECTION_ERROR_CODES.has(toSafeError(caught).code);
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
  const [terminalResizeOwner, setTerminalResizeOwner] = useState(false);
  const [sessionFiles, setSessionFiles] = useState<SessionFilesResultPayload | undefined>();
  const [sessionFilesLoading, setSessionFilesLoading] = useState(false);
  const [sessionFilesError, setSessionFilesError] = useState<SafeError | undefined>();
  const [sessionFilesFollowTerminalCwd, setSessionFilesFollowTerminalCwd] = useState(true);
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
  const [connectionEditorOpen, setConnectionEditorOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [qrScannerOpen, setQrScannerOpen] = useState(false);
  const [renamingDaemonId, setRenamingDaemonId] = useState<UUID | undefined>();
  const [daemonRenameDraft, setDaemonRenameDraft] = useState("");
  const [activeSurface, setActiveSurface] = useState<AppSurface>("admin");
  const [status, setStatus] = useState("idle");
  const [error, setError] = useState<SafeError | undefined>();
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const terminalResizeOwnerRef = useRef(false);
  const autoAttachAttemptedSessionRef = useRef<UUID | undefined>(undefined);
  const attachingSessionIdRef = useRef<UUID | undefined>(undefined);
  const attachRequestIdRef = useRef(0);
  const reattachCurrentSessionOnOpenRef = useRef(false);
  const userDetachedRef = useRef(false);
  const pendingResizeKeyRef = useRef<string | undefined>(undefined);
  const confirmedSessionSizesRef = useRef<Map<UUID, TerminalSize>>(new Map());
  const receiveLoopActiveRef = useRef(false);
  const closingSessionIdsRef = useRef<Set<UUID>>(new Set());
  const forgettingClientIdsRef = useRef<Set<UUID>>(new Set());
  const renamingSessionIdRef = useRef<UUID | undefined>(undefined);
  const filesPanelWidthRef = useRef(DEFAULT_FILES_PANEL_WIDTH);
  const sessionFilesFollowTerminalCwdRef = useRef(sessionFilesFollowTerminalCwd);
  const sessionFilesRequestSeqRef = useRef(0);
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
  const terminalRenderAckRef = useRef<{ sessionId: UUID; lastTransportSeq: number; credit: number } | undefined>(undefined);
  const lastRenderedTerminalSeqRef = useRef<Map<UUID, number>>(new Map());
  const terminalOutputResetVersionRef = useRef(0);
  const terminalOutputAppliedResetVersionRef = useRef(0);
  const terminalOutputResetWaitersRef = useRef<Map<number, () => void>>(new Map());
  const terminalOutputFlushFrameRef = useRef<number | undefined>(undefined);
  const terminalOutputDrainRef = useRef<(() => void) | undefined>(undefined);
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);
  const attachReconnectTimerRef = useRef<number | undefined>(undefined);
  const attachReconnectKeyRef = useRef<string | undefined>(undefined);
  const attachReconnectAttemptsRef = useRef(0);
  const attachReconnectLastErrorRef = useRef<unknown>(undefined);
  const attachReconnectHandlerRef = useRef<(client: DirectClient, caught: unknown) => boolean>(() => false);
  const daemonNetworkSampleRef = useRef<DaemonNetworkCounterSample | undefined>(undefined);
  const daemonStatusRefreshInFlightRef = useRef(false);
  const daemonClientsRefreshInFlightRef = useRef(false);
  const lastNotificationAtRef = useRef(0);
  const isMobileLayout = useMobileLayout();
  const visualViewportMetrics = useVisualViewportMetrics(isMobileLayout && activeSurface === "workspace");
  const systemTheme = useSystemTheme();
  const preferences = state.preferences ?? DEFAULT_BROWSER_PREFERENCES;
  const effectiveTheme = resolveTheme(preferences.theme, systemTheme);
  const effectiveLocale = resolveLocale(preferences.language);
  const t = useMemo(() => createTranslator(effectiveLocale), [effectiveLocale]);

  useEffect(() => {
    sessionFilesFollowTerminalCwdRef.current = sessionFilesFollowTerminalCwd;
  }, [sessionFilesFollowTerminalCwd]);

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
    };
  }, []);

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
  const showDesktopFilesPanel = !isMobileLayout && filesPanelOpen;
  const desktopWorkspaceStyle =
    !isMobileLayout && showDesktopFilesPanel
      ? { gridTemplateColumns: `minmax(0, 1fr) ${filesPanelWidth}px` }
      : undefined;
  const appShellStyle = isMobileLayout
    ? ({
        "--termd-visual-viewport-height": `${visualViewportMetrics.height}px`,
        "--termd-visual-viewport-offset-top": `${visualViewportMetrics.offsetTop}px`,
      } as CSSProperties)
    : undefined;
  const mobileKeyboardOpen = isMobileLayout && activeSurface === "workspace" && visualViewportMetrics.keyboardOpen;
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
    sessionFilesFollowRefreshInFlightRef.current = false;
    setSessionFiles(undefined);
    setSessionFilesError(undefined);
    setSessionFilesLoading(false);
    setSessionGit(undefined);
    setSessionGitError(undefined);
    setSessionGitLoading(false);
    setFileEditor(undefined);
  }, []);

  const updateTerminalResizeOwner = useCallback((owned: boolean) => {
    terminalResizeOwnerRef.current = owned;
    setTerminalResizeOwner(owned);
  }, []);

  const discardPendingTerminalOutput = useCallback(() => {
    // 终端输出由 xterm 自己维护 scrollback；React 只保留尚未写入 xterm 的短队列。
    terminalOutputQueueRef.current = [];
    terminalRenderAckRef.current = undefined;
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
    for (const [pendingVersion, resolve] of terminalOutputResetWaitersRef.current) {
      if (pendingVersion <= terminalOutputAppliedResetVersionRef.current) {
        terminalOutputResetWaitersRef.current.delete(pendingVersion);
        resolve();
      }
    }
  }, []);

  const waitForTerminalOutputResetApplied = useCallback((version: number) => {
    if (terminalOutputAppliedResetVersionRef.current >= version) {
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      const timer = window.setTimeout(() => {
        terminalOutputResetWaitersRef.current.delete(version);
        resolve();
      }, 250);
      terminalOutputResetWaitersRef.current.set(version, () => {
        window.clearTimeout(timer);
        resolve();
      });
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

  const closeAttachForReconnect = useCallback((client?: DirectClient) => {
    receiveLoopActiveRef.current = false;
    if (!client || attachClientRef.current === client) {
      attachClientRef.current?.close();
      attachClientRef.current = undefined;
    } else {
      client.close();
    }
    pendingResizeKeyRef.current = undefined;
    updateTerminalResizeOwner(false);
    lastCursorReportRef.current = "";
    lastCursorFocusedRef.current = undefined;
    if (cursorRefreshTimerRef.current !== undefined) {
      window.clearTimeout(cursorRefreshTimerRef.current);
      cursorRefreshTimerRef.current = undefined;
    }
  }, [updateTerminalResizeOwner]);

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

  const flushRenderedTerminalAck = useCallback(() => {
    const pending = terminalRenderAckRef.current;
    const client = attachClientRef.current;
    if (!pending || !client) {
      return;
    }
    terminalRenderAckRef.current = undefined;
    client.ackTerminalRender(pending.sessionId, pending.lastTransportSeq, pending.credit);
  }, []);

  const markTerminalFrameRendered = useCallback((sessionId: UUID, transportSeq: number, renderCredit: number) => {
    const credit = Math.max(1, Math.floor(renderCredit));
    const pending = terminalRenderAckRef.current;
    if (!pending || pending.sessionId !== sessionId) {
      if (pending && attachClientRef.current) {
        attachClientRef.current.ackTerminalRender(pending.sessionId, pending.lastTransportSeq, pending.credit);
      }
      terminalRenderAckRef.current = { sessionId, lastTransportSeq: transportSeq, credit };
    } else {
      pending.lastTransportSeq = Math.max(pending.lastTransportSeq, transportSeq);
      pending.credit += credit;
    }
    if ((terminalRenderAckRef.current?.credit ?? 0) >= 64 * 1024) {
      flushRenderedTerminalAck();
    }
  }, [flushRenderedTerminalAck]);

  const enqueueTerminalOutput = useCallback((item: TerminalOutputItem) => {
    const queue = terminalOutputQueueRef.current;
    const previous = queue.at(-1);
    if (item.kind === "data" && previous?.kind === "data") {
      // legacy `session_data` 没有 terminal_seq/transport_seq 边界，可以安全合并成
      // 一个 xterm write；新的 terminal_frame 仍逐帧保留，用于渲染完成后精确补 credit。
      previous.bytes = concatByteChunks([previous.bytes, item.bytes]);
    } else {
      queue.push(item);
    }
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

  const disconnectAttach = useCallback(() => {
    resetAttachReconnectState();
    receiveLoopActiveRef.current = false;
    attachClientRef.current?.close();
    attachClientRef.current = undefined;
    if (attachedSessionRef.current) {
      lastRenderedTerminalSeqRef.current.delete(attachedSessionRef.current);
    }
    attachedSessionRef.current = undefined;
    pendingResizeKeyRef.current = undefined;
    confirmedSessionSizesRef.current.clear();
    updateTerminalResizeOwner(false);
    setAttachedSessionId(undefined);
    lastCursorReportRef.current = "";
    lastCursorFocusedRef.current = undefined;
    if (cursorRefreshTimerRef.current !== undefined) {
      window.clearTimeout(cursorRefreshTimerRef.current);
      cursorRefreshTimerRef.current = undefined;
    }
    clearTerminalOutput();
    clearSessionFiles();
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
  }, [clearSessionFiles, clearTerminalOutput, resetAttachReconnectState, updateTerminalResizeOwner]);

  const handleDisconnectAttach = useCallback(() => {
    // 用户主动断开时不要被“默认打开第一个 session”的自动流程立即重新 attach。
    userDetachedRef.current = true;
    autoAttachAttemptedSessionRef.current = undefined;
    disconnectAttach();
    setSelectedSessionId(undefined);
    setStatus("ready");
  }, [disconnectAttach]);

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
    setSessionOrder([]);
    sessionOrderRef.current = [];
    autoAttachAttemptedSessionRef.current = undefined;
    attachingSessionIdRef.current = undefined;
    attachRequestIdRef.current += 1;
    reattachCurrentSessionOnOpenRef.current = false;
    userDetachedRef.current = false;
    setNewOutputSessionIds(new Set());
    lastRenderedTerminalSeqRef.current.clear();
    setDaemonClients([]);
    setDaemonStatus(undefined);
    setDaemonCpuHistory([]);
    setDaemonNetworkRate(undefined);
    setDaemonNetworkLatencyMs(undefined);
    daemonNetworkSampleRef.current = undefined;
    setDaemonStatusError(undefined);
    setSelectedSessionId(undefined);
    renamingSessionIdRef.current = undefined;
    setRenamingSessionId(undefined);
    setRenameDraft("");
    setRenameOriginalName("");
    clearTerminalOutput();
    clearSessionFiles();
    autoCheckedServerRef.current = undefined;
  }, [clearSessionFiles, clearTerminalOutput]);

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
      setState(nextState);
      setPairingToken("");
      setConnectionEditorOpen(false);
      setSessions([]);
      confirmedSessionSizesRef.current.clear();
      setSessionOrder([]);
      sessionOrderRef.current = [];
      autoAttachAttemptedSessionRef.current = undefined;
      reattachCurrentSessionOnOpenRef.current = false;
      userDetachedRef.current = false;
      setNewOutputSessionIds(new Set());
      setDaemonClients([]);
      setDaemonStatus(undefined);
      setDaemonCpuHistory([]);
      setDaemonNetworkRate(undefined);
      setDaemonNetworkLatencyMs(undefined);
      daemonNetworkSampleRef.current = undefined;
      setDaemonStatusError(undefined);
      setSelectedSessionId(undefined);
      renamingSessionIdRef.current = undefined;
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setRenameOriginalName("");
      clearTerminalOutput();
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      disconnectAttach();
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
  }, [activeServer, clearTerminalOutput, disconnectAttach, pairingToken, setSafeError, url]);

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
      disconnectAttach();
      const nextState = await recordServerUrl(server.server_id, effectiveUrl);
      setState(nextState);
      setSessions([]);
      confirmedSessionSizesRef.current.clear();
      setSessionOrder([]);
      sessionOrderRef.current = [];
      autoAttachAttemptedSessionRef.current = undefined;
      reattachCurrentSessionOnOpenRef.current = false;
      userDetachedRef.current = false;
      setNewOutputSessionIds(new Set());
      setDaemonClients([]);
      setDaemonStatus(undefined);
      setDaemonCpuHistory([]);
      setDaemonNetworkRate(undefined);
      setDaemonNetworkLatencyMs(undefined);
      daemonNetworkSampleRef.current = undefined;
      setDaemonStatusError(undefined);
      setSelectedSessionId(undefined);
      renamingSessionIdRef.current = undefined;
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setRenameOriginalName("");
      clearTerminalOutput();
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
  }, [activeServer, clearTerminalOutput, disconnectAttach, setSafeError, state.device, url]);

  const handleSelectServer = useCallback(
    async (serverId: UUID) => {
      const target = state.pairedServers.find((server) => server.server_id === serverId);
      if (!target || target.server_id === activeServer?.server_id) {
        return;
      }

      setError(undefined);
      disconnectAttach();
      setSessions([]);
      confirmedSessionSizesRef.current.clear();
      setSessionOrder([]);
      sessionOrderRef.current = [];
      autoAttachAttemptedSessionRef.current = undefined;
      reattachCurrentSessionOnOpenRef.current = false;
      userDetachedRef.current = false;
      setNewOutputSessionIds(new Set());
      setDaemonClients([]);
      setDaemonStatus(undefined);
      setDaemonCpuHistory([]);
      setDaemonNetworkRate(undefined);
      setDaemonNetworkLatencyMs(undefined);
      daemonNetworkSampleRef.current = undefined;
      setDaemonStatusError(undefined);
      setSelectedSessionId(undefined);
      renamingSessionIdRef.current = undefined;
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setRenameOriginalName("");
      clearTerminalOutput();
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
    [activeServer?.server_id, clearTerminalOutput, disconnectAttach, state.pairedServers],
  );

  const authenticatedClient = useCallback(async (timeoutMs = APP_CONNECTION_TIMEOUT_MS) => {
    const server = activeServer;
    const device = state.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    const reachableUrl = browserReachableWsUrl(server.url);
    const routeUrl = routeWsUrlForKnownServer(reachableUrl, server.server_id) ?? reachableUrl;
    const client = await DirectClient.connect(routeUrl, server.server_id, device.device_id, {
      expectedDaemonPublicKey: server.daemon_public_key,
      timeoutMs,
    });
    await client.authenticate(device, { ...server, url: routeUrl });
    return client;
  }, [activeServer, state.device]);

  const authenticatedSessionClient = useCallback(
    async (sessionId: UUID) => {
      const client = await authenticatedClient();
      try {
        // daemon 对文件、重命名等 session 级操作要求当前连接已 attach。
        // 这些旁路连接只短暂 attach，避免和 xterm 主连接的 receive loop 抢同一个 socket。
        await client.attachSession(sessionId, { watchUpdates: false });
        return client;
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
      const attachedClient = attachClientRef.current;
      let client: DirectClient | undefined;
      let request: Promise<SessionFilesResultPayload>;
      try {
        if (attachedSessionRef.current === sessionId && attachedClient) {
          // 直接请求可以拿到一次性的响应，避免把文件树状态和后台推送混在同一条回写链路里。
          request = attachedClient.listSessionFiles(sessionId, path);
        } else {
          client = await authenticatedSessionClient(sessionId);
          // 文件树当前位置是 daemon 端 session 共享状态；不传 path 时由 daemon 返回当前共享目录。
          request = client.listSessionFiles(sessionId, path);
        }
        const files = await request;
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
        client?.close();
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
      if (!silent) {
        setSessionGitLoading(true);
        setSessionGitError(undefined);
      }
      const attachedClient = attachClientRef.current;
      if (attachedSessionRef.current === sessionId && attachedClient) {
        try {
          // Git tab 和文件树一样复用当前 attach 连接，响应仍交给 receive loop，
          // 避免多个 reader 同时消费同一个 WebSocket。
          await attachedClient.requestSessionGit(sessionId);
          return;
        } catch (caught) {
          if (!silent) {
            setSessionGit(undefined);
            setSessionGitError(toSafeError(caught));
            setSessionGitLoading(false);
          }
          return;
        }
      }
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const git = await client.getSessionGit(sessionId);
        setSessionGit(git);
        setSessionGitError(undefined);
      } catch (caught) {
        if (!silent) {
          setSessionGit(undefined);
          setSessionGitError(toSafeError(caught));
        }
      } finally {
        client?.close();
        if (!silent) {
          setSessionGitLoading(false);
        }
      }
    },
    [authenticatedSessionClient],
  );

  const handleRefresh = useCallback(async () => {
    const requestServerId = activeServer?.server_id;
    setError(undefined);
    setStatus("listing");
    const requestOrderGeneration = sessionOrderGenerationRef.current;
    try {
      const client = await authenticatedClient();
      const list = await client.listSessions();
      const clients = await client.listDaemonClients();
      client.close();
      if (activeServerIdRef.current !== requestServerId) {
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
      const firstSessionId = userDetachedRef.current
        ? undefined
        : orderedSessions.at(0)?.session_id ?? renamingSessionIdRef.current ?? attachedSessionRef.current;
      setSessions((current) =>
        mergeSessionRefresh(list.sessions, current, [
          renamingSessionIdRef.current,
          attachedSessionRef.current,
        ], nextOrder),
      );
      setDaemonClients(clients.clients);
      setSelectedSessionId(firstSessionId);
      // session 列表刷新可能来自后台轮询或 cursor 同步；已有 attach 时保留右侧文件树，
      // 避免用户刷新 session 列表后文件 panel 被短暂清空。
      if (!attachedSessionRef.current) {
        clearSessionFiles();
      }
      setStatus("ready");
    } catch (caught) {
      if (activeServerIdRef.current !== requestServerId) {
        return;
      }
      setActiveSurface("admin");
      setSafeError(caught);
    }
  }, [activeServer?.server_id, authenticatedClient, clearSessionFiles, setSafeError]);

  const refreshDaemonClients = useCallback(
    async () => {
      if (daemonClientsRefreshInFlightRef.current) {
        return;
      }
      daemonClientsRefreshInFlightRef.current = true;
      const requestServerId = activeServer?.server_id;
      const requestOrderGeneration = sessionOrderGenerationRef.current;
      try {
        const attachedClient = attachClientRef.current;
        const ownsClient = !attachedClient;
        const client = attachedClient ?? await authenticatedClient();
        try {
          // 已 attach 时复用 xterm 主连接，避免状态轮询每 2 秒创建一次 relay client。
          // receive pump 会按 packet id 分发 response，不会和终端输出读取互相抢 socket。
          const [sessionList, clientList] = await Promise.all([
            client.listSessions(),
            client.listDaemonClients(),
          ]);
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
          setDaemonClients(clientList.clients);
        } catch (caught) {
          if (attachedClient && isRetryableConnectionError(caught)) {
            attachReconnectHandlerRef.current(attachedClient, caught);
          }
          throw caught;
        } finally {
          if (ownsClient) {
            client.close();
          }
        }
      } catch (caught) {
        // 后台 client/session 刷新失败不能把正在使用的 xterm 切到错误态；
        // 主 attach 连接有自己的重连路径，手动 Refresh 仍会显示错误。
        void caught;
      } finally {
        daemonClientsRefreshInFlightRef.current = false;
      }
    },
    [activeServer?.server_id, authenticatedClient],
  );

  const loadDaemonStatus = useCallback(async () => {
    if (daemonStatusRefreshInFlightRef.current) {
      return;
    }
    daemonStatusRefreshInFlightRef.current = true;
    setDaemonStatusLoading(true);
    setDaemonStatusError(undefined);
    try {
      const attachedClient = attachClientRef.current;
      const ownsClient = !attachedClient;
      const client = attachedClient ?? await authenticatedClient();
      try {
        // 状态栏和 RTT 都走同一条主连接；relay 页面不会再因为每秒状态刷新看到短连接风暴。
        const status = await client.getDaemonStatus();
        const latencyMs = await client.measureLatency().catch(() => undefined);
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
        if (attachedClient && isRetryableConnectionError(caught)) {
          attachReconnectHandlerRef.current(attachedClient, caught);
        }
        throw caught;
      } finally {
        if (ownsClient) {
          client.close();
        }
      }
    } catch (caught) {
      setDaemonStatusError(toSafeError(caught));
      if (!attachClientRef.current) {
        setDaemonNetworkLatencyMs(undefined);
      }
    } finally {
      daemonStatusRefreshInFlightRef.current = false;
      setDaemonStatusLoading(false);
    }
  }, [authenticatedClient]);

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

  useEffect(() => {
    if (!activeServer || !state.device || status !== "idle" || autoCheckedServerRef.current === activeServer.server_id) {
      return;
    }
    autoCheckedServerRef.current = activeServer.server_id;
    setStatus("connecting");
    void handleRefresh();
  }, [activeServer, handleRefresh, state.device, status]);

  const startReceiveLoop = useCallback((client: DirectClient) => {
    receiveLoopActiveRef.current = true;
    const read = async () => {
      while (receiveLoopActiveRef.current && attachClientRef.current === client) {
        try {
          const inner = await client.receiveInner();
          if (inner.type === "session_data") {
            const payload = inner.payload as SessionDataPayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              continue;
            }
            enqueueTerminalOutput({ kind: "data", bytes: sessionDataFromBase64(payload.data_base64) });
          } else if (inner.type === "terminal_frame") {
            const payload = inner.payload as RenderableTerminalFramePayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              continue;
            }
            const onRendered = () => markTerminalFrameRendered(payload.session_id, payload.transport_seq, payload.render_credit);
            if (payload.kind === "snapshot") {
              enqueueTerminalOutput({
                kind: "snapshot",
                bytes: sessionDataFromBase64(payload.data_base64),
                baseSeq: payload.base_seq,
                onRendered,
              });
            } else if (payload.kind === "output") {
              enqueueTerminalOutput({
                kind: "output",
                bytes: sessionDataFromBase64(payload.data_base64),
                terminalSeq: payload.terminal_seq,
                onRendered,
              });
            } else if (payload.kind === "resize") {
              enqueueTerminalOutput({ kind: "resize", terminalSeq: payload.terminal_seq, onRendered });
            } else if (payload.kind === "exit") {
              enqueueTerminalOutput({ kind: "exit", terminalSeq: payload.terminal_seq, onRendered });
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
            // session_resize 是请求，session_resized 才是 daemon 确认；前端只在这里更新
            // session size，TerminalPane 随后按这份确认尺寸执行本地 xterm resize。
            confirmedSessionSizesRef.current.set(payload.session_id, payload.size);
            setSessions((current) =>
              current.map((session) =>
                session.session_id === payload.session_id ? { ...session, size: payload.size } : session,
              ),
            );
            if (payload.session_id === attachedSessionRef.current) {
              updateTerminalResizeOwner(Boolean(payload.resize_owner));
              const confirmedResizeKey = terminalSizeKey(payload.session_id, payload.size);
              if (pendingResizeKeyRef.current === confirmedResizeKey) {
                pendingResizeKeyRef.current = undefined;
              }
            }
          }
        } catch (caught) {
          // 旧 attach 关闭可能晚于新 attach 启动；只有当前 client 的错误才能切到错误态。
          if (receiveLoopActiveRef.current && attachClientRef.current === client) {
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
  }, [enqueueTerminalOutput, markNewOutputIfBackground, markTerminalFrameRendered, setSafeError, updateTerminalResizeOwner]);

  const scheduleAttachReconnect = useCallback((staleClient: DirectClient, caught: unknown, options: { lastTerminalSeq?: number } = {}) => {
    const sessionId = attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId;
    if (!sessionId || userDetachedRef.current || !isRetryableConnectionError(caught)) {
      return false;
    }
    const lastTerminalSeq =
      options.lastTerminalSeq ?? lastRenderedTerminalSeqRef.current.get(sessionId);

    const reconnectKey = `${activeServer?.server_id ?? "unknown"}:${sessionId}`;
    if (attachReconnectKeyRef.current !== reconnectKey) {
      attachReconnectKeyRef.current = reconnectKey;
      attachReconnectAttemptsRef.current = 0;
      attachReconnectLastErrorRef.current = caught;
    } else {
      attachReconnectLastErrorRef.current = caught;
    }

    closeAttachForReconnect(staleClient);
    discardPendingTerminalOutput();
    setError(undefined);

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
          client = await authenticatedClient(ATTACH_CONNECTION_TIMEOUT_MS);
          const attached = await client.attachSession(
            sessionId,
            lastTerminalSeq !== undefined ? { lastTerminalSeq } : {},
          );
          if (userDetachedRef.current) {
            resetAttachReconnectState();
            client.close();
            return;
          }
          if (attachReconnectKeyRef.current !== reconnectKey) {
            client.close();
            return;
          }

          const attachedClient = client;
          client = undefined;
          resetAttachReconnectState();
          attachClientRef.current = attachedClient;
          attachedSessionRef.current = sessionId;
          confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
          setSelectedSessionId(sessionId);
          setAttachedSessionId(sessionId);
          setSessions((current) => upsertAttachedSession(current, attached, sessionOrderRef.current));
          clearNewOutputMark(sessionId);
          setStatus("attached");
          if (lastTerminalSeq === undefined) {
            await waitForTerminalOutputResetApplied(clearTerminalOutput());
            if (userDetachedRef.current || attachClientRef.current !== attachedClient) {
              attachedClient.close();
              return;
            }
          }
          startReceiveLoop(attachedClient);
          updateTerminalResizeOwner(Boolean(attached.resize_owner));
          void loadSessionFiles(sessionId, undefined, { silent: true, source: "initial" });
          void loadSessionGit(sessionId, { silent: true });
          void refreshDaemonClients();
        } catch (retryError) {
          client?.close();
          attachReconnectLastErrorRef.current = retryError;
          if (!attachReconnectHandlerRef.current(staleClient, retryError)) {
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
    setSafeError,
    startReceiveLoop,
    updateTerminalResizeOwner,
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

  const handleAttach = useCallback(
    async (sessionId: UUID) => {
      if (attachingSessionIdRef.current === sessionId) {
        setSelectedSessionId(sessionId);
        clearNewOutputMark(sessionId);
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
        return;
      }
      userDetachedRef.current = false;
      setError(undefined);
      setStatus("attaching");
      const attachRequestId = attachRequestIdRef.current + 1;
      attachRequestIdRef.current = attachRequestId;
      attachingSessionIdRef.current = sessionId;
      let client: DirectClient | undefined;
      try {
        const shouldRefreshCurrentAttach =
          reattachCurrentSessionOnOpenRef.current &&
          attachedSessionRef.current === sessionId &&
          Boolean(attachClientRef.current);
        if (attachedSessionRef.current === sessionId && attachClientRef.current && !shouldRefreshCurrentAttach) {
          setSelectedSessionId(sessionId);
          clearNewOutputMark(sessionId);
          setStatus("attached");
          setMobilePanel(undefined);
          setMobileMenuOpen(false);
          return;
        }
        reattachCurrentSessionOnOpenRef.current = false;
        disconnectAttach();
        clearTerminalOutput();
        client = await authenticatedClient(ATTACH_CONNECTION_TIMEOUT_MS);
        const attached = await client.attachSession(sessionId);
        if (
          attachRequestIdRef.current !== attachRequestId ||
          attachingSessionIdRef.current !== sessionId
        ) {
          client.close();
          client = undefined;
          return;
        }
        const attachedClient = client;
        client = undefined;
        attachClientRef.current = attachedClient;
        attachedSessionRef.current = sessionId;
        confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
        setSelectedSessionId(sessionId);
        setAttachedSessionId(sessionId);
        setSessions((current) => upsertAttachedSession(current, attached, sessionOrderRef.current));
        clearNewOutputMark(sessionId);
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
        setStatus("attached");
        if (isMobileLayout) {
          // 移动端打开历史 session 后主动请求 xterm focus，让软键盘保持在终端下方。
          // 真实 resize 权限仍由 daemon 下发的 resize_owner 控制。
          setTerminalFocusRequest((request) => request + 1);
        }
        startReceiveLoop(attachedClient);
        updateTerminalResizeOwner(Boolean(attached.resize_owner));
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
        client?.close();
        if (
          attachRequestIdRef.current === attachRequestId &&
          attachingSessionIdRef.current === sessionId
        ) {
          attachingSessionIdRef.current = undefined;
        }
      }
    },
    [
      authenticatedClient,
      clearNewOutputMark,
      clearTerminalOutput,
      disconnectAttach,
      loadSessionFiles,
      loadSessionGit,
      refreshDaemonClients,
      setSafeError,
      isMobileLayout,
      startReceiveLoop,
      updateTerminalResizeOwner,
    ],
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
      attachClientRef.current ||
      attachedSessionRef.current ||
      userDetachedRef.current ||
      (autoAttachAttemptedSessionRef.current === sessionId && !shouldReattachCurrentSession)
    ) {
      return;
    }

    // 首次打开或浏览器刷新后，session_list 只选中了第一行；这里补上真正的 attach。
    autoAttachAttemptedSessionRef.current = sessionId;
    void handleAttach(sessionId);
  }, [activeSurface, connectionReady, handleAttach, selectedSessionId, status]);

  const handleCreateSession = useCallback(async () => {
    userDetachedRef.current = false;
    setError(undefined);
    disconnectAttach();
    clearTerminalOutput();
    setStatus("creating");
    try {
      const client = await authenticatedClient();
      // Web 只创建完整的默认 shell 会话，避免把 session 误导成一次性命令执行。
      const created = await client.createSession([], DEFAULT_SESSION_SIZE);
      attachClientRef.current = client;
      attachedSessionRef.current = created.session_id;
      confirmedSessionSizesRef.current.set(created.session_id, created.size);
      updateTerminalResizeOwner(Boolean(created.resize_owner));
      setSelectedSessionId(created.session_id);
      setAttachedSessionId(created.session_id);
      clearNewOutputMark(created.session_id);
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      const nextOrder = [created.session_id, ...sessionOrderRef.current.filter((sessionId) => sessionId !== created.session_id)];
      sessionOrderRef.current = nextOrder;
      setSessionOrder(nextOrder);
      setSessions((current) => upsertSession(current, created, nextOrder));
      // 新建 session 等价于打开一个新的 SSH shell，应立即把输入焦点交给 xterm。
      // 普通打开历史 session 仍保持 viewer 逻辑，避免意外接管其他客户端的 PTY 尺寸。
      setTerminalFocusRequest((request) => request + 1);
      setStatus("attached");
      startReceiveLoop(client);
      void loadSessionFiles(created.session_id, undefined, { source: "initial" });
      void refreshDaemonClients();
    } catch (caught) {
      setSafeError(caught);
    }
  }, [
    authenticatedClient,
    clearNewOutputMark,
    clearTerminalOutput,
    disconnectAttach,
    loadSessionFiles,
    refreshDaemonClients,
    setSafeError,
    startReceiveLoop,
    updateTerminalResizeOwner,
  ]);

  const handleRetryConnection = useCallback(async () => {
    const sessionId = attachedSessionRef.current ?? attachedSessionId ?? selectedSessionId;
    if (sessionId) {
      // PWA 从后台恢复时旧 WebSocket 可能已经被系统关闭；先断开旧 attach，
      // 否则 handleAttach 会误以为当前 session 已连接而直接短路返回。
      disconnectAttach();
      await handleAttach(sessionId);
      return;
    }

    setError(undefined);
    setActiveSurface("workspace");
    autoCheckedServerRef.current = undefined;
    await handleRefresh();
  }, [attachedSessionId, disconnectAttach, handleAttach, handleRefresh, selectedSessionId]);

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
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const renamed = await client.renameSession(sessionId, nextName);
        setSessions((current) =>
          current.map((session) =>
            session.session_id === renamed.session_id ? { ...session, name: renamed.name } : session,
          ),
        );
        handleCancelRename();
      } catch (caught) {
        setSafeError(caught);
      } finally {
        client?.close();
      }
    },
    [authenticatedSessionClient, handleCancelRename, renameDraft, renameOriginalName, setSafeError],
  );

  const handleCloseSession = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      closingSessionIdsRef.current.add(sessionId);
      const wasAttached = attachedSessionRef.current === sessionId;
      const wasSelected = selectedSessionId === sessionId;
      if (wasAttached) {
        // 先断开本地 xterm attach 连接，再发关闭请求；否则 xterm 清理阶段的 cursor/resize
        // 可能在 daemon 已删除 session 后继续发送，导致页面错误地显示 session_not_found。
        disconnectAttach();
        clearTerminalOutput();
      }
      let client: DirectClient | undefined;
      try {
        client = await authenticatedClient();
        try {
          await client.closeSession(sessionId);
        } catch (caught) {
          if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
            throw caught;
          }
        }
        setSessions((current) => current.filter((session) => session.session_id !== sessionId));
        confirmedSessionSizesRef.current.delete(sessionId);
        sessionOrderRef.current = sessionOrderRef.current.filter((candidate) => candidate !== sessionId);
        setSessionOrder(sessionOrderRef.current);
        clearNewOutputMark(sessionId);
        if (wasSelected) {
          setSelectedSessionId(undefined);
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
        client?.close();
        // 关闭当前会话时，旧 attach 连接上已经发出的 cursor/resize promise 可能稍后才失败；
        // 短暂保留 closing 标记，避免这些迟到的 session_not_found 覆盖掉成功删除后的 UI。
        window.setTimeout(() => {
          closingSessionIdsRef.current.delete(sessionId);
        }, 1000);
      }
    },
    [
      authenticatedClient,
      clearSessionFiles,
      clearTerminalOutput,
      disconnectAttach,
      clearNewOutputMark,
      isIgnoredClosingSessionNotFound,
      refreshDaemonClients,
      selectedSessionId,
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
          const client = await authenticatedClient();
          const reordered = await client.reorderSessions(sessionIds);
          client.close();
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
    [authenticatedClient, handleRefresh, setSafeError],
  );

  const handleForgetOfflineClient = useCallback(
    async (deviceId: UUID) => {
      if (forgettingClientIdsRef.current.has(deviceId)) {
        return;
      }
      setError(undefined);
      forgettingClientIdsRef.current.add(deviceId);
      setForgettingClientIds((current) => new Set(current).add(deviceId));
      let client: DirectClient | undefined;
      try {
        client = await authenticatedClient();
        await client.forgetDaemonClient(deviceId);
        setDaemonClients((current) => current.filter((candidate) => candidate.device_id !== deviceId));
      } catch (caught) {
        setSafeError(caught);
      } finally {
        client?.close();
        forgettingClientIdsRef.current.delete(deviceId);
        setForgettingClientIds((current) => {
          const next = new Set(current);
          next.delete(deviceId);
          return next;
        });
      }
    },
    [authenticatedClient, setSafeError],
  );

  const handleTerminalInput = useCallback(
    async (data: string) => {
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
      if (!terminalResizeOwnerRef.current) {
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
      // attach receive loop 收到 session_resized 后才会更新 state，避免和终端输出读取循环抢 socket。
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
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        await client.applySessionGitAction(sessionId, worktree.path, change.path, action);
        client.close();
        client = undefined;
        await loadSessionGit(sessionId);
      } catch (caught) {
        setSessionGitError(toSafeError(caught));
        setSessionGitLoading(false);
      } finally {
        client?.close();
      }
    },
    [authenticatedSessionClient, loadSessionGit],
  );

  const handleTerminalSearch = useCallback(
    async (query: string): Promise<SessionSearchResultPayload> => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        throw new ProtocolClientError("invalid_state", "no attached session");
      }
      let transientClient: DirectClient | undefined;
      try {
        transientClient = await authenticatedSessionClient(sessionId);
        return await transientClient.searchSessionOutput(sessionId, query, { maxResults: 80 });
      } finally {
        transientClient?.close();
      }
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
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const diff: SessionGitDiffResultPayload = await client.getSessionGitDiff(sessionId, worktree.path, change?.path, staged);
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
        client?.close();
      }
    },
    [authenticatedSessionClient, t],
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
      if (file.size > FILE_UPLOAD_MAX_BYTES) {
        setSessionFilesError({
          code: "file_too_large",
          message: "file is too large to upload in browser",
        });
        return;
      }
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        await client.writeSessionFile(sessionId, joinRemotePath(sessionFiles?.path ?? "", file.name), await fileToBytes(file));
        client.close();
        client = undefined;
        await loadSessionFiles(sessionId, sessionFiles?.path, { source: "manual" });
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
        setSessionFilesLoading(false);
      }
    },
    [authenticatedSessionClient, loadSessionFiles, sessionFiles?.path],
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
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const payload = await readEditableSessionFile(client, sessionId, entry.path);
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
        client?.close();
      }
    },
    [authenticatedSessionClient, t],
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
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        const written = await client.writeSessionFile(sessionId, editor.path, new TextEncoder().encode(text));
        client.close();
        client = undefined;
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
        client?.close();
      }
    },
    [authenticatedSessionClient, fileEditor, loadSessionFiles, sessionFiles?.path, t],
  );

  const handleDownloadFile = useCallback(
    async (entry: { name: string; path: string }) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionFilesError(undefined);
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        await downloadSessionFile(client, sessionId, entry.name, entry.path);
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
      }
    },
    [authenticatedSessionClient],
  );

  const handleDeleteFile = useCallback(
    async (entry: { path: string }) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let client: DirectClient | undefined;
      try {
        client = await authenticatedSessionClient(sessionId);
        await client.deleteSessionFile(sessionId, entry.path);
        client.close();
        client = undefined;
        await loadSessionFiles(sessionId, sessionFiles?.path, { source: "manual" });
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
        setSessionFilesLoading(false);
      }
    },
    [authenticatedSessionClient, loadSessionFiles, sessionFiles?.path],
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
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t("app.refresh")}
                    onClick={handleRefresh}
                    disabled={status === "listing"}
                  >
                    <RefreshCcw size={16} aria-hidden="true" />
                  </button>
                  <button type="button" className="icon-button" aria-label={t("app.disconnect")} onClick={handleDisconnectAttach} disabled={!attachedSessionId}>
                    <Unplug size={16} aria-hidden="true" />
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
              <>
                <div className="panel session-create" aria-label={t("app.newSession")}>
                  <button type="button" onClick={handleCreateSession} disabled={status === "creating"}>
                    <Plus size={16} aria-hidden="true" />
                    {t("app.newSession")}
                  </button>
                </div>
                <div className="panel-actions">
                  <button type="button" onClick={handleRefresh} disabled={status === "listing"}>
                    <RefreshCcw size={16} aria-hidden="true" />
                    {t("app.refresh")}
                  </button>
                  <button type="button" onClick={handleDisconnectAttach} disabled={!attachedSessionId}>
                    <Unplug size={16} aria-hidden="true" />
                    {t("app.disconnect")}
                  </button>
                </div>
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
              </>
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
              className="toolbar-title toolbar-title-button"
              aria-label={t("app.openSessionListFromTitle")}
              aria-expanded={showMobileSessionsPanel}
              onClick={handleOpenMobileSessions}
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
                theme={effectiveTheme}
                resizeEnabled={terminalResizeOwner}
                outputResetVersion={terminalOutputResetVersion}
                takeOutput={takeTerminalOutput}
                registerOutputDrain={registerTerminalOutputDrain}
                onOutputResetApplied={handleTerminalOutputResetApplied}
                onTerminalResync={handleTerminalResync}
                onTerminalSeqRendered={handleTerminalSeqRendered}
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

function useVisualViewportMetrics(enabled: boolean): { height: number; offsetTop: number; keyboardOpen: boolean } {
  const metricsFromWindow = () => {
    if (typeof window === "undefined") {
      return { height: 0, offsetTop: 0, keyboardOpen: false };
    }
    const viewport = window.visualViewport;
    const height = Math.round(viewport?.height ?? window.innerHeight);
    const offsetTop = Math.round(viewport?.offsetTop ?? 0);
    const keyboardInset = Math.max(0, Math.round(window.innerHeight - height - offsetTop));
    // 地址栏收缩也会改变 visualViewport，高度差超过常见工具栏后才按软键盘处理。
    return { height, offsetTop, keyboardOpen: keyboardInset >= 80 };
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
    : { height: typeof window === "undefined" ? 0 : window.innerHeight, offsetTop: 0, keyboardOpen: false };
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

function fileToBytes(file: File): Promise<Uint8Array> {
  if (typeof file.arrayBuffer === "function") {
    return file.arrayBuffer().then((buffer) => new Uint8Array(buffer));
  }

  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.addEventListener("load", () => {
      if (reader.result instanceof ArrayBuffer) {
        resolve(new Uint8Array(reader.result));
        return;
      }
      reject(new Error("invalid_file_data"));
    });
    reader.addEventListener("error", () => reject(reader.error ?? new Error("file_read_failed")));
    reader.readAsArrayBuffer(file);
  });
}

async function readEditableSessionFile(
  client: DirectClient,
  sessionId: UUID,
  path: string,
): Promise<{ path: string; bytes: Uint8Array }> {
  let offset = 0;
  let resolvedPath = path;
  let totalBytes = 0;
  const chunks: Uint8Array[] = [];

  while (true) {
    const chunk = await client.readSessionFileDownloadChunk(sessionId, path, offset, FILE_DOWNLOAD_CHUNK_BYTES);
    if (chunk.size_bytes > TEXT_FILE_EDITOR_MAX_BYTES) {
      throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
    }
    const bytes = sessionDataFromBase64(chunk.data_base64);
    if (bytes.includes(0)) {
      throw new ProtocolClientError("binary_file", "binary files cannot be edited in browser");
    }
    totalBytes += bytes.byteLength;
    if (totalBytes > TEXT_FILE_EDITOR_MAX_BYTES) {
      throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
    }
    chunks.push(bytes);
    resolvedPath = chunk.path;
    if (chunk.eof) {
      break;
    }
    if (chunk.next_offset_bytes <= offset) {
      // daemon 必须单调推进 offset，否则前端会无限循环等待同一个 chunk。
      throw new ProtocolClientError("invalid_file_chunk", "file chunk did not advance");
    }
    offset = chunk.next_offset_bytes;
  }

  return { path: resolvedPath, bytes: concatByteChunks(chunks) };
}

async function downloadSessionFile(
  client: DirectClient,
  sessionId: UUID,
  name: string,
  path: string,
): Promise<void> {
  const writer = await createDownloadWriter(name);
  if (writer) {
    let offset = 0;
    try {
      while (true) {
        const chunk = await client.readSessionFileDownloadChunk(sessionId, path, offset, FILE_DOWNLOAD_CHUNK_BYTES);
        const bytes = sessionDataFromBase64(chunk.data_base64);
        await writer.write(bytes);
        offset = chunk.next_offset_bytes;
        if (chunk.eof) {
          break;
        }
      }
    } finally {
      await writer.close();
    }
    return;
  }

  let offset = 0;
  let sizeBytes: number | undefined;
  const chunks: Uint8Array[] = [];
  while (true) {
    const chunk = await client.readSessionFileDownloadChunk(sessionId, path, offset, FILE_DOWNLOAD_CHUNK_BYTES);
    sizeBytes = chunk.size_bytes;
    if (sizeBytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
      throw new ProtocolClientError("file_too_large", "browser streaming download is unavailable for this file");
    }
    const bytes = sessionDataFromBase64(chunk.data_base64);
    chunks.push(bytes);
    offset = chunk.next_offset_bytes;
    if (chunk.eof) {
      break;
    }
  }
  triggerBrowserDownload(name, concatByteChunks(chunks));
}

async function createDownloadWriter(name: string): Promise<{ write: (bytes: Uint8Array) => Promise<void>; close: () => Promise<void> } | undefined> {
  const picker = (globalThis as {
    showSaveFilePicker?: (options?: { suggestedName?: string }) => Promise<{
      createWritable: () => Promise<{
        write: (data: Uint8Array) => Promise<void>;
        close: () => Promise<void>;
      }>;
    }>;
  }).showSaveFilePicker;
  if (!picker) {
    return undefined;
  }
  try {
    const handle = await picker({ suggestedName: name || "download" });
    const writable = await handle.createWritable();
    return {
      write: (bytes) => writable.write(bytes),
      close: () => writable.close(),
    };
  } catch (caught) {
    if (caught instanceof DOMException && caught.name === "AbortError") {
      throw new ProtocolClientError("download_cancelled", "download was cancelled");
    }
    return undefined;
  }
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
