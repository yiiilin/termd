import { useCallback, useEffect, useRef, useState, type MutableRefObject } from "react";
import { DirectClient, ProtocolClientError } from "../protocol/direct-client";
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
  onBrokenAttachedClient: (client: DirectClient, caught: unknown) => boolean;
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

function createTransportAbortController(
  isTerminalTransportPaused: () => boolean,
): { controller: AbortController; dispose: () => void } | undefined {
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

function waitForConnectionRetryDelay(delayMs: number, signal?: AbortSignal): Promise<void> {
  return abortableConnectionStep(
    new Promise((resolve) => {
      globalThis.setTimeout(resolve, delayMs);
    }),
    signal,
  );
}

export function useWorkspaceConnection(options: UseWorkspaceConnectionOptions) {
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const pendingAttachClientRef = useRef<DirectClient | undefined>(undefined);
  const workspaceClientRef = useRef<DirectClient | undefined>(undefined);
  const workspaceClientPromiseRef = useRef<Promise<DirectClient> | undefined>(undefined);
  const workspaceClientAbortControllerRef = useRef<AbortController | undefined>(undefined);
  const workspaceClientGenerationRef = useRef(0);
  const sessionPermissionIdsRef = useRef<Set<UUID>>(new Set());
  const workspaceSessionPermissionIdsRef = useRef<Set<UUID>>(new Set());
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);

  const closeWorkspaceMetadataClient = useCallback(() => {
    workspaceClientGenerationRef.current += 1;
    workspaceClientAbortControllerRef.current?.abort();
    workspaceClientAbortControllerRef.current = undefined;
    workspaceClientPromiseRef.current = undefined;
    workspaceSessionPermissionIdsRef.current.clear();
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
    options.pendingTerminalAttachSessionRef.current = undefined;
    sessionPermissionIdsRef.current.clear();
  }, [
    options.pendingTerminalAttachSessionRef,
    options.receiveLoopActiveRef,
    options.receiveLoopGenerationRef,
  ]);

  const closeWorkspaceClient = useCallback(() => {
    closeWorkspaceMetadataClient();
    closeAttachClient();
  }, [closeAttachClient, closeWorkspaceMetadataClient]);

  const claimAttachClient = useCallback((client: DirectClient) => {
    // 中文注释：terminal attach/create/reconnect 一旦成功，这条 WebSocket 就成为当前
    // session 的唯一主连接。任何旧 metadata sidecar（包括尚未落地的 pending connect）
    // 都必须立刻作废，避免迟到 promise 再把孤儿连接写回 workspaceClientRef。
    closeWorkspaceMetadataClient();
    attachClientRef.current = client;
  }, [closeWorkspaceMetadataClient]);

  const authenticatedClient = useCallback(async (
    timeoutMs = options.requestTimeoutMs,
    signal?: AbortSignal,
    clientOptions: AuthenticatedClientOptions = {},
  ) => {
    const server = options.activeServer;
    const device = options.device;
    if (!server || !device) {
      throw new ProtocolClientError("missing_pairing", "device is not paired");
    }
    if (options.isTerminalTransportPaused()) {
      throw terminalTransportPausedError();
    }
    // 中文注释：document hidden 不会中断 terminal WebSocket；这里只有浏览器明确 offline
    // 才快速取消建连，避免后台恢复后必须整条 terminal stream 重建。
    const transportAbort = createTransportAbortController(options.isTerminalTransportPaused);
    const linkedAbort = createLinkedAbortController(signal, transportAbort?.controller.signal);
    const abortSignal = linkedAbort?.controller.signal;
    let client: DirectClient | undefined;
    const routeUrls = options.resolveServerRouteUrls(server);
    try {
      throwIfConnectionAborted(abortSignal);
      let lastConnectError: unknown;
      const connectTimeoutMs = Math.min(timeoutMs, options.socketConnectTimeoutMs);
      for (let attempt = 1; attempt <= options.socketConnectAttempts; attempt += 1) {
        for (const routeUrl of routeUrls) {
          try {
            client = await DirectClient.connect(routeUrl, server.server_id, device.device_id, {
              expectedDaemonPublicKey: server.daemon_public_key,
              trustedDevice: device,
              timeoutMs: connectTimeoutMs,
              socketOpenTimeoutMs: Math.min(connectTimeoutMs, options.socketOpenTimeoutMs),
              socketOpenHedgeDelayMs: options.socketOpenHedgeDelayMs,
              requestTimeoutMs: options.requestTimeoutMs,
              signal: abortSignal,
            });
            await abortableConnectionStep(
              client.authenticate(device, { ...server, url: routeUrl }, {
                clientKind: clientOptions.clientKind,
              }),
              abortSignal,
            );
            return client;
          } catch (caught) {
            lastConnectError = caught;
            client?.close();
            client = undefined;
            if (abortSignal?.aborted || options.isTerminalTransportPaused()) {
              throw caught;
            }
          }
        }
        if (attempt >= options.socketConnectAttempts || !options.isRetryableConnectionError(lastConnectError)) {
          throw lastConnectError ?? new ProtocolClientError("connection_error", "connection error");
        }
        await waitForConnectionRetryDelay(options.socketConnectRetryDelayMs, abortSignal);
      }
      throw lastConnectError ?? new ProtocolClientError("connection_error", "connection error");
    } catch (caught) {
      // 中文注释：E2EE/auth 失败时必须回收半开 socket，避免 relay 上堆积认证未完成 client。
      client?.close();
      throw caught;
    } finally {
      linkedAbort?.dispose();
      transportAbort?.dispose();
    }
  }, [
    options.activeServer,
    options.device,
    options.isRetryableConnectionError,
    options.isTerminalTransportPaused,
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
      sessionPermissionIdsRef.current.clear();
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
      workspaceSessionPermissionIdsRef.current.clear();
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
    async (sessionId: UUID) => {
      const client = await authenticatedWorkspaceClient();
      const permissionIds =
        client === attachClientRef.current
          ? sessionPermissionIdsRef.current
          : workspaceSessionPermissionIdsRef.current;
      if (!permissionIds.has(sessionId)) {
        await client.attachSessionPermission(sessionId);
        permissionIds.add(sessionId);
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
      const client = await authenticatedClient(options.requestTimeoutMs);
      try {
        // 中文注释：文件上传/下载不应排在 terminal snapshot 后面；这里显式只拿 session 权限，
        // 不订阅 stdout。
        await client.attachSessionPermission(sessionId);
        return { client, ownsClient: true };
      } catch (caught) {
        client.close();
        throw caught;
      }
    },
    [authenticatedClient, options.requestTimeoutMs],
  );

  return {
    attachClientRef,
    pendingAttachClientRef,
    workspaceClientPromiseRef,
    workspaceClientAbortControllerRef,
    workspaceClientGenerationRef,
    workspaceClientRef,
    sessionPermissionIdsRef,
    workspaceSessionPermissionIdsRef,
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
    resolveSessionScopedClient,
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
