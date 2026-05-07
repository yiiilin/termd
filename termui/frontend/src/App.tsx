import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Cable, Link, RefreshCcw, ShieldCheck, Unplug } from "lucide-react";
import { DirectClient, ProtocolClientError } from "./protocol/direct-client";
import { toSafeError } from "./protocol/errors";
import { parsePairingQrPayload } from "./protocol/pairing-payload";
import type {
  AttachRole,
  BrowserState,
  PairedServerState,
  SafeError,
  SessionSummaryPayload,
  UUID,
} from "./protocol/types";
import { decodeUtf8, sessionDataFromBase64 } from "./protocol/wire";
import { defaultServer, ensureDevice, loadBrowserState, recordPairing } from "./state/browser-state";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { SessionList } from "./components/SessionList";
import { StatusBar } from "./components/StatusBar";
import { TerminalPane } from "./components/TerminalPane";

const DEFAULT_URL = "ws://127.0.0.1:8765/ws";

export default function App() {
  const [state, setState] = useState<BrowserState>({ pairedServers: [] });
  const [url, setUrl] = useState(DEFAULT_URL);
  const [pairingToken, setPairingToken] = useState("");
  const [sessions, setSessions] = useState<SessionSummaryPayload[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<UUID | undefined>();
  const [role, setRole] = useState<AttachRole | undefined>();
  const [terminalChunks, setTerminalChunks] = useState<string[]>([]);
  const [status, setStatus] = useState("idle");
  const [error, setError] = useState<SafeError | undefined>();
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const receiveLoopActiveRef = useRef(false);
  const urlTouchedRef = useRef(false);

  useEffect(() => {
    void loadBrowserState().then((loaded) => {
      setState(loaded);
      if (!urlTouchedRef.current) {
        setUrl(loaded.defaultUrl ?? defaultServer(loaded)?.url ?? DEFAULT_URL);
      }
    });
  }, []);

  const activeServer = useMemo<PairedServerState | undefined>(() => defaultServer(state), [state]);

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
      if (payload) {
        setUrl(effectiveUrl);
      }
      setStatus("paired");
    } catch (caught) {
      setPairingToken("");
      setSafeError(caught);
    }
  }, [pairingToken, setSafeError, url]);

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
    const client = await DirectClient.connect(server.url, device.device_id);
    await client.authenticate(device, server);
    return client;
  }, [activeServer, state.device]);

  const handleRefresh = useCallback(async () => {
    setError(undefined);
    setStatus("listing");
    try {
      const client = await authenticatedClient();
      const list = await client.listSessions();
      client.close();
      setSessions(list.sessions);
      setSelectedSessionId(list.sessions.at(0)?.session_id);
      setStatus("ready");
    } catch (caught) {
      setSafeError(caught);
    }
  }, [authenticatedClient, setSafeError]);

  const handleAttach = useCallback(
    async (sessionId: UUID) => {
      setError(undefined);
      disconnectAttach();
      setTerminalChunks([]);
      setStatus("attaching");
      try {
        const client = await authenticatedClient();
        const attached = await client.attachSession(sessionId);
        attachClientRef.current = client;
        attachedSessionRef.current = sessionId;
        setSelectedSessionId(sessionId);
        setRole(attached.role);
        setStatus("attached");
        startReceiveLoop(client);
      } catch (caught) {
        setSafeError(caught);
      }
    },
    [authenticatedClient, disconnectAttach, setSafeError],
  );

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

  useEffect(() => {
    if (!attachClientRef.current || !attachedSessionRef.current) {
      return undefined;
    }
    // 当前 daemon 的输出 flush 依赖入站帧触发；attach 后发送加密 ping，避免空闲终端无输出。
    const timer = window.setInterval(() => {
      void attachClientRef.current?.sendPing().catch(setSafeError);
    }, 200);
    return () => window.clearInterval(timer);
  }, [role, setSafeError]);

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
          <Cable size={18} aria-hidden="true" />
          <span>termd</span>
        </div>
        <ConnectionPanel
          url={url}
          token={pairingToken}
          status={status}
          onUrlChange={handleUrlChange}
          onTokenChange={setPairingToken}
          onPair={handlePair}
        />
        <div className="panel-actions">
          <button type="button" onClick={handleRefresh} disabled={!activeServer || status === "listing"}>
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
          onSelect={setSelectedSessionId}
          onAttach={handleAttach}
        />
      </aside>
      <main className="workspace">
        <div className="toolbar">
          <div className="toolbar-group">
            <Link size={16} aria-hidden="true" />
            <span>{activeServer?.server_id ?? "unpaired"}</span>
          </div>
          <div className="toolbar-group">
            <ShieldCheck size={16} aria-hidden="true" />
            <span>{role ?? "detached"}</span>
          </div>
          <button type="button" onClick={handleControl} disabled={!attachedSessionRef.current}>
            Steal control
          </button>
        </div>
        <TerminalPane
          chunks={terminalChunks}
          role={role}
          attached={Boolean(attachedSessionRef.current)}
          onInput={handleTerminalInput}
          onResize={handleResize}
        />
        <StatusBar status={status} error={error} sessionId={attachedSessionRef.current ?? selectedSessionId} />
      </main>
    </div>
  );
}
