import { useCallback, useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
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
  PairedServerState,
  SafeError,
  SessionCreatedPayload,
  SessionCursorPresence,
  SessionFilesResultPayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "./protocol/types";
import { decodeUtf8, sessionDataFromBase64 } from "./protocol/wire";
import {
  defaultServer,
  ensureDevice,
  loadBrowserState,
  normalizeRouteWsUrl,
  forgetDaemon,
  recordPairing,
  recordServerUrl,
  renameDaemon,
  selectDefaultServer,
} from "./state/browser-state";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { DaemonClientsPanel } from "./components/DaemonClientsPanel";
import { DaemonManagerPanel } from "./components/DaemonManagerPanel";
import { SessionList } from "./components/SessionList";
import { SessionFilesPanel } from "./components/SessionFilesPanel";
import { StatusBar } from "./components/StatusBar";
import { TerminalPane } from "./components/TerminalPane";
import { PairingQrScanner } from "./components/PairingQrScanner";
import { sessionDisplayName } from "./session-names";

const FALLBACK_WS_URL = "ws://127.0.0.1:8765/ws";
const DEFAULT_SESSION_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
const DEFAULT_FILES_PANEL_WIDTH = 286;
const MIN_FILES_PANEL_WIDTH = 240;
const MAX_FILES_PANEL_WIDTH = 640;
const FILES_PANEL_RESIZER_WIDTH = 10;
const CONNECTION_AUTO_RETRY_DELAY_MS = 1000;
const CONNECTION_AUTO_RETRY_LIMIT = 3;
const MOBILE_LAYOUT_QUERY = "(max-width: 760px)";
const MOBILE_LAYOUT_BREAKPOINT = 760;
type AppSurface = "admin" | "workspace";

export default function App() {
  const [state, setState] = useState<BrowserState>({ pairedServers: [] });
  const [url, setUrl] = useState(() => defaultWsUrlFromPage());
  const [pairingToken, setPairingToken] = useState("");
  const [sessions, setSessions] = useState<SessionSummaryPayload[]>([]);
  const [daemonClients, setDaemonClients] = useState<DaemonClientSummaryPayload[]>([]);
  const [forgettingClientIds, setForgettingClientIds] = useState<Set<UUID>>(() => new Set());
  const [clientsOpen, setClientsOpen] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [attachedSessionId, setAttachedSessionId] = useState<UUID | undefined>();
  const [renamingSessionId, setRenamingSessionId] = useState<UUID | undefined>();
  const [renameDraft, setRenameDraft] = useState("");
  const [renameOriginalName, setRenameOriginalName] = useState("");
  const [terminalOutputVersion, setTerminalOutputVersion] = useState(0);
  const [terminalOutputResetVersion, setTerminalOutputResetVersion] = useState(0);
  const [terminalFocusRequest, setTerminalFocusRequest] = useState(0);
  const [sessionFiles, setSessionFiles] = useState<SessionFilesResultPayload | undefined>();
  const [sessionFilesLoading, setSessionFilesLoading] = useState(false);
  const [sessionFilesError, setSessionFilesError] = useState<SafeError | undefined>();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [filesPanelOpen, setFilesPanelOpen] = useState(true);
  const [filesPanelWidth, setFilesPanelWidth] = useState(DEFAULT_FILES_PANEL_WIDTH);
  const [isFilesPanelResizing, setIsFilesPanelResizing] = useState(false);
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);
  const [mobilePanel, setMobilePanel] = useState<"sessions" | "files" | undefined>();
  const [connectionEditorOpen, setConnectionEditorOpen] = useState(false);
  const [qrScannerOpen, setQrScannerOpen] = useState(false);
  const [renamingDaemonId, setRenamingDaemonId] = useState<UUID | undefined>();
  const [daemonRenameDraft, setDaemonRenameDraft] = useState("");
  const [activeSurface, setActiveSurface] = useState<AppSurface>("admin");
  const [status, setStatus] = useState("idle");
  const [error, setError] = useState<SafeError | undefined>();
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const receiveLoopActiveRef = useRef(false);
  const closingSessionIdsRef = useRef<Set<UUID>>(new Set());
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
  const lastCursorReportRef = useRef("");
  const cursorRefreshTimerRef = useRef<number | undefined>(undefined);
  const terminalOutputQueueRef = useRef<string[]>([]);
  const terminalOutputResetVersionRef = useRef(0);
  const terminalOutputFlushFrameRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);
  const isMobileLayout = useMobileLayout();

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
    return () => {
      if (terminalOutputFlushFrameRef.current !== undefined) {
        window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
        terminalOutputFlushFrameRef.current = undefined;
      }
      if (connectionAutoRetryTimerRef.current !== undefined) {
        window.clearTimeout(connectionAutoRetryTimerRef.current);
        connectionAutoRetryTimerRef.current = undefined;
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
  const hasPairedServer = Boolean(activeServer && state.device);
  const showConnectionStatus = hasPairedServer && !error && status !== "pairing";
  const connectionReady =
    showConnectionStatus && status !== "idle" && status !== "connecting" && status !== "listing";
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
      return "No session";
    }
    return sessionDisplayName(toolbarSession);
  }, [sessions, toolbarSession]);
  const toolbarSessionSize = toolbarSession ? terminalSizeDisplay(toolbarSession.size) : undefined;
  const pairedServerOptions = useMemo(
    () =>
      state.pairedServers.map((server, index) => ({
        server,
        label: daemonDisplayLabel(server, index),
      })),
    [state.pairedServers],
  );
  const showMobileWorkspaceMenu = isMobileLayout && connectionReady;
  const showMobileSessionsPanel = showMobileWorkspaceMenu && mobilePanel === "sessions";
  const showMobileFilesPanel = showMobileWorkspaceMenu && mobilePanel === "files";
  const showDesktopFilesPanel = !isMobileLayout && filesPanelOpen;
  const desktopWorkspaceStyle =
    !isMobileLayout && showDesktopFilesPanel
      ? { gridTemplateColumns: `minmax(0, 1fr) ${FILES_PANEL_RESIZER_WIDTH}px ${filesPanelWidth}px` }
      : undefined;
  const canOpenWorkspace = Boolean(activeServer && state.device);
  const canSaveRename = Boolean(renameDraft.trim()) && renameDraft.trim() !== renameOriginalName.trim();
  const activeDaemonLabel =
    pairedServerOptions.find((item) => item.server.server_id === activeServer?.server_id)?.label ?? "No daemon";

  const handleOpenAdmin = useCallback((options: { editConnection?: boolean } = {}) => {
    setActiveSurface("admin");
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
    // 只有显式进入连接编辑时才保留编辑器，普通返回管理页时收起它。
    setConnectionEditorOpen(Boolean(options.editConnection));
  }, []);

  const handleOpenWorkspace = useCallback(() => {
    if (!activeServer || !state.device) {
      return;
    }
    setError(undefined);
    setActiveSurface("workspace");
    setConnectionEditorOpen(false);
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
    if (status === "error" || status === "idle") {
      // 从后台重新进入工作台时允许对当前 daemon 再做一次连通性探测。
      autoCheckedServerRef.current = undefined;
      setStatus("idle");
    }
  }, [activeServer, state.device, status]);

  const setSafeError = useCallback((caught: unknown) => {
    setError(toSafeError(caught));
    setStatus("error");
  }, []);

  const isIgnoredClosingSessionNotFound = useCallback((sessionId: UUID, caught: unknown) => {
    if (!closingSessionIdsRef.current.has(sessionId)) {
      return false;
    }
    return toSafeError(caught).code === "session_not_found";
  }, []);

  const clearSessionFiles = useCallback(() => {
    setSessionFiles(undefined);
    setSessionFilesError(undefined);
    setSessionFilesLoading(false);
  }, []);

  const clearTerminalOutput = useCallback(() => {
    // 终端输出由 xterm 自己维护 scrollback；React 只保留尚未写入 xterm 的短队列。
    terminalOutputQueueRef.current = [];
    terminalOutputResetVersionRef.current += 1;
    if (terminalOutputFlushFrameRef.current !== undefined) {
      window.cancelAnimationFrame(terminalOutputFlushFrameRef.current);
      terminalOutputFlushFrameRef.current = undefined;
    }
    setTerminalOutputResetVersion(terminalOutputResetVersionRef.current);
  }, []);

  const flushTerminalOutput = useCallback(() => {
    terminalOutputFlushFrameRef.current = undefined;
    // 这一帧里累积的 session_data 会在 xterm 里一次性写入，避免每个 chunk 都触发一次 React 更新。
    setTerminalOutputVersion((version) => version + 1);
  }, []);

  const scheduleTerminalOutputFlush = useCallback(() => {
    if (terminalOutputFlushFrameRef.current !== undefined) {
      return;
    }
    terminalOutputFlushFrameRef.current = window.requestAnimationFrame(() => {
      flushTerminalOutput();
    });
  }, [flushTerminalOutput]);

  const enqueueTerminalOutput = useCallback((chunk: string) => {
    terminalOutputQueueRef.current.push(chunk);
    scheduleTerminalOutputFlush();
  }, [scheduleTerminalOutputFlush]);

  const takeTerminalOutput = useCallback(() => {
    const chunks = terminalOutputQueueRef.current;
    terminalOutputQueueRef.current = [];
    return chunks;
  }, []);

  const disconnectAttach = useCallback(() => {
    receiveLoopActiveRef.current = false;
    attachClientRef.current?.close();
    attachClientRef.current = undefined;
    attachedSessionRef.current = undefined;
    setAttachedSessionId(undefined);
    lastCursorReportRef.current = "";
    if (cursorRefreshTimerRef.current !== undefined) {
      window.clearTimeout(cursorRefreshTimerRef.current);
      cursorRefreshTimerRef.current = undefined;
    }
    clearTerminalOutput();
    clearSessionFiles();
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
  }, [clearSessionFiles, clearTerminalOutput]);

  const resetWorkspaceState = useCallback(() => {
    setSessions([]);
    setDaemonClients([]);
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
      const rawCandidateUrl = payload?.ws_url ?? (url.trim() || activeServer?.url || defaultWsUrlFromPage());
      const candidateUrls = pairingWsUrlCandidates(rawCandidateUrl, routeServerId);
      const token = payload?.token ?? pairingInput.trim();
      const { client, effectiveUrl } = await connectPairingClient(candidateUrls, routeServerId, device.device_id);
      const accepted = await client.pair(token, device.device_public_key);
      client.close();
      const nextState = await recordPairing(accepted, effectiveUrl);
      setState(nextState);
      setPairingToken("");
      setConnectionEditorOpen(false);
      setSessions([]);
      setDaemonClients([]);
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
      client = await DirectClient.connect(effectiveUrl, server.server_id, device.device_id);
      await client.authenticate(device, { ...server, url: effectiveUrl });
      client.close();
      client = undefined;
      disconnectAttach();
      const nextState = await recordServerUrl(server.server_id, effectiveUrl);
      setState(nextState);
      setSessions([]);
      setDaemonClients([]);
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
      setDaemonClients([]);
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

  const authenticatedClient = useCallback(async () => {
    const server = activeServer;
    const device = state.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    const reachableUrl = browserReachableWsUrl(server.url);
    const routeUrl = routeWsUrlForKnownServer(reachableUrl, server.server_id) ?? reachableUrl;
    const client = await DirectClient.connect(routeUrl, server.server_id, device.device_id);
    await client.authenticate(device, { ...server, url: routeUrl });
    return client;
  }, [activeServer, state.device]);

  const loadSessionFiles = useCallback(
    async (sessionId: UUID, path?: string) => {
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let client: DirectClient | undefined;
      try {
        client = await authenticatedClient();
        // 文件树当前位置是 daemon 端 session 共享状态；不传 path 时由 daemon 返回当前共享目录。
        const files = await client.listSessionFiles(sessionId, path);
        setSessionFiles(files);
      } catch (caught) {
        // 文件列表是终端旁路信息；失败时只收敛到右侧 panel，不打断已 attach 的终端会话。
        setSessionFiles(undefined);
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
        setSessionFilesLoading(false);
      }
    },
    [authenticatedClient],
  );

  const handleRefresh = useCallback(async () => {
    setError(undefined);
    setStatus("listing");
    try {
      const client = await authenticatedClient();
      const list = await client.listSessions();
      const clients = await client.listDaemonClients();
      client.close();
      const firstSessionId =
        sortSessionsNewestFirst(list.sessions).at(0)?.session_id ??
        renamingSessionIdRef.current ??
        attachedSessionRef.current;
      setSessions((current) =>
        mergeSessionRefresh(list.sessions, current, [
          renamingSessionIdRef.current,
          attachedSessionRef.current,
        ]),
      );
      setDaemonClients(clients.clients);
      setSelectedSessionId(firstSessionId);
      // session 列表刷新可能来自后台轮询或 cursor 同步，不能打断用户正在编辑的标题。
      clearSessionFiles();
      setStatus("ready");
    } catch (caught) {
      setActiveSurface("admin");
      setSafeError(caught);
    }
  }, [authenticatedClient, clearSessionFiles, setSafeError]);

  const refreshDaemonClients = useCallback(
    async () => {
      try {
        const client = await authenticatedClient();
        try {
          const sessionList = await client.listSessions();
          const clientList = await client.listDaemonClients();
          setSessions((current) =>
            mergeSessionRefresh(sessionList.sessions, current, [
              renamingSessionIdRef.current,
              attachedSessionRef.current,
            ]),
          );
          setDaemonClients(clientList.clients);
        } finally {
          client.close();
        }
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, setSafeError],
  );

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
            const payload = inner.payload as { data_base64: string };
            enqueueTerminalOutput(decodeUtf8(sessionDataFromBase64(payload.data_base64)));
          } else if (inner.type === "session_files_result") {
            const payload = inner.payload as SessionFilesResultPayload;
            // daemon 主动推送的文件树状态和当前 attach 的 session 对齐后才更新右侧 panel。
            if (payload.session_id === attachedSessionRef.current) {
              setSessionFiles(payload);
              setSessionFilesError(undefined);
              setSessionFilesLoading(false);
            }
          }
        } catch (caught) {
          if (receiveLoopActiveRef.current) {
            setSafeError(caught);
          }
          return;
        }
      }
    };
    void read();
  }, [enqueueTerminalOutput, setSafeError]);

  const handleAttach = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      setStatus("attaching");
      try {
        if (attachedSessionRef.current === sessionId && attachClientRef.current) {
          setSelectedSessionId(sessionId);
          setStatus("attached");
          setMobilePanel(undefined);
          setMobileMenuOpen(false);
          return;
        }
        disconnectAttach();
        clearTerminalOutput();
        const client = await authenticatedClient();
        await client.attachSession(sessionId);
        attachClientRef.current = client;
        attachedSessionRef.current = sessionId;
        setSelectedSessionId(sessionId);
        setAttachedSessionId(sessionId);
        setMobilePanel(undefined);
        setMobileMenuOpen(false);
        setStatus("attached");
        await loadSessionFiles(sessionId);
        void refreshDaemonClients();
        startReceiveLoop(client);
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, clearTerminalOutput, disconnectAttach, loadSessionFiles, refreshDaemonClients, setSafeError, startReceiveLoop],
  );

  const handleCreateSession = useCallback(async () => {
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
      setSelectedSessionId(created.session_id);
      setAttachedSessionId(created.session_id);
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      setSessions((current) => upsertSession(current, created));
      // 新建 session 等价于打开一个新的 SSH shell，应立即把输入焦点交给 xterm。
      // 普通打开历史 session 仍保持 viewer 逻辑，避免意外接管其他客户端的 PTY 尺寸。
      setTerminalFocusRequest((request) => request + 1);
      setStatus("attached");
      await loadSessionFiles(created.session_id);
      void refreshDaemonClients();
      startReceiveLoop(client);
    } catch (caught) {
      setSafeError(caught);
    }
  }, [authenticatedClient, clearTerminalOutput, disconnectAttach, loadSessionFiles, refreshDaemonClients, setSafeError, startReceiveLoop]);

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

    if (!error || !hasPairedServer) {
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
  }, [activeServer?.server_id, attachedSessionId, error, handleRetryConnection, hasPairedServer, selectedSessionId]);

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
      try {
        const client = await authenticatedClient();
        const renamed = await client.renameSession(sessionId, nextName);
        client.close();
        setSessions((current) =>
          current.map((session) =>
            session.session_id === renamed.session_id ? { ...session, name: renamed.name } : session,
          ),
        );
        handleCancelRename();
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, handleCancelRename, renameDraft, renameOriginalName, setSafeError],
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
      isIgnoredClosingSessionNotFound,
      refreshDaemonClients,
      selectedSessionId,
      setSafeError,
    ],
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
      let shouldSendResize = false;
      setSessions((current) => {
        const next = current.map((session) =>
          session.session_id === sessionId ? { ...session, size } : session,
        );
        const currentSession = current.find((session) => session.session_id === sessionId);
        if (
          currentSession &&
          currentSession.size.rows === size.rows &&
          currentSession.size.cols === size.cols &&
          currentSession.size.pixel_width === size.pixel_width &&
          currentSession.size.pixel_height === size.pixel_height
        ) {
          return current;
        }
        shouldSendResize = true;
        return next;
      });
      if (!shouldSendResize) {
        return;
      }
      void client.resizeSession(sessionId, size).catch((caught) => {
        if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
          setSafeError(caught);
        }
      });
    },
    [isIgnoredClosingSessionNotFound, setSafeError],
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
      void client.sendSessionCursor(sessionId, presence).catch((caught) => {
        if (!isIgnoredClosingSessionNotFound(sessionId, caught)) {
          setSafeError(caught);
        }
      });
      if (cursorRefreshTimerRef.current === undefined) {
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

  const handleOpenDirectory = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      void loadSessionFiles(sessionId, path);
    },
    [loadSessionFiles],
  );

  const handleGoToFilePath = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      void loadSessionFiles(sessionId, resolveRemoteDirectoryPath(sessionFiles?.path ?? "", path));
    },
    [loadSessionFiles, sessionFiles?.path],
  );

  const handleUploadFile = useCallback(
    async (file: File) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let client: DirectClient | undefined;
      try {
        client = await authenticatedClient();
        await client.writeSessionFile(sessionId, joinRemotePath(sessionFiles?.path ?? "", file.name), await fileToBytes(file));
        client.close();
        client = undefined;
        await loadSessionFiles(sessionId, sessionFiles?.path);
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
        setSessionFilesLoading(false);
      }
    },
    [authenticatedClient, loadSessionFiles, sessionFiles?.path],
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
        client = await authenticatedClient();
        const payload = await client.readSessionFile(sessionId, entry.path);
        triggerBrowserDownload(entry.name, payload.data_base64);
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
      }
    },
    [authenticatedClient],
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
        client = await authenticatedClient();
        await client.deleteSessionFile(sessionId, entry.path);
        client.close();
        client = undefined;
        await loadSessionFiles(sessionId, sessionFiles?.path);
      } catch (caught) {
        setSessionFilesError(toSafeError(caught));
      } finally {
        client?.close();
        setSessionFilesLoading(false);
      }
    },
    [authenticatedClient, loadSessionFiles, sessionFiles?.path],
  );

  const handleHideFiles = useCallback(() => {
    if (isMobileLayout) {
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      return;
    }
    setFilesPanelOpen(false);
  }, [isMobileLayout]);

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
  }, []);

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
      <div className="admin-shell">
        <header className="admin-topbar">
          <div className="admin-brand">
            <Cable size={18} aria-hidden="true" />
            <span>termd admin</span>
          </div>
          <div className="admin-topbar-actions">
            <button type="button" onClick={handleOpenWorkspace} disabled={!canOpenWorkspace}>
              <MonitorUp size={16} aria-hidden="true" />
              Workspace
            </button>
          </div>
        </header>
        <main className="admin-main" aria-label="daemon admin">
          <section className="admin-summary-band" aria-label="selected daemon">
            <div className="admin-summary-main">
              <span>Selected daemon</span>
              <strong>{activeDaemonLabel}</strong>
              <code>{activeServer?.url ?? "unpaired"}</code>
            </div>
            <button type="button" onClick={handleOpenWorkspace} disabled={!canOpenWorkspace}>
              <MonitorUp size={16} aria-hidden="true" />
              Open workspace
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
        </main>
        <StatusBar status={status} error={error} sessionId={attachedSessionId ?? selectedSessionId} />
      </div>
    );
  }

  return (
    <div
      className={[
        "app-shell",
        "workspace-surface",
        sidebarCollapsed ? "sidebar-is-collapsed" : "",
        connectionReady ? "connection-ready" : "",
        isFilesPanelResizing ? "files-panel-resizing" : "",
        mobileMenuOpen ? "mobile-menu-open" : "",
        mobilePanel ? `mobile-panel-${mobilePanel}` : "",
      ]
        .filter(Boolean)
        .join(" ")}
    >
      {mobileMenuOpen ? (
        <button
          type="button"
          className="mobile-backdrop mobile-menu-backdrop"
          aria-label="Close mobile workspace menu"
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
                aria-label="Expand sidebar"
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
                    aria-label="New session"
                    onClick={handleCreateSession}
                    disabled={status === "creating"}
                  >
                    <Plus size={16} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label="Refresh"
                    onClick={handleRefresh}
                    disabled={status === "listing"}
                  >
                    <RefreshCcw size={16} aria-hidden="true" />
                  </button>
                  <button type="button" className="icon-button" aria-label="Disconnect" onClick={disconnectAttach} disabled={!attachedSessionId}>
                    <Unplug size={16} aria-hidden="true" />
                  </button>
                </div>
                <section className="collapsed-session-list" aria-label="collapsed sessions">
                  {sessions.map((session) => (
                    <button
                      type="button"
                      key={session.session_id}
                      className={session.session_id === selectedSessionId ? "icon-button selected-session-dot" : "icon-button"}
                      aria-label={`Select ${sessionDisplayName(session)}`}
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
                <span>termd</span>
              </div>
              <button
                type="button"
                className="icon-button sidebar-collapse-toggle"
                aria-label="Collapse sidebar"
                onClick={() => setSidebarCollapsed(true)}
              >
                <PanelLeftClose size={16} aria-hidden="true" />
              </button>
            </div>
            {!isMobileLayout && connectionReady ? (
              <>
                <div className="panel session-create" aria-label="new session">
                  <button type="button" onClick={handleCreateSession} disabled={status === "creating"}>
                    <Plus size={16} aria-hidden="true" />
                    New session
                  </button>
                </div>
                <div className="panel-actions">
                  <button type="button" onClick={handleRefresh} disabled={status === "listing"}>
                    <RefreshCcw size={16} aria-hidden="true" />
                    Refresh
                  </button>
                  <button type="button" onClick={disconnectAttach} disabled={!attachedSessionId}>
                    <Unplug size={16} aria-hidden="true" />
                    Disconnect
                  </button>
                </div>
                <SessionList
                  sessions={sessions}
                  selectedSessionId={selectedSessionId}
                  renamingSessionId={renamingSessionId}
                  renameDraft={renameDraft}
                  canSaveRename={canSaveRename}
                  onAttach={handleAttach}
                  onStartRename={handleStartRename}
                  onRenameDraftChange={setRenameDraft}
                  onSaveRename={handleSaveRename}
                  onCancelRename={handleCancelRename}
                  onClose={handleCloseSession}
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
              aria-label="Open mobile workspace menu"
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
              aria-label="Open session list from title"
              aria-expanded={showMobileSessionsPanel}
              onClick={handleOpenMobileSessions}
            >
              <MonitorUp size={16} aria-hidden="true" />
              <span>{toolbarSessionName}</span>
              {toolbarSessionSize ? <small>{toolbarSessionSize}</small> : null}
            </button>
          ) : (
            <div className="toolbar-title">
              <MonitorUp size={16} aria-hidden="true" />
              <span>{toolbarSessionName}</span>
              {toolbarSessionSize ? <small>{toolbarSessionSize}</small> : null}
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
                aria-label="Clients"
                aria-controls="daemon-clients-popover"
                aria-expanded={clientsOpen}
                onClick={() => setClientsOpen((open) => !open)}
              >
                <UsersRound size={16} aria-hidden="true" />
                Clients
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
                Daemons
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
                outputVersion={terminalOutputVersion}
                outputResetVersion={terminalOutputResetVersion}
                takeOutput={takeTerminalOutput}
                onInput={handleTerminalInput}
                onResize={handleResize}
                onCursorChange={handleCursorChange}
              />
              {showDesktopFilesPanel ? (
                <>
                  <div
                    className="files-resizer"
                    role="separator"
                    aria-label="Resize files panel"
                    aria-orientation="vertical"
                    tabIndex={0}
                    onPointerDown={handleFilesPanelResizePointerDown}
                    onKeyDown={handleFilesPanelResizeKeyDown}
                  />
                  <SessionFilesPanel
                    attachedSessionId={attachedSessionId}
                    files={sessionFiles}
                    loading={sessionFilesLoading}
                    error={sessionFilesError}
                    onOpenDirectory={handleOpenDirectory}
                    onGoToPath={handleGoToFilePath}
                    onUpload={handleUploadFile}
                    onDownload={handleDownloadFile}
                    onDelete={handleDeleteFile}
                    onHide={handleHideFiles}
                  />
                </>
              ) : !isMobileLayout ? (
                <aside className="files-rail" aria-label="files panel collapsed">
                  <button type="button" className="icon-button" aria-label="Show files panel" onClick={() => setFilesPanelOpen(true)}>
                    <PanelRightOpen size={16} aria-hidden="true" />
                  </button>
                </aside>
              ) : null}
            </>
          ) : (
            <div className="terminal-pane" aria-label="terminal unavailable">
              <div className="terminal-placeholder">disconnected</div>
            </div>
          )}
        </div>
        {showMobileWorkspaceMenu && mobileMenuOpen ? (
          <nav className="mobile-menu-popover" aria-label="mobile workspace menu">
            <button type="button" onClick={() => handleOpenAdmin()}>
              <Server size={16} aria-hidden="true" />
              Daemons
            </button>
            <button type="button" onClick={handleOpenMobileSessions}>
              <MonitorUp size={16} aria-hidden="true" />
              Sessions
            </button>
            <button type="button" onClick={handleOpenMobileFiles} disabled={!attachedSessionId}>
              <Folder size={16} aria-hidden="true" />
              Files
            </button>
            <button type="button" onClick={handleOpenMobileNewSession} disabled={status === "creating"}>
              <Plus size={16} aria-hidden="true" />
              New
            </button>
          </nav>
        ) : null}
        {showMobileSessionsPanel ? (
          <section className="mobile-panel mobile-sessions-panel" aria-label="sessions panel">
            <header className="mobile-panel-header">
              <div className="mobile-panel-title">
                <MonitorUp size={15} aria-hidden="true" />
                <span>Sessions</span>
              </div>
              <div className="mobile-panel-actions">
                <button
                  type="button"
                  className="icon-button"
                  aria-label="Refresh sessions"
                  onClick={handleRefresh}
                  disabled={status === "listing"}
                >
                  <RefreshCcw size={15} aria-hidden="true" />
                </button>
                <button type="button" className="icon-button" aria-label="Close sessions panel" onClick={handleCloseMobilePanel}>
                  <X size={15} aria-hidden="true" />
                </button>
              </div>
            </header>
            <div className="mobile-panel-body">
              <SessionList
                sessions={sessions}
                selectedSessionId={selectedSessionId}
                renamingSessionId={renamingSessionId}
                renameDraft={renameDraft}
                canSaveRename={canSaveRename}
                onAttach={handleAttach}
                onStartRename={handleStartRename}
                onRenameDraftChange={setRenameDraft}
                onSaveRename={handleSaveRename}
                onCancelRename={handleCancelRename}
                onClose={handleCloseSession}
              />
            </div>
          </section>
        ) : null}
        {showMobileFilesPanel ? (
          <div className="mobile-panel mobile-files-panel">
            <SessionFilesPanel
              attachedSessionId={attachedSessionId}
              files={sessionFiles}
              loading={sessionFilesLoading}
              error={sessionFilesError}
              onOpenDirectory={handleOpenDirectory}
              onGoToPath={handleGoToFilePath}
              onUpload={handleUploadFile}
              onDownload={handleDownloadFile}
              onDelete={handleDeleteFile}
              onHide={handleHideFiles}
            />
          </div>
        ) : null}
        <StatusBar status={status} error={error} sessionId={attachedSessionId ?? selectedSessionId} />
      </main>
    </div>
  );
}

function ProtocolErrorAlert(props: {
  error: SafeError;
  onRefresh?: () => void;
  refreshing?: boolean;
}) {
  return (
    <section className="protocol-error-alert" role="alert" aria-label="Connection error">
      <div className="protocol-error-alert-title">
        <CircleAlert size={17} aria-hidden="true" />
        <span>Connection error</span>
        {props.onRefresh ? (
          <button
            type="button"
            className="protocol-error-refresh"
            onClick={props.onRefresh}
            disabled={props.refreshing}
          >
            <RefreshCcw size={15} aria-hidden="true" />
            Refresh
          </button>
        ) : null}
      </div>
      <div className="protocol-error-alert-detail">
        <code>{props.error.code}</code>
        {/* 主体提示只展示 SafeError 字段，避免把 token、签名或密文等原始 payload 泄漏到 UI。 */}
        <span>{props.error.message}</span>
      </div>
    </section>
  );
}

function SessionOperatorsBar(props: {
  operators: DaemonClientSummaryPayload[];
  currentDeviceId?: UUID;
  sessionId: UUID;
}) {
  return (
    <div className="session-operators" aria-label="session operators">
      <div className="session-operators-title">
        <UsersRound size={15} aria-hidden="true" />
        <span>{props.operators.length}</span>
      </div>
      {props.operators.length === 0 ? (
        <span className="session-operator muted">no operators</span>
      ) : (
        props.operators.map((client) => {
          const isCurrentDevice = client.device_id === props.currentDeviceId;
          const label = client.name?.trim() || client.peer_ip || "Client";
          const cursor =
            client.cursor_session_id === props.sessionId && client.cursor_row && client.cursor_col
              ? `${client.cursor_row}:${client.cursor_col}`
              : "cursor ?";
          const focus =
            client.cursor_session_id === props.sessionId && client.cursor_focused !== undefined && client.cursor_focused !== null
              ? client.cursor_focused
                ? "focused"
                : "blurred"
              : undefined;
          return (
            <span className="session-operator" key={client.client_id} title={label}>
              <span className="status-dot online" aria-hidden="true" />
              <span>{label}</span>
              {isCurrentDevice ? <span>you</span> : null}
              <span className="session-operator-cursor">{cursor}</span>
              {focus ? <span className={client.cursor_focused ? "focus-chip focused" : "focus-chip"}>{focus}</span> : null}
            </span>
          );
        })
      )}
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
): Promise<{ client: DirectClient; effectiveUrl: string }> {
  if (!routeServerId) {
    throw new ProtocolClientError("pairing_server_unknown", "pairing requires a known daemon server id");
  }
  let lastError: unknown;
  for (const candidateUrl of candidateUrls) {
    try {
      const client = await DirectClient.connect(candidateUrl, routeServerId, deviceId);
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

function daemonDisplayLabel(server: PairedServerState, index: number): string {
  const name = server.name?.trim();
  if (name) {
    return name;
  }
  try {
    const parsed = new URL(server.url);
    return `Daemon ${index + 1} ${parsed.host}`;
  } catch {
    return `Daemon ${index + 1}`;
  }
}

function terminalSizeDisplay(size: TerminalSize): string {
  return `${size.cols}x${size.rows}`;
}

function sortSessionsNewestFirst(sessions: SessionSummaryPayload[]): SessionSummaryPayload[] {
  return [...sessions].sort((left, right) => sessionCreatedAt(right) - sessionCreatedAt(left));
}

function mergeSessionRefresh(
  remoteSessions: SessionSummaryPayload[],
  currentSessions: SessionSummaryPayload[],
  preserveSessionIds: Array<UUID | undefined>,
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

  for (const sessionId of preserveSessionIds) {
    if (!sessionId || remoteIds.has(sessionId)) {
      continue;
    }
    const current = currentById.get(sessionId);
    if (current) {
      // 正在编辑或 attach 的 session 可能被更早发出的旧 session_list 暂时漏掉；
      // 先保留本地行，下一次权威刷新或保存/关闭结果会再收敛。
      next.push(current);
    }
  }

  return sortSessionsNewestFirst(next);
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

function triggerBrowserDownload(name: string, dataBase64: string): void {
  if (typeof navigator !== "undefined" && navigator.userAgent.toLowerCase().includes("jsdom")) {
    return;
  }
  if (typeof URL.createObjectURL !== "function") {
    return;
  }
  const bytes = Uint8Array.from(sessionDataFromBase64(dataBase64));
  const blob = new Blob([bytes.buffer], { type: "application/octet-stream" });
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

function upsertSession(current: SessionSummaryPayload[], session: SessionCreatedPayload): SessionSummaryPayload[] {
  const next = {
    session_id: session.session_id,
    name: session.name ?? null,
    state: session.state,
    size: session.size,
    created_at_ms: Date.now(),
  };
  return sortSessionsNewestFirst([next, ...current.filter((candidate) => candidate.session_id !== session.session_id)]);
}
