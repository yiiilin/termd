import { useCallback, useEffect, useRef, useState, type MutableRefObject } from "react";
import { ProtocolClientError } from "../protocol/errors";
import { V070Client } from "../protocol/v070-client";
import { migrateDeviceCertificate } from "../protocol/pairing-client";
import type { DeviceState, PairedServerState, UUID } from "../protocol/types";

export interface WorkspaceAutoRetryStatus {
  phase: "idle" | "scheduled" | "exhausted";
  retryKey?: string;
  attempts: number;
}

interface UseWorkspaceConnectionOptions {
  activeServer?: PairedServerState;
  device?: DeviceState;
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  pendingTerminalAttachSessionRef: MutableRefObject<UUID | undefined>;
  receiveLoopActiveRef: MutableRefObject<boolean>;
  receiveLoopGenerationRef: MutableRefObject<number>;
  isTerminalTransportPaused: () => boolean;
  isRetryableConnectionError: (caught: unknown) => boolean;
  resolveServerRouteUrls: (server: PairedServerState) => string[];
  onBrokenAttachedClient: (client: V070Client, caught: unknown) => boolean;
  onDeviceCertificateMigrated?: (serverId: UUID, deviceCertificate: string) => Promise<void> | void;
  requestTimeoutMs: number;
  defaultWorkspaceTimeoutMs: number;
  socketConnectTimeoutMs: number;
  socketOpenTimeoutMs: number;
  socketOpenHedgeDelayMs: number;
  socketConnectAttempts: number;
  socketConnectRetryDelayMs: number;
}

interface AuthenticatedClientOptions {
  clientKind?: "interactive" | "metadata";
}

function terminalTransportPausedError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection paused while browser is offline");
}

function connectionAbortedError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection closed");
}

function throwIfConnectionAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw connectionAbortedError();
  }
}

export function useWorkspaceConnection(options: UseWorkspaceConnectionOptions) {
  const attachClientRef = useRef<V070Client | undefined>(undefined);
  const pendingAttachClientRef = useRef<V070Client | undefined>(undefined);
  const workspaceClientRef = useRef<V070Client | undefined>(undefined);
  const workspaceClientPromiseRef = useRef<Promise<V070Client> | undefined>(undefined);
  const workspaceClientAbortControllerRef = useRef<AbortController | undefined>(undefined);
  const workspaceClientGenerationRef = useRef(0);
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);

  const closeWorkspaceMetadataClient = useCallback(() => {
    workspaceClientGenerationRef.current += 1;
    workspaceClientAbortControllerRef.current?.abort();
    workspaceClientAbortControllerRef.current = undefined;
    workspaceClientPromiseRef.current = undefined;
    const workspaceClient = workspaceClientRef.current;
    workspaceClientRef.current = undefined;
    if (workspaceClient) {
      workspaceClient.interruptReceiveWaiters();
      workspaceClient.close();
    }
  }, []);

  const closeAttachClient = useCallback(() => {
    options.receiveLoopActiveRef.current = false;
    options.receiveLoopGenerationRef.current += 1;
    const clients = new Set<V070Client>();
    if (pendingAttachClientRef.current) {
      clients.add(pendingAttachClientRef.current);
    }
    if (attachClientRef.current) {
      clients.add(attachClientRef.current);
    }
    for (const client of clients) {
      // session 切换会复用同一 V070Client；先中断旧 loop 的 pending receive，
      // 避免它抢走下一条 terminal socket 的 attach_sync 后再按 stale generation 丢弃。
      client.interruptReceiveWaiters();
      const sessionId = options.attachedSessionRef.current
        ?? options.pendingTerminalAttachSessionRef.current;
      if (sessionId) client.detachSession(sessionId);
      if (!workspaceClientRef.current && !client.isClosed) workspaceClientRef.current = client;
    }
    pendingAttachClientRef.current = undefined;
    attachClientRef.current = undefined;
    options.pendingTerminalAttachSessionRef.current = undefined;
  }, [
    options.attachedSessionRef,
    options.pendingTerminalAttachSessionRef,
    options.receiveLoopActiveRef,
    options.receiveLoopGenerationRef,
  ]);

  const closeWorkspaceClient = useCallback(() => {
    const clients = new Set([
      workspaceClientRef.current,
      pendingAttachClientRef.current,
      attachClientRef.current,
    ].filter((client): client is V070Client => Boolean(client)));
    closeWorkspaceMetadataClient();
    options.receiveLoopActiveRef.current = false;
    options.receiveLoopGenerationRef.current += 1;
    pendingAttachClientRef.current = undefined;
    attachClientRef.current = undefined;
    options.pendingTerminalAttachSessionRef.current = undefined;
    for (const client of clients) client.close();
  }, [
    closeWorkspaceMetadataClient,
    options.pendingTerminalAttachSessionRef,
    options.receiveLoopActiveRef,
    options.receiveLoopGenerationRef,
  ]);

  const claimAttachClient = useCallback((client: V070Client) => {
    if (workspaceClientPromiseRef.current && workspaceClientRef.current !== client) {
      workspaceClientGenerationRef.current += 1;
      workspaceClientAbortControllerRef.current?.abort();
      workspaceClientAbortControllerRef.current = undefined;
      workspaceClientPromiseRef.current = undefined;
    }
    workspaceClientRef.current = client;
    attachClientRef.current = client;
  }, []);

  const authenticatedClient = useCallback(async (
    _timeoutMs = options.requestTimeoutMs,
    signal?: AbortSignal,
    _clientOptions: AuthenticatedClientOptions = {},
  ) => {
    const server = options.activeServer;
    const device = options.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    if (options.isTerminalTransportPaused()) {
      throw terminalTransportPausedError();
    }
    throwIfConnectionAborted(signal);
    let effectiveServer = server;
    if (!server.device_certificate) {
      const deviceCertificate = await migrateDeviceCertificate(server, device);
      effectiveServer = { ...server, device_certificate: deviceCertificate };
      await options.onDeviceCertificateMigrated?.(server.server_id, deviceCertificate);
    }
    throwIfConnectionAborted(signal);
    return V070Client.connect(effectiveServer, device) as unknown as V070Client;
  }, [
    options.activeServer,
    options.device,
    options.isRetryableConnectionError,
    options.isTerminalTransportPaused,
    options.onDeviceCertificateMigrated,
    options.requestTimeoutMs,
    options.resolveServerRouteUrls,
    options.socketConnectAttempts,
    options.socketConnectRetryDelayMs,
    options.socketConnectTimeoutMs,
    options.socketOpenHedgeDelayMs,
    options.socketOpenTimeoutMs,
  ]);

  const authenticatedWorkspaceClient = useCallback(async (timeoutMs = options.defaultWorkspaceTimeoutMs) => {
    const attachedClient = attachClientRef.current;
    if (attachedClient && !attachedClient.isClosed) {
      // 中文注释：一旦当前 session 已 attach，普通 metadata RPC 必须回到同一条
      // terminal WebSocket 上。这样 status/files/git 的超时才只会表现为 segment
      // 级失败，而不会在后台悄悄多留一条独立 metadata 连接。
      if (
        workspaceClientPromiseRef.current ||
        (workspaceClientRef.current && workspaceClientRef.current !== attachedClient)
      ) {
        closeWorkspaceMetadataClient();
      }
      workspaceClientRef.current = attachedClient;
      return attachedClient;
    }
    if (attachedClient?.isClosed) {
      // 中文注释：attach transport 已断开但 attached session 事实仍在时，旁路 metadata
      // 不能抢先新建另一条认证连接；必须让终端重连状态机先接管当前 session。
      closeWorkspaceMetadataClient();
      if (options.attachedSessionRef.current) {
        const error = new ProtocolClientError("connection_closed", "terminal connection closed");
        if (options.onBrokenAttachedClient(attachedClient, error)) {
          throw error;
        }
      }
      attachClientRef.current = undefined;
    }
    if (options.attachedSessionRef.current) {
      throw new ProtocolClientError("connection_closed", "terminal connection is reconnecting");
    }
    const existing = workspaceClientRef.current;
    if (existing && !existing.isClosed) {
      return existing;
    }
    if (existing?.isClosed) {
      workspaceClientRef.current = undefined;
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
    let promise: Promise<V070Client>;
    promise = authenticatedClient(timeoutMs, abortController.signal)
      .then((client) => {
        clearAbortController();
        if (workspaceClientGenerationRef.current !== requestGeneration) {
          // 中文注释：daemon 切换、session 切换或 workspace reset 发生后，迟到 client
          // 只能立刻关闭，不能再写回当前工作台连接引用。
          client.close();
          throw new ProtocolClientError("stale_connection", "session connection was superseded");
        }
        workspaceClientRef.current = client;
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
  }, [
    attachClientRef,
    authenticatedClient,
    closeWorkspaceMetadataClient,
    options.attachedSessionRef,
    options.defaultWorkspaceTimeoutMs,
    options.onBrokenAttachedClient,
  ]);

  const authenticatedSessionClient = useCallback(
    async (_sessionId: UUID) => authenticatedWorkspaceClient(),
    [authenticatedWorkspaceClient],
  );

  const resolveSessionClient = useCallback(
    async (sessionId: UUID): Promise<{ client: V070Client; ownsClient: boolean }> => {
      return { client: await authenticatedSessionClient(sessionId), ownsClient: false };
    },
    [authenticatedSessionClient],
  );

  const openSessionOperationClient = useCallback(
    async (sessionId: UUID): Promise<{ client: V070Client; ownsClient: boolean }> => {
      return { client: await authenticatedSessionClient(sessionId), ownsClient: false };
    },
    [authenticatedSessionClient],
  );

  return {
    attachClientRef,
    pendingAttachClientRef,
    workspaceClientPromiseRef,
    workspaceClientAbortControllerRef,
    workspaceClientGenerationRef,
    workspaceClientRef,
    claimAttachClient,
    closeAttachClient,
    closeWorkspaceMetadataClient,
    connectionAutoRetryTimerRef,
    connectionAutoRetryKeyRef,
    connectionAutoRetryAttemptsRef,
    closeWorkspaceClient,
    authenticatedClient,
    authenticatedWorkspaceClient,
    authenticatedSessionClient,
    resolveSessionClient,
    openSessionOperationClient,
  };
}

interface UseWorkspaceAutoRetryOptions {
  error: unknown;
  status: string;
  activeSurface: "admin" | "workspace";
  hasPairedServer: boolean;
  activeServerId?: UUID;
  attachedSessionId?: UUID;
  selectedSessionId?: UUID;
  currentAttachedSessionRef: MutableRefObject<UUID | undefined>;
  retryDelayMs: number;
  retryLimit: number;
  onRetryConnection: () => void | Promise<void>;
}

export function useWorkspaceAutoRetry(
  connection: ReturnType<typeof useWorkspaceConnection>,
  options: UseWorkspaceAutoRetryOptions,
): WorkspaceAutoRetryStatus {
  const {
    connectionAutoRetryTimerRef,
    connectionAutoRetryKeyRef,
    connectionAutoRetryAttemptsRef,
  } = connection;
  const onRetryConnectionRef = useRef(options.onRetryConnection);
  const [retryStatus, setRetryStatus] = useState<WorkspaceAutoRetryStatus>({
    phase: "idle",
    attempts: 0,
  });

  useEffect(() => {
    onRetryConnectionRef.current = options.onRetryConnection;
  }, [options.onRetryConnection]);

  useEffect(() => {
    if (!options.error && (options.status === "ready" || options.status === "attached")) {
      connectionAutoRetryKeyRef.current = undefined;
      connectionAutoRetryAttemptsRef.current = 0;
      setRetryStatus({ phase: "idle", attempts: 0 });
    }
  }, [connectionAutoRetryAttemptsRef, connectionAutoRetryKeyRef, options.error, options.status]);

  useEffect(() => {
    if (connectionAutoRetryTimerRef.current !== undefined) {
      window.clearTimeout(connectionAutoRetryTimerRef.current);
      connectionAutoRetryTimerRef.current = undefined;
    }

    if (!options.error || !options.hasPairedServer || options.activeSurface !== "workspace") {
      setRetryStatus((current) => current.phase === "idle" ? current : { phase: "idle", attempts: 0 });
      return undefined;
    }

    const retryKey = [
      options.activeServerId ?? "unknown",
      options.currentAttachedSessionRef.current ?? options.attachedSessionId ?? options.selectedSessionId ?? "no-session",
    ].join(":");
    if (connectionAutoRetryKeyRef.current !== retryKey) {
      connectionAutoRetryKeyRef.current = retryKey;
      connectionAutoRetryAttemptsRef.current = 0;
    }
    if (connectionAutoRetryAttemptsRef.current >= options.retryLimit) {
      setRetryStatus({
        phase: "exhausted",
        retryKey,
        attempts: connectionAutoRetryAttemptsRef.current,
      });
      return undefined;
    }

    setRetryStatus({
      phase: "scheduled",
      retryKey,
      attempts: connectionAutoRetryAttemptsRef.current,
    });
    connectionAutoRetryTimerRef.current = window.setTimeout(() => {
      connectionAutoRetryTimerRef.current = undefined;
      connectionAutoRetryAttemptsRef.current += 1;
      setRetryStatus({
        phase: "scheduled",
        retryKey,
        attempts: connectionAutoRetryAttemptsRef.current,
      });
      // 错误态自动恢复只复用手动 Refresh 的路径：有当前 session 就重新 attach，
      // 否则重新刷新 daemon 列表；失败后由新的 error 继续驱动剩余重试次数。
      void onRetryConnectionRef.current();
    }, options.retryDelayMs);

    return () => {
      if (connectionAutoRetryTimerRef.current !== undefined) {
        window.clearTimeout(connectionAutoRetryTimerRef.current);
        connectionAutoRetryTimerRef.current = undefined;
      }
    };
  }, [
    options.activeServerId,
    options.activeSurface,
    options.attachedSessionId,
    options.currentAttachedSessionRef,
    options.error,
    options.hasPairedServer,
    options.retryDelayMs,
    options.retryLimit,
    options.selectedSessionId,
    connectionAutoRetryAttemptsRef,
    connectionAutoRetryKeyRef,
    connectionAutoRetryTimerRef,
  ]);

  return retryStatus;
}
