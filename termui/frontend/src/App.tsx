import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Cable,
  Folder,
  Link,
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

const FALLBACK_WS_URL = "ws://127.0.0.1:8765/ws";
const DEFAULT_SESSION_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
const MOBILE_LAYOUT_QUERY = "(max-width: 760px)";
const MOBILE_LAYOUT_BREAKPOINT = 760;
type AppSurface = "admin" | "workspace";

export default function App() {
  const [state, setState] = useState<BrowserState>({ pairedServers: [] });
  const [url, setUrl] = useState(() => defaultWsUrlFromPage());
  const [pairingToken, setPairingToken] = useState("");
  const [sessions, setSessions] = useState<SessionSummaryPayload[]>([]);
  const [daemonClients, setDaemonClients] = useState<DaemonClientSummaryPayload[]>([]);
  const [clientsOpen, setClientsOpen] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [attachedSessionId, setAttachedSessionId] = useState<UUID | undefined>();
  const [renamingSessionId, setRenamingSessionId] = useState<UUID | undefined>();
  const [renameDraft, setRenameDraft] = useState("");
  const [terminalChunks, setTerminalChunks] = useState<string[]>([]);
  const [sessionFiles, setSessionFiles] = useState<SessionFilesResultPayload | undefined>();
  const [sessionFilesLoading, setSessionFilesLoading] = useState(false);
  const [sessionFilesError, setSessionFilesError] = useState<SafeError | undefined>();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [filesPanelOpen, setFilesPanelOpen] = useState(true);
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
  const urlTouchedRef = useRef(false);
  const autoCheckedServerRef = useRef<UUID | undefined>(undefined);
  const lastCursorReportRef = useRef("");
  const cursorRefreshTimerRef = useRef<number | undefined>(undefined);
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
  const canOpenWorkspace = Boolean(activeServer && state.device);
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

  const clearSessionFiles = useCallback(() => {
    setSessionFiles(undefined);
    setSessionFilesError(undefined);
    setSessionFilesLoading(false);
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
    clearSessionFiles();
    setMobilePanel(undefined);
    setMobileMenuOpen(false);
  }, [clearSessionFiles]);

  const resetWorkspaceState = useCallback(() => {
    setSessions([]);
    setDaemonClients([]);
    setSelectedSessionId(undefined);
    setRenamingSessionId(undefined);
    setRenameDraft("");
    setTerminalChunks([]);
    clearSessionFiles();
    autoCheckedServerRef.current = undefined;
  }, [clearSessionFiles]);

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
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setTerminalChunks([]);
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      disconnectAttach();
      if (payload) {
        setUrl(effectiveUrl);
      }
      setActiveSurface("workspace");
      setStatus("paired");
    } catch (caught) {
      setPairingToken("");
      setSafeError(caught);
    }
  }, [activeServer, disconnectAttach, pairingToken, setSafeError, url]);

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
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setTerminalChunks([]);
      setConnectionEditorOpen(false);
      autoCheckedServerRef.current = undefined;
      setActiveSurface("workspace");
      setStatus("ready");
    } catch (caught) {
      setSafeError(caught);
    } finally {
      client?.close();
    }
  }, [activeServer, disconnectAttach, setSafeError, state.device, url]);

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
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setTerminalChunks([]);
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
    [activeServer?.server_id, disconnectAttach, state.pairedServers],
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
      const firstSessionId = list.sessions.at(0)?.session_id;
      setSessions(list.sessions);
      setDaemonClients(clients.clients);
      setSelectedSessionId(firstSessionId);
      setRenamingSessionId(undefined);
      setRenameDraft("");
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
          setSessions(sessionList.sessions);
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
            setTerminalChunks((chunks) => [...chunks, decodeUtf8(sessionDataFromBase64(payload.data_base64))]);
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
  }, [setSafeError]);

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
        setTerminalChunks([]);
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
    [authenticatedClient, disconnectAttach, loadSessionFiles, refreshDaemonClients, setSafeError, startReceiveLoop],
  );

  const handleCreateSession = useCallback(async () => {
    setError(undefined);
    disconnectAttach();
    setTerminalChunks([]);
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
      setStatus("attached");
      await loadSessionFiles(created.session_id);
      void refreshDaemonClients();
      startReceiveLoop(client);
    } catch (caught) {
      setSafeError(caught);
    }
  }, [authenticatedClient, disconnectAttach, loadSessionFiles, refreshDaemonClients, setSafeError, startReceiveLoop]);

  const handleStartRename = useCallback((sessionId: UUID, currentName: string) => {
    setRenamingSessionId(sessionId);
    setRenameDraft(currentName);
  }, []);

  const handleCancelRename = useCallback(() => {
    setRenamingSessionId(undefined);
    setRenameDraft("");
  }, []);

  const handleSaveRename = useCallback(
    async (sessionId: UUID) => {
      const nextName = renameDraft.trim();
      if (!nextName) {
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
    [authenticatedClient, handleCancelRename, renameDraft, setSafeError],
  );

  const handleCloseSession = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      try {
        const client = await authenticatedClient();
        await client.closeSession(sessionId);
        client.close();
      setSessions((current) => current.filter((session) => session.session_id !== sessionId));
      if (selectedSessionId === sessionId) {
        setSelectedSessionId(undefined);
        clearSessionFiles();
      }
      if (attachedSessionRef.current === sessionId) {
        disconnectAttach();
        setTerminalChunks([]);
      }
      setMobilePanel(undefined);
      setMobileMenuOpen(false);
      void refreshDaemonClients();
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, clearSessionFiles, disconnectAttach, refreshDaemonClients, selectedSessionId, setSafeError],
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
        setSafeError(caught);
      }
    },
    [setSafeError],
  );

  const handleResize = useCallback(
    (size: { rows: number; cols: number; pixel_width: number; pixel_height: number }) => {
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId) {
        return;
      }
      setSessions((current) =>
        current.map((session) => (session.session_id === sessionId ? { ...session, size } : session)),
      );
      void client.resizeSession(sessionId, size).catch(setSafeError);
    },
    [setSafeError],
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
      void client.sendSessionCursor(sessionId, presence).catch(setSafeError);
      if (cursorRefreshTimerRef.current === undefined) {
        cursorRefreshTimerRef.current = window.setTimeout(() => {
          cursorRefreshTimerRef.current = undefined;
          void refreshDaemonClients();
        }, 500);
      }
    },
    [refreshDaemonClients, setSafeError],
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
                    aria-label="Clients"
                    aria-controls="daemon-clients-popover"
                    aria-expanded={clientsOpen}
                    onClick={() => setClientsOpen((open) => !open)}
                  >
                    <UsersRound size={15} aria-hidden="true" />
                  </button>
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
                {clientsOpen ? (
                  <div className="clients-popover rail-popover" id="daemon-clients-popover">
                    <DaemonClientsPanel clients={daemonClients} />
                  </div>
                ) : null}
                <section className="collapsed-session-list" aria-label="collapsed sessions">
                  {sessions.map((session) => (
                    <button
                      type="button"
                      key={session.session_id}
                      className={session.session_id === selectedSessionId ? "icon-button selected-session-dot" : "icon-button"}
                      aria-label={`Select session ${session.name?.trim() || shortSessionId(session.session_id)}`}
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
              {connectionReady ? (
                <button
                  type="button"
                  className="icon-button clients-toggle"
                  aria-label="Clients"
                  aria-controls="daemon-clients-popover"
                  aria-expanded={clientsOpen}
                  onClick={() => setClientsOpen((open) => !open)}
                >
                  <UsersRound size={15} aria-hidden="true" />
                </button>
              ) : null}
              {connectionReady && clientsOpen ? (
                <div className="clients-popover" id="daemon-clients-popover">
                  <DaemonClientsPanel clients={daemonClients} />
                </div>
              ) : null}
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
          <div className="toolbar-group">
            <Link size={16} aria-hidden="true" />
            <span>{connectionReady ? "Connected" : activeServer ? "paired daemon" : "unpaired"}</span>
            {activeServer && connectionReady ? <small>paired daemon</small> : null}
          </div>
          <button type="button" className="toolbar-admin-button" onClick={() => handleOpenAdmin()}>
            <Server size={16} aria-hidden="true" />
            Daemons
          </button>
          {connectionReady && attachedSessionId ? (
            <SessionOperatorsBar
              operators={sessionOperators}
              currentDeviceId={state.device?.device_id}
              sessionId={attachedSessionId}
            />
          ) : null}
        </div>
        <div
          className={
            isMobileLayout
              ? "workspace-body workspace-body-mobile"
              : filesPanelOpen
                ? "workspace-body"
                : "workspace-body files-panel-hidden"
          }
        >
          {connectionReady ? (
            <>
              <TerminalPane
                chunks={terminalChunks}
                attached={Boolean(attachedSessionId)}
                sessionSize={attachedSession?.size}
                onInput={handleTerminalInput}
                onResize={handleResize}
                onCursorChange={handleCursorChange}
              />
              {showDesktopFilesPanel ? (
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
            <span className="session-operator" key={client.client_id} title={client.device_id}>
              <span className="status-dot online" aria-hidden="true" />
              <span>{client.peer_ip ?? shortSessionId(client.client_id)}</span>
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

function shortSessionId(sessionId: UUID): string {
  return sessionId.slice(0, 8);
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
    name: null,
    state: session.state,
    size: session.size,
  };
  return [next, ...current.filter((candidate) => candidate.session_id !== session.session_id)];
}
