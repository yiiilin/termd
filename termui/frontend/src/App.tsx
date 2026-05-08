import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Cable, Link, Plus, RefreshCcw, Unplug, UsersRound } from "lucide-react";
import { DirectClient, ProtocolClientError } from "./protocol/direct-client";
import { toSafeError } from "./protocol/errors";
import { parsePairingQrPayload } from "./protocol/pairing-payload";
import type {
  AttachRole,
  BrowserState,
  DaemonClientSummaryPayload,
  PairedServerState,
  SafeError,
  SessionCreatedPayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "./protocol/types";
import { decodeUtf8, sessionDataFromBase64 } from "./protocol/wire";
import { defaultServer, ensureDevice, loadBrowserState, recordPairing } from "./state/browser-state";
import { ConnectionPanel, ConnectionStatusPanel } from "./components/ConnectionPanel";
import { DaemonClientsPanel } from "./components/DaemonClientsPanel";
import { SessionList } from "./components/SessionList";
import { StatusBar } from "./components/StatusBar";
import { TerminalPane } from "./components/TerminalPane";

const FALLBACK_WS_URL = "ws://127.0.0.1:8765/ws";
const DEFAULT_SESSION_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

export default function App() {
  const [state, setState] = useState<BrowserState>({ pairedServers: [] });
  const [url, setUrl] = useState(() => defaultWsUrlFromPage());
  const [pairingToken, setPairingToken] = useState("");
  const [sessions, setSessions] = useState<SessionSummaryPayload[]>([]);
  const [daemonClients, setDaemonClients] = useState<DaemonClientSummaryPayload[]>([]);
  const [clientsOpen, setClientsOpen] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [role, setRole] = useState<AttachRole | undefined>();
  const [renamingSessionId, setRenamingSessionId] = useState<UUID | undefined>();
  const [renameDraft, setRenameDraft] = useState("");
  const [terminalChunks, setTerminalChunks] = useState<string[]>([]);
  const [status, setStatus] = useState("idle");
  const [error, setError] = useState<SafeError | undefined>();
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const receiveLoopActiveRef = useRef(false);
  const urlTouchedRef = useRef(false);
  const autoCheckedServerRef = useRef<UUID | undefined>(undefined);

  useEffect(() => {
    void loadBrowserState().then((loaded) => {
      setState(loaded);
      if (!urlTouchedRef.current) {
        setUrl(browserReachableWsUrl(loaded.defaultUrl ?? defaultServer(loaded)?.url ?? defaultWsUrlFromPage()));
      }
    });
  }, []);

  const activeServer = useMemo<PairedServerState | undefined>(() => defaultServer(state), [state]);
  const connectionStatusUrl = activeServer ? browserReachableWsUrl(activeServer.url) : url;
  const hasPairedServer = Boolean(activeServer && state.device);
  const showConnectionStatus = hasPairedServer && !error && status !== "pairing";
  const connectionReady =
    showConnectionStatus && status !== "idle" && status !== "connecting" && status !== "listing";
  const showPairingForm = !showConnectionStatus;

  const setSafeError = useCallback((caught: unknown) => {
    setError(toSafeError(caught));
    setStatus("error");
  }, []);

  const disconnectAttach = useCallback(() => {
    receiveLoopActiveRef.current = false;
    attachClientRef.current?.close();
    attachClientRef.current = undefined;
    attachedSessionRef.current = undefined;
    setRole(undefined);
  }, []);

  const handlePair = useCallback(async () => {
    setError(undefined);
    setStatus("pairing");
    try {
      const device = await ensureDevice();
      const payload = parsePairingQrPayload(pairingToken);
      const effectiveUrl = payload?.ws_url ?? url.trim();
      const token = payload?.token ?? pairingToken.trim();
      const client = await DirectClient.connect(effectiveUrl, device.device_id);
      if (payload && client.serverId !== payload.server_id) {
        client.close();
        throw new ProtocolClientError("pairing_payload_server_mismatch", "pairing payload does not match the connected daemon");
      }
      const accepted = await client.pair(token, device.device_public_key);
      client.close();
      const nextState = await recordPairing(accepted, effectiveUrl);
      setState(nextState);
      setPairingToken("");
      setSessions([]);
      setDaemonClients([]);
      setSelectedSessionId(undefined);
      setRenamingSessionId(undefined);
      setRenameDraft("");
      setTerminalChunks([]);
      disconnectAttach();
      if (payload) {
        setUrl(effectiveUrl);
      }
      setStatus("paired");
    } catch (caught) {
      setPairingToken("");
      setSafeError(caught);
    }
  }, [disconnectAttach, pairingToken, setSafeError, url]);

  const handleUrlChange = useCallback((nextUrl: string) => {
    urlTouchedRef.current = true;
    setUrl(nextUrl);
  }, []);

  const authenticatedClient = useCallback(async () => {
    const server = activeServer;
    const device = state.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    const reachableUrl = browserReachableWsUrl(server.url);
    const client = await DirectClient.connect(reachableUrl, device.device_id);
    await client.authenticate(device, { ...server, url: reachableUrl });
    return client;
  }, [activeServer, state.device]);

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
      setStatus("ready");
    } catch (caught) {
      setSafeError(caught);
    }
  }, [authenticatedClient, setSafeError]);

  const refreshDaemonClients = useCallback(
    async () => {
      try {
        const client = await authenticatedClient();
        try {
          const clientList = await client.listDaemonClients();
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
          }
          if (inner.type === "control_grant") {
            setRole("controller");
          }
        } catch (caught) {
          if (caught instanceof ProtocolClientError && caught.code === "controller_required") {
            setRole("viewer");
            continue;
          }
          if (receiveLoopActiveRef.current) {
            setSafeError(caught);
          }
          return;
        }
      }
    };
    void read();
  }, [setSafeError]);

  const handleSelectSession = useCallback(
    async (sessionId: UUID) => {
      if (attachedSessionRef.current === sessionId && role === "viewer") {
        setSelectedSessionId(sessionId);
        return;
      }
      setError(undefined);
      disconnectAttach();
      setTerminalChunks([]);
      setStatus("viewing");
      try {
        const client = await authenticatedClient();
        const attached = await client.attachSession(sessionId, "viewer");
        attachClientRef.current = client;
        attachedSessionRef.current = sessionId;
        setSelectedSessionId(sessionId);
        setRole(attached.role);
        setStatus("attached");
        void refreshDaemonClients();
        startReceiveLoop(client);
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, disconnectAttach, refreshDaemonClients, role, setSafeError, startReceiveLoop],
  );

  const handleAttach = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      setStatus("attaching");
      try {
        if (attachedSessionRef.current === sessionId && attachClientRef.current && role === "viewer") {
          await attachClientRef.current.sendControlRequest(sessionId);
          setStatus("attached");
          return;
        }
        disconnectAttach();
        setTerminalChunks([]);
        const client = await authenticatedClient();
        const attached = await client.attachSession(sessionId);
        attachClientRef.current = client;
        attachedSessionRef.current = sessionId;
        setSelectedSessionId(sessionId);
        setRole(attached.role);
        setStatus("attached");
        void refreshDaemonClients();
        startReceiveLoop(client);
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, disconnectAttach, refreshDaemonClients, role, setSafeError, startReceiveLoop],
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
      setRole(created.role);
      setSessions((current) => upsertSession(current, created));
      setStatus("attached");
      void refreshDaemonClients();
      startReceiveLoop(client);
    } catch (caught) {
      setSafeError(caught);
    }
  }, [authenticatedClient, disconnectAttach, refreshDaemonClients, setSafeError, startReceiveLoop]);

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
        }
        if (attachedSessionRef.current === sessionId) {
          disconnectAttach();
          setTerminalChunks([]);
        }
        void refreshDaemonClients();
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, disconnectAttach, refreshDaemonClients, selectedSessionId, setSafeError],
  );

  const handleTerminalInput = useCallback(
    async (data: string) => {
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId || role !== "controller") {
        return;
      }
      try {
        await client.sendSessionData(sessionId, new TextEncoder().encode(data));
      } catch (caught) {
        if (caught instanceof ProtocolClientError && caught.code === "controller_required") {
          setRole("viewer");
          return;
        }
        setSafeError(caught);
      }
    },
    [role, setSafeError],
  );

  const handleResize = useCallback(
    (size: { rows: number; cols: number; pixel_width: number; pixel_height: number }) => {
      const client = attachClientRef.current;
      const sessionId = attachedSessionRef.current;
      if (!client || !sessionId) {
        return;
      }
      void client.resizeSession(sessionId, size).catch(setSafeError);
    },
    [setSafeError],
  );

  const handleControl = useCallback(async () => {
    const client = attachClientRef.current;
    const sessionId = attachedSessionRef.current;
    if (!client || !sessionId) {
      return;
    }
    try {
      await client.sendControlRequest(sessionId);
    } catch (caught) {
      setSafeError(caught);
    }
  }, [setSafeError]);

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand-row">
          <div className="brand-title">
            <Cable size={18} aria-hidden="true" />
            <span>termd</span>
          </div>
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
        {showPairingForm ? (
          <ConnectionPanel
            url={url}
            token={pairingToken}
            status={status}
            onUrlChange={handleUrlChange}
            onTokenChange={setPairingToken}
            onPair={handlePair}
          />
        ) : null}
        {showConnectionStatus && activeServer ? (
          <ConnectionStatusPanel serverId={activeServer.server_id} url={connectionStatusUrl} status={status} />
        ) : null}
        {connectionReady ? (
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
              <button type="button" onClick={disconnectAttach} disabled={!role}>
                <Unplug size={16} aria-hidden="true" />
                Disconnect
              </button>
            </div>
            <SessionList
              sessions={sessions}
              selectedSessionId={selectedSessionId}
              attachedSessionId={attachedSessionRef.current}
              attachedRole={role}
              renamingSessionId={renamingSessionId}
              renameDraft={renameDraft}
              onSelect={handleSelectSession}
              onAttach={handleAttach}
              onStartRename={handleStartRename}
              onRenameDraftChange={setRenameDraft}
              onSaveRename={handleSaveRename}
              onCancelRename={handleCancelRename}
              onClose={handleCloseSession}
            />
          </>
        ) : null}
      </aside>
      <main className="workspace">
        <div className="toolbar">
          <div className="toolbar-group">
            <Link size={16} aria-hidden="true" />
            <span>{activeServer?.server_id ?? "unpaired"}</span>
          </div>
          {connectionReady && role === "viewer" ? (
            <button type="button" onClick={handleControl}>
              Take control
            </button>
          ) : null}
        </div>
        {connectionReady ? (
          <TerminalPane
            chunks={terminalChunks}
            role={role}
            attached={Boolean(attachedSessionRef.current)}
            onInput={handleTerminalInput}
            onResize={handleResize}
          />
        ) : (
          <div className="terminal-pane" aria-label="terminal unavailable">
            <div className="terminal-placeholder">disconnected</div>
          </div>
        )}
        <StatusBar status={status} error={error} sessionId={attachedSessionRef.current ?? selectedSessionId} />
      </main>
    </div>
  );
}

export function defaultWsUrlFromPage(location: Pick<Location, "protocol" | "host"> | undefined = globalThis.location): string {
  if (!location || !location.host || (location.protocol !== "http:" && location.protocol !== "https:")) {
    return FALLBACK_WS_URL;
  }
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

export function browserReachableWsUrl(
  rawUrl: string,
  page: Pick<Location, "protocol" | "host" | "hostname"> | undefined = globalThis.location,
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

function isLoopbackHost(hostname: string): boolean {
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "::1" || hostname === "[::1]";
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
