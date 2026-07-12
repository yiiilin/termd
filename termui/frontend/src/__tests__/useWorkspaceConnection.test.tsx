import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DirectClient } from "../protocol/direct-client";
import { useWorkspaceConnection } from "../hooks/useWorkspaceConnection";
import type { DeviceState, PairedServerState, UUID } from "../protocol/types";

const SERVER_ID = "00000000-0000-0000-0000-000000000101";
const DEVICE_ID = "00000000-0000-0000-0000-000000000201";
const SESSION_ID = "00000000-0000-0000-0000-000000000301";

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

function makeClient(overrides: Partial<Record<string, unknown>> = {}): DirectClient {
  const client = {
    isClosed: false,
    authenticate: vi.fn(async () => undefined),
    attachSessionPermission: vi.fn(async (_sessionId: UUID) => undefined),
    close: vi.fn(() => {
      client.isClosed = true;
    }),
    interruptReceiveWaiters: vi.fn(),
    ...overrides,
  };
  return client as unknown as DirectClient;
}

function renderWorkspaceConnection() {
  const attachedSessionRef = { current: undefined as UUID | undefined };
  const pendingTerminalAttachSessionRef = { current: undefined as UUID | undefined };
  const receiveLoopActiveRef = { current: false };
  const receiveLoopGenerationRef = { current: 0 };
  const result = renderHook(() =>
    useWorkspaceConnection({
      activeServer: makeServer(),
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
  it("并发获取 workspace client 只建立一条连接", async () => {
    const client = makeClient();
    const connectSpy = vi.spyOn(DirectClient, "connect").mockResolvedValue(client);
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
    vi.spyOn(DirectClient, "connect").mockResolvedValue(client);
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
    result.current.sessionPermissionIdsRef.current.add(SESSION_ID);
    pendingTerminalAttachSessionRef.current = SESSION_ID;
    receiveLoopActiveRef.current = true;
    receiveLoopGenerationRef.current = 7;

    result.current.closeWorkspaceClient();

    expect(activeClient.interruptReceiveWaiters).toHaveBeenCalledTimes(1);
    expect(activeClient.close).toHaveBeenCalledTimes(1);
    expect(pendingClient.interruptReceiveWaiters).toHaveBeenCalledTimes(1);
    expect(pendingClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.attachClientRef.current).toBeUndefined();
    expect(result.current.pendingAttachClientRef.current).toBeUndefined();
    expect(result.current.sessionPermissionIdsRef.current.size).toBe(0);
    expect(pendingTerminalAttachSessionRef.current).toBeUndefined();
    expect(receiveLoopActiveRef.current).toBe(false);
    expect(receiveLoopGenerationRef.current).toBe(8);
  });

  it("同一 session 的权限 attach 只补一次", async () => {
    const client = makeClient();
    vi.spyOn(DirectClient, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedSessionClient(SESSION_ID)).resolves.toBe(client);
    await expect(result.current.authenticatedSessionClient(SESSION_ID)).resolves.toBe(client);

    expect(client.attachSessionPermission).toHaveBeenCalledTimes(1);
    expect(client.attachSessionPermission).toHaveBeenCalledWith(SESSION_ID);
  });

  it("已 attach 后 workspace metadata 会回到当前 terminal client 并关闭旧 metadata 连接", async () => {
    const metadataClient = makeClient();
    const attachClient = makeClient();
    const connectSpy = vi.spyOn(DirectClient, "connect").mockResolvedValue(metadataClient);
    const { attachedSessionRef, result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(metadataClient);
    result.current.workspaceClientRef.current = metadataClient;
    result.current.attachClientRef.current = attachClient;
    attachedSessionRef.current = SESSION_ID;

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(attachClient);

    expect(metadataClient.interruptReceiveWaiters).toHaveBeenCalledTimes(1);
    expect(metadataClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.workspaceClientRef.current).toBeUndefined();
    expect(connectSpy).toHaveBeenCalledTimes(1);
  });

  it("将同一条 workspace client 提升为 terminal attach 时不关闭 transport", async () => {
    const workspaceClient = makeClient();
    vi.spyOn(DirectClient, "connect").mockResolvedValue(workspaceClient);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(workspaceClient);
    result.current.claimAttachClient(workspaceClient);

    expect(workspaceClient.interruptReceiveWaiters).not.toHaveBeenCalled();
    expect(workspaceClient.close).not.toHaveBeenCalled();
    expect(result.current.workspaceClientRef.current).toBeUndefined();
    expect(result.current.attachClientRef.current).toBe(workspaceClient);
  });

  it("metadata 建连未完成时若 terminal 先 attach，迟到 metadata client 会被作废", async () => {
    const authGate = deferred<void>();
    const metadataClient = makeClient({
      authenticate: vi.fn(() => authGate.promise),
    });
    const attachClient = makeClient();
    vi.spyOn(DirectClient, "connect").mockResolvedValue(metadataClient);
    const { attachedSessionRef, result } = renderWorkspaceConnection();

    const pendingMetadata = result.current.authenticatedWorkspaceClient();
    result.current.claimAttachClient(attachClient);
    attachedSessionRef.current = SESSION_ID;
    authGate.resolve();

    await expect(pendingMetadata).rejects.toMatchObject({
      code: "connection_closed",
    });
    expect(metadataClient.close).toHaveBeenCalledTimes(1);
    expect(result.current.workspaceClientRef.current).toBeUndefined();
    expect(result.current.workspaceClientPromiseRef.current).toBeUndefined();
    await expect(result.current.authenticatedWorkspaceClient()).resolves.toBe(attachClient);
  });

  it("独立 operation client 权限补齐失败时会关闭短连接", async () => {
    const client = makeClient({
      attachSessionPermission: vi.fn(async () => {
        throw new Error("permission failed");
      }),
    });
    vi.spyOn(DirectClient, "connect").mockResolvedValue(client);
    const { result } = renderWorkspaceConnection();

    await expect(result.current.openSessionOperationClient(SESSION_ID)).rejects.toThrow("permission failed");
    expect(client.close).toHaveBeenCalledTimes(1);
  });
});
