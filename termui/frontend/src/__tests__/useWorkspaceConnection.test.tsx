import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { V070Client } from "../protocol/v070-client";
import { generateDeviceIdentity } from "../protocol/auth";
import { decodeSupervisorTerminalServerFrame, encodeSupervisorTerminalServerFrame } from "../protocol/supervisor-terminal";
import { useWorkspaceConnection } from "../hooks/useWorkspaceConnection";
import type { DeviceState, PairedServerState, UUID } from "../protocol/types";

const SERVER_ID = "00000000-0000-0000-0000-000000000101";
const DEVICE_ID = "00000000-0000-0000-0000-000000000201";
const SESSION_ID = "00000000-0000-0000-0000-000000000301";
const NEXT_SESSION_ID = "00000000-0000-0000-0000-000000000302";

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T | PromiseLike<T>) => void;
  reject: (reason?: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((innerResolve, innerReject) => {
    resolve = innerResolve;
    reject = innerReject;
  });
  return { promise, resolve, reject };
}

function makeServer(): PairedServerState {
  return {
    server_id: SERVER_ID,
    daemon_public_key: "daemon-public-key",
    url: "ws://127.0.0.1:8765/ws",
    paired_at_ms: Date.now(),
    device_certificate: "device.certificate.signature",
  };
}

function makeDevice(): DeviceState {
  return {
    device_id: DEVICE_ID,
    device_public_key: "device-public-key",
    device_signing_key_secret: "device-secret",
    name: "Web client",
  };
}

function makeClient(overrides: Partial<Record<string, unknown>> = {}): V070Client {
  const client = {
    isClosed: false,
    authenticate: vi.fn(async () => undefined),
    detachSession: vi.fn(),
    close: vi.fn(() => {
      client.isClosed = true;
    }),
    interruptReceiveWaiters: vi.fn(),
    ...overrides,
  };
  return client as unknown as V070Client;
}

function renderWorkspaceConnection(server = makeServer()) {
  const attachedSessionRef = { current: undefined as UUID | undefined };
  const pendingTerminalAttachSessionRef = { current: undefined as UUID | undefined };
  const receiveLoopActiveRef = { current: false };
  const receiveLoopGenerationRef = { current: 0 };
  const result = renderHook(() =>
    useWorkspaceConnection({
      activeServer: server,
      device: makeDevice(),
      attachedSessionRef,
      pendingTerminalAttachSessionRef,
      receiveLoopActiveRef,
      receiveLoopGenerationRef,
      isTerminalTransportPaused: () => false,
      isRetryableConnectionError: () => true,
      resolveServerRouteUrls: (server) => [server.url],
      onBrokenAttachedClient: () => false,
      requestTimeoutMs: 5000,
      defaultWorkspaceTimeoutMs: 15000,
      socketConnectTimeoutMs: 3000,
      socketOpenTimeoutMs: 1200,
      socketOpenHedgeDelayMs: 300,
      socketConnectAttempts: 4,
      socketConnectRetryDelayMs: 80,
    }),
  );
  return {
    attachedSessionRef,
    pendingTerminalAttachSessionRef,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    ...result,
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("useWorkspaceConnection", () => {
  it("v0.7 attach 保留 metadata client，detach 只关闭 terminal transport", async () => {
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const server = { ...makeServer(), device_certificate: "device.certificate.signature" };
    const device = await generateDeviceIdentity(DEVICE_ID);
    const client = new V070Client(server, device, transport);
    const attach = client.attachSession(SESSION_ID);
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: { session_id: SESSION_ID },
    }));
    await attach;
    const { attachedSessionRef, result } = renderWorkspaceConnection(server);
    result.current.workspaceClientRef.current = client as unknown as V070Client;

    result.current.claimAttachClient(client as unknown as V070Client);

    expect(result.current.workspaceClientRef.current).toBe(client);
    expect(result.current.attachClientRef.current).toBe(client);
    attachedSessionRef.current = SESSION_ID;
    result.current.closeAttachClient();
    expect(transport.closeTerminal).toHaveBeenCalledTimes(1);
    expect(transport.close).not.toHaveBeenCalled();
    expect(result.current.workspaceClientRef.current).toBe(client);
  });

  it("切换 session 时中断旧 receive waiter，让复用 client 的新 loop 消费 attach_sync", async () => {
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const server = { ...makeServer(), device_certificate: "device.certificate.signature" };
    const client = new V070Client(server, await generateDeviceIdentity(DEVICE_ID), transport);
    const {
      attachedSessionRef,
      pendingTerminalAttachSessionRef,
      result,
    } = renderWorkspaceConnection(server);
    result.current.workspaceClientRef.current = client;

    const firstAttach = client.attachSession(SESSION_ID);
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: { session_id: SESSION_ID },
    }));
    await firstAttach;
    result.current.claimAttachClient(client);
    attachedSessionRef.current = SESSION_ID;
    const staleReceive = client.receiveInner().then(
      (envelope) => ({ outcome: "resolved" as const, envelope }),
      (error) => ({ outcome: "rejected" as const, error }),
    );

    result.current.closeAttachClient();
    attachedSessionRef.current = undefined;

    const reusedClient = await result.current.authenticatedWorkspaceClient();
    expect(reusedClient).toBe(client);
    const secondAttach = reusedClient.attachSession(NEXT_SESSION_ID);
    pendingTerminalAttachSessionRef.current = NEXT_SESSION_ID;
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: { session_id: NEXT_SESSION_ID },
    }));
    await secondAttach;
    result.current.claimAttachClient(reusedClient);
    attachedSessionRef.current = NEXT_SESSION_ID;
    pendingTerminalAttachSessionRef.current = undefined;
    const nextReceive = reusedClient.receiveInner();
    transport.onTerminal?.(encodeSupervisorTerminalServerFrame({
      type: "attach_sync",
      session_id: NEXT_SESSION_ID,
      base_seq: 0,
      snapshot: {
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        process_id: 7,
        retained_output_bytes: new TextEncoder().encode("session-b-ready\n"),
      },
      frames: [],
    }));

    await expect(staleReceive).resolves.toMatchObject({
      outcome: "rejected",
      error: { code: "connection_closed" },
    });
    const nextEnvelope = await nextReceive;
    const nextFrame = decodeSupervisorTerminalServerFrame((nextEnvelope.payload as any).data_bytes);
    expect(nextFrame).toMatchObject({
      type: "attach_sync",
      session_id: NEXT_SESSION_ID,
    });
    expect(transport.closeTerminal).toHaveBeenCalledTimes(1);
    expect(transport.close).not.toHaveBeenCalled();
  });

  it("device certificate 存在时使用 v0.7 workspace transport", async () => {
    const server = { ...makeServer(), device_certificate: "device.certificate.signature" };
    const client = makeClient();
    const v070Connect = vi.spyOn(V070Client, "connect").mockResolvedValue(client as unknown as V070Client);
    const { result } = renderWorkspaceConnection(server);

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(client);

    expect(v070Connect).toHaveBeenCalledWith(server, expect.objectContaining({ device_id: DEVICE_ID }));
  });

  it("旧 BrowserState 先迁移并持久化 device certificate 再创建 v0.7 client", async () => {
    const server = { ...makeServer(), device_certificate: undefined };
    const device = await generateDeviceIdentity(DEVICE_ID);
    const client = makeClient();
    const migrated = vi.fn(async () => undefined);
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ challenge: "migration-challenge" }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ device_certificate: "migrated.certificate" }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const connect = vi.spyOn(V070Client, "connect").mockResolvedValue(client);
    const attachedSessionRef = { current: undefined as UUID | undefined };
    const pendingTerminalAttachSessionRef = { current: undefined as UUID | undefined };
    const { result } = renderHook(() => useWorkspaceConnection({
      activeServer: server,
      device,
      attachedSessionRef,
      pendingTerminalAttachSessionRef,
      receiveLoopActiveRef: { current: false },
      receiveLoopGenerationRef: { current: 0 },
      isTerminalTransportPaused: () => false,
      isRetryableConnectionError: () => true,
      resolveServerRouteUrls: (candidate) => [candidate.url],
      onBrokenAttachedClient: () => false,
      onDeviceCertificateMigrated: migrated,
      requestTimeoutMs: 5000,
      defaultWorkspaceTimeoutMs: 15000,
      socketConnectTimeoutMs: 3000,
      socketOpenTimeoutMs: 1200,
      socketOpenHedgeDelayMs: 300,
      socketConnectAttempts: 4,
      socketConnectRetryDelayMs: 80,
    }));

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(client);

    expect(migrated).toHaveBeenCalledWith(SERVER_ID, "migrated.certificate");
    expect(connect).toHaveBeenCalledWith(
      expect.objectContaining({ device_certificate: "migrated.certificate" }),
      expect.objectContaining({ device_id: DEVICE_ID }),
    );
  });

  it("v0.7 文件操作复用唯一 workspace client 和 metadata socket", async () => {
    const server = { ...makeServer(), device_certificate: "device.certificate.signature" };
    let metadataSocketCount = 0;
    let terminalSocketCount = 0;
    const clients: V070Client[] = [];
    vi.spyOn(V070Client, "connect").mockImplementation(async (_server, device) => {
      let metadataOpen = false;
      const transport = {
        onMetadata: undefined as ((data: unknown) => void) | undefined,
        onTerminal: undefined as ((data: unknown) => void) | undefined,
        connectMetadata: vi.fn(async () => {
          if (!metadataOpen) {
            metadataOpen = true;
            metadataSocketCount += 1;
            queueMicrotask(() => transport.onMetadata?.(JSON.stringify({
              type: "metadata.snapshot",
              payload: {
                revision: 1,
                state: { sessions: [{ session_id: SESSION_ID, state: "running" }] },
              },
            })));
          }
        }),
        reconnectMetadata: vi.fn(async () => undefined),
        openTerminal: vi.fn(async () => {
          terminalSocketCount += 1;
          queueMicrotask(() => transport.onTerminal?.(JSON.stringify({
            type: "terminal.attached",
            payload: { session_id: SESSION_ID },
          })));
        }),
        closeTerminal: vi.fn(),
        close: vi.fn(),
        sendTerminal: vi.fn(),
      };
      const client = new V070Client(server, device, transport);
      clients.push(client);
      return client;
    });
    const { attachedSessionRef, result } = renderWorkspaceConnection(server);
    const workspaceClient = await result.current.authenticatedWorkspaceClient();
    await (workspaceClient as unknown as V070Client).subscribeMetadata();
    await workspaceClient.attachSession(SESSION_ID);
    result.current.claimAttachClient(workspaceClient);
    attachedSessionRef.current = SESSION_ID;

    const operation = await result.current.openSessionOperationClient(SESSION_ID);

    expect(operation).toEqual({ client: workspaceClient, ownsClient: false });
    expect(clients).toHaveLength(1);
    expect(metadataSocketCount).toBe(1);
    expect(terminalSocketCount).toBe(1);
  });

  it("并发获取 workspace client 只建立一条连接", async () => {
    const client = makeClient();
    const connectSpy = vi.spyOn(V070Client, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    const first = result.current.authenticatedWorkspaceClient();
    const second = result.current.authenticatedWorkspaceClient();

    await expect(first).resolves.toBe(client);
    await expect(second).resolves.toBe(client);
    expect(connectSpy).toHaveBeenCalledTimes(1);
  });

  it("generation 已推进时会关闭迟到 client 并返回 stale_connection", async () => {
    const authGate = deferred<void>();
    const client = makeClient({
      authenticate: vi.fn(() => authGate.promise),
    });
    vi.spyOn(V070Client, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    const pending = result.current.authenticatedWorkspaceClient();
    result.current.workspaceClientGenerationRef.current += 1;
    authGate.resolve();

    await expect(pending).rejects.toMatchObject({
      code: "stale_connection",
    });
    expect(client.close).toHaveBeenCalledTimes(1);
  });

  it("closeWorkspaceClient 会中断 active/pending client 并清空连接状态", () => {
    const activeClient = makeClient();
    const pendingClient = makeClient();
    const {
      pendingTerminalAttachSessionRef,
      receiveLoopActiveRef,
      receiveLoopGenerationRef,
      result,
    } = renderWorkspaceConnection();

    result.current.attachClientRef.current = activeClient;
    result.current.pendingAttachClientRef.current = pendingClient;
    pendingTerminalAttachSessionRef.current = SESSION_ID;
    receiveLoopActiveRef.current = true;
    receiveLoopGenerationRef.current = 7;

    result.current.closeWorkspaceClient();

    expect(activeClient.interruptReceiveWaiters).not.toHaveBeenCalled();
    expect(activeClient.close).toHaveBeenCalledTimes(1);
    expect(pendingClient.interruptReceiveWaiters).not.toHaveBeenCalled();
    expect(pendingClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.attachClientRef.current).toBeUndefined();
    expect(result.current.pendingAttachClientRef.current).toBeUndefined();
    expect(pendingTerminalAttachSessionRef.current).toBeUndefined();
    expect(receiveLoopActiveRef.current).toBe(false);
    expect(receiveLoopGenerationRef.current).toBe(8);
  });

  it("会话控制直接复用 Access Token client，不发送 permission attach", async () => {
    const client = makeClient();
    vi.spyOn(V070Client, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedSessionClient(SESSION_ID)).resolves.toBe(client);
    await expect(result.current.authenticatedSessionClient(SESSION_ID)).resolves.toBe(client);

    expect(client).not.toHaveProperty("attachSessionPermission");
  });

  it("已 attach 后 workspace metadata 会回到当前 terminal client 并关闭旧 metadata 连接", async () => {
    const metadataClient = makeClient();
    const attachClient = makeClient();
    const connectSpy = vi.spyOn(V070Client, "connect").mockResolvedValue(metadataClient);
    const { attachedSessionRef, result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(metadataClient);
    result.current.workspaceClientRef.current = metadataClient;
    result.current.attachClientRef.current = attachClient;
    attachedSessionRef.current = SESSION_ID;

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(attachClient);

    expect(metadataClient.interruptReceiveWaiters).toHaveBeenCalledTimes(1);
    expect(metadataClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.workspaceClientRef.current).toBe(attachClient);
    expect(connectSpy).toHaveBeenCalledTimes(1);
  });

  it("将同一条 workspace client 提升为 terminal attach 时不关闭 transport", async () => {
    const workspaceClient = makeClient();
    vi.spyOn(V070Client, "connect").mockResolvedValue(workspaceClient);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(workspaceClient);
    result.current.claimAttachClient(workspaceClient);

    expect(workspaceClient.interruptReceiveWaiters).not.toHaveBeenCalled();
    expect(workspaceClient.close).not.toHaveBeenCalled();
    expect(result.current.workspaceClientRef.current).toBe(workspaceClient);
    expect(result.current.attachClientRef.current).toBe(workspaceClient);
  });

  it("metadata 建连未完成时若 terminal 先 attach，迟到 metadata client 会被作废", async () => {
    const connectGate = deferred<V070Client>();
    const metadataClient = makeClient();
    const attachClient = makeClient();
    vi.spyOn(V070Client, "connect").mockReturnValue(connectGate.promise);
    const { attachedSessionRef, result } = renderWorkspaceConnection();

    const pendingMetadata = result.current.authenticatedWorkspaceClient();
    result.current.claimAttachClient(attachClient);
    attachedSessionRef.current = SESSION_ID;
    connectGate.resolve(metadataClient);

    await expect(pendingMetadata).rejects.toMatchObject({ code: "stale_connection" });
    expect(metadataClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.workspaceClientRef.current).toBe(attachClient);
    expect(result.current.workspaceClientPromiseRef.current).toBeUndefined();
    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(attachClient);
  });

  it("operation client 复用唯一 workspace client", async () => {
    const client = makeClient();
    vi.spyOn(V070Client, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.openSessionOperationClient(SESSION_ID)).resolves.toEqual({
      client,
      ownsClient: false,
    });
    expect(client.close).not.toHaveBeenCalled();
  });
});
