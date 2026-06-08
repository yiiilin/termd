import { useRef } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  useTerminalAttach,
  useTerminalReceiveLoop,
  useTerminalReconnectScheduler,
  type AttachReconnectOptions,
} from "../hooks/useTerminalAttach";
import { ProtocolClientError } from "../protocol/errors";
import type { DirectClient } from "../protocol/direct-client";
import type { Envelope, SessionAttachedPayload, TerminalSize, UUID } from "../protocol/types";
import type { TerminalOutputItem } from "../components/terminal/types";
import { sessionDataToBase64 } from "../protocol/wire";

const SESSION_ID = "00000000-0000-0000-0000-000000009901";
const SERVER_ID = "00000000-0000-0000-0000-000000009902";
const DEFAULT_SIZE: TerminalSize = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

class FakeDirectClient {
  public readonly attachCalls: Array<{ sessionId: UUID; options: { lastTerminalSeq?: number; timeoutMs?: number } }> = [];
  public closed = false;

  constructor(private readonly inbox: Envelope[] = []) {}

  async attachSession(
    sessionId: UUID,
    options: { lastTerminalSeq?: number; timeoutMs?: number } = {},
  ): Promise<SessionAttachedPayload> {
    this.attachCalls.push({ sessionId, options });
    return {
      session_id: sessionId,
      role: "operator",
      state: "running",
      size: DEFAULT_SIZE,
      resize_owner: true,
    };
  }

  async receiveInner(): Promise<Envelope> {
    const next = this.inbox.shift();
    if (next) {
      return next;
    }
    // 中文注释：receive loop 会并行启动两个 read；空队列时保持挂起，避免测试因第二个 read
    // 立刻抛错而进入重连分支，掩盖 snapshot token 的断言。
    return new Promise(() => undefined);
  }

  detachSession(): void {
    this.closed = true;
  }

  close(): void {
    this.closed = true;
  }
}

function terminalSnapshot(text: string): Envelope {
  return {
    type: "terminal_frame",
    payload: {
      kind: "snapshot",
      session_id: SESSION_ID,
      base_seq: 1,
      terminal_seq: 1,
      size: DEFAULT_SIZE,
      data_base64: sessionDataToBase64(new TextEncoder().encode(text)),
    },
  };
}

function asDirectClient(client: FakeDirectClient): DirectClient {
  return client as unknown as DirectClient;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function useReconnectHarness(input: {
  authenticatedClient: () => Promise<DirectClient>;
  output: TerminalOutputItem[];
  reconnectDelaysMs?: number[];
  closeAttachForReconnect?: (client?: DirectClient) => boolean;
  waitForTerminalOutputResetApplied?: (version: number) => Promise<void>;
}) {
  const controller = useTerminalAttach();
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const pendingAttachClientRef = useRef<DirectClient | undefined>(undefined);
  const sessionPermissionIdsRef = useRef<Set<UUID>>(new Set());
  const sessionOrderRef = useRef<UUID[]>([SESSION_ID]);
  const sessionFilesFollowTerminalCwdRef = useRef(false);
  const startReceiveLoop = useTerminalReceiveLoop(controller, {
    attachClientRef,
    sessionFilesFollowTerminalCwdRef,
    applyConfirmedSessionSize: vi.fn(),
    enqueueTerminalOutput: (item) => input.output.push(item),
    isIgnoredClosingSessionError: vi.fn(() => false),
    markNewOutputIfBackground: vi.fn(),
    setSafeError: vi.fn(),
    setSessionFiles: vi.fn(),
    setSessionFilesError: vi.fn(),
    setSessionFilesLoading: vi.fn(),
    setSessionGit: vi.fn(),
    setSessionGitError: vi.fn(),
    setSessionGitLoading: vi.fn(),
  });
  const scheduleReconnect = useTerminalReconnectScheduler(controller, {
    attachClientRef,
    pendingAttachClientRef,
    activeServerId: SERVER_ID,
    attachedSessionId: SESSION_ID,
    selectedSessionId: SESSION_ID,
    authenticatedClient: input.authenticatedClient,
    attachConnectionTimeoutMs: 1000,
    reconnectDelaysMs: input.reconnectDelaysMs ?? [0],
    isRetryableConnectionError: () => true,
    isTerminalTransportPaused: () => false,
    closeAttachForReconnect: input.closeAttachForReconnect ?? (() => true),
    discardPendingTerminalOutput: vi.fn(),
    resetAttachReconnectState: () => {
      if (controller.attachReconnectTimerRef.current !== undefined) {
        window.clearTimeout(controller.attachReconnectTimerRef.current);
        controller.attachReconnectTimerRef.current = undefined;
      }
      controller.attachReconnectKeyRef.current = undefined;
      controller.attachReconnectAttemptsRef.current = 0;
      controller.attachReconnectLastErrorRef.current = undefined;
    },
    setError: vi.fn(),
    setStatus: vi.fn(),
    setSafeError: vi.fn(),
    setAttachedSessionId: vi.fn(),
    setSessions: vi.fn(),
    sessionOrderRef,
    sessionPermissionIdsRef,
    clearNewOutputMark: vi.fn(),
    clearTerminalOutput: vi.fn(() => 1),
    clearTerminalSnapshotRevealHistory: (sessionId?: UUID, snapshotToken?: number) => {
      if (!sessionId) {
        controller.terminalSnapshotRevealHistoryTokensRef.current.clear();
        controller.terminalSnapshotPendingFullSnapshotTokensRef.current.clear();
        return;
      }
      const revealToken = controller.terminalSnapshotRevealHistoryTokensRef.current.get(sessionId);
      if (snapshotToken === undefined || revealToken === snapshotToken) {
        controller.terminalSnapshotRevealHistoryTokensRef.current.delete(sessionId);
      }
      const pendingSnapshot = controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
      if (snapshotToken === undefined || pendingSnapshot?.token === snapshotToken) {
        controller.terminalSnapshotPendingFullSnapshotTokensRef.current.delete(sessionId);
      }
    },
    waitForTerminalOutputResetApplied: input.waitForTerminalOutputResetApplied ?? vi.fn(async () => undefined),
    selectSession: vi.fn(),
    startReceiveLoop,
    loadSessionFiles: vi.fn(async () => undefined),
    loadSessionGit: vi.fn(async () => undefined),
    refreshDaemonClients: vi.fn(async () => undefined),
    upsertAttachedSession: (current, attached) => [
      ...current.filter((session) => session.session_id !== attached.session_id),
      attached,
    ],
  });

  return { controller, attachClientRef, scheduleReconnect };
}

describe("useTerminalAttach snapshot reveal intent", () => {
  it("普通 full snapshot 调度不会取消已升级的 reveal-history token", async () => {
    const output: TerminalOutputItem[] = [];
    const initialClient = new FakeDirectClient();
    const reconnectClient = new FakeDirectClient([terminalSnapshot("history\n")]);
    const authenticatedClient = vi.fn(async () => asDirectClient(reconnectClient));
    const { result, unmount } = renderHook(() => useReconnectHarness({ authenticatedClient, output }));

    act(() => {
      result.current.controller.attachedSessionRef.current = SESSION_ID;
      result.current.attachClientRef.current = asDirectClient(initialClient);
    });

    const schedule = (options: AttachReconnectOptions) =>
      result.current.scheduleReconnect(
        asDirectClient(initialClient),
        new ProtocolClientError("terminal_resync", "test resync"),
        options,
      );

    act(() => {
      expect(schedule({ forceFullSnapshot: true })).toBe(true);
    });
    const snapshotToken = result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token;
    expect(snapshotToken).toBeTypeOf("number");

    act(() => {
      expect(schedule({ forceFullSnapshot: true, revealHistory: true })).toBe(true);
    });
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(snapshotToken);

    act(() => {
      expect(schedule({ forceFullSnapshot: true })).toBe(true);
    });
    // 中文注释：主题切换这类普通 full resync 可以复用当前 pending token，
    // 但不能把用户滚轮升级出来的 reveal intent 删掉。
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(snapshotToken);

    await waitFor(() => expect(authenticatedClient).toHaveBeenCalledTimes(1));
    await waitFor(() => {
      expect(output).toEqual([
        expect.objectContaining({
          kind: "snapshot",
          revealHistory: true,
        }),
      ]);
    });
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.has(SESSION_ID)).toBe(false);
    expect(result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.has(SESSION_ID)).toBe(false);
    expect(reconnectClient.attachCalls).toHaveLength(1);
    expect(reconnectClient.attachCalls[0]?.options.lastTerminalSeq).toBeUndefined();

    act(() => {
      result.current.controller.receiveLoopActiveRef.current = false;
      result.current.controller.receiveLoopGenerationRef.current += 1;
    });
    unmount();
  });

  it("旧 reconnect 在 reset 等待后变 stale 时不会删除新 reconnect 的 reveal token", async () => {
    const output: TerminalOutputItem[] = [];
    const initialClient = new FakeDirectClient();
    const firstReconnectClient = new FakeDirectClient();
    const secondReconnectClient = new FakeDirectClient([terminalSnapshot("history-after-stale\n")]);
    const clients = [firstReconnectClient, secondReconnectClient];
    const authenticatedClient = vi.fn(async () => {
      const client = clients.shift();
      if (!client) {
        throw new Error("unexpected reconnect client request");
      }
      return asDirectClient(client);
    });
    const firstReset = deferred<void>();
    const secondReset = deferred<void>();
    const waitForTerminalOutputResetApplied = vi.fn((version: number) => {
      void version;
      return waitForTerminalOutputResetApplied.mock.calls.length === 1
        ? firstReset.promise
        : secondReset.promise;
    });
    const { result, unmount } = renderHook(() =>
      useReconnectHarness({
        authenticatedClient,
        output,
        reconnectDelaysMs: [0, 0, 0],
        waitForTerminalOutputResetApplied,
      }),
    );

    act(() => {
      result.current.controller.attachedSessionRef.current = SESSION_ID;
      result.current.attachClientRef.current = asDirectClient(initialClient);
    });

    const schedule = (options: AttachReconnectOptions) =>
      result.current.scheduleReconnect(
        asDirectClient(initialClient),
        new ProtocolClientError("terminal_resync", "test resync"),
        options,
      );

    act(() => {
      expect(schedule({ forceFullSnapshot: true })).toBe(true);
      expect(schedule({ forceFullSnapshot: true, revealHistory: true })).toBe(true);
    });
    const firstToken = result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token;
    expect(firstToken).toBeTypeOf("number");
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(firstToken);

    await waitFor(() => expect(firstReconnectClient.attachCalls).toHaveLength(1));
    await waitFor(() => expect(waitForTerminalOutputResetApplied).toHaveBeenCalledTimes(1));

    act(() => {
      expect(schedule({ forceFullSnapshot: true })).toBe(true);
    });
    const secondToken = result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token;
    // 中文注释：第一个 reconnect 已经 claim 了 token；后续 full resync 必须换新 token，
    // 并把旧 token 上的 reveal intent 转移过去，避免旧 stale cleanup 误删新 intent。
    expect(secondToken).toBeTypeOf("number");
    expect(secondToken).not.toBe(firstToken);
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(secondToken);

    act(() => {
      expect(
        result.current.scheduleReconnect(
          asDirectClient(initialClient),
          new ProtocolClientError("connection_closed", "late retry from old reconnect"),
          {
            forceFullSnapshot: true,
            snapshotToken: firstToken,
            skipCurrentClientClose: true,
          },
        ),
      ).toBe(true);
    });
    expect(result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token).toBe(secondToken);
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(secondToken);

    await waitFor(() => expect(secondReconnectClient.attachCalls).toHaveLength(1));
    await waitFor(() => expect(waitForTerminalOutputResetApplied).toHaveBeenCalledTimes(2));

    await act(async () => {
      firstReset.resolve();
      await firstReset.promise;
    });
    await waitFor(() => expect(firstReconnectClient.closed).toBe(true));
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(secondToken);
    expect(result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token).toBe(secondToken);

    await act(async () => {
      secondReset.resolve();
      await secondReset.promise;
    });
    await waitFor(() => {
      expect(output).toEqual([
        expect.objectContaining({
          kind: "snapshot",
          revealHistory: true,
        }),
      ]);
    });

    act(() => {
      result.current.controller.receiveLoopActiveRef.current = false;
      result.current.controller.receiveLoopGenerationRef.current += 1;
    });
    unmount();
  });

  it("旧 client 的迟到错误不会清理当前 reconnect 的 reveal token", () => {
    const output: TerminalOutputItem[] = [];
    const currentClient = new FakeDirectClient();
    const staleClient = new FakeDirectClient();
    const closeAttachForReconnect = vi.fn((client?: DirectClient) => client === asDirectClient(currentClient));
    const { result, unmount } = renderHook(() =>
      useReconnectHarness({
        authenticatedClient: vi.fn(async () => asDirectClient(new FakeDirectClient())),
        output,
        reconnectDelaysMs: [60_000],
        closeAttachForReconnect,
      }),
    );

    act(() => {
      result.current.controller.attachedSessionRef.current = SESSION_ID;
      result.current.attachClientRef.current = asDirectClient(currentClient);
    });

    act(() => {
      expect(result.current.scheduleReconnect(
        asDirectClient(currentClient),
        new ProtocolClientError("terminal_resync", "current resync"),
        { forceFullSnapshot: true, revealHistory: true },
      )).toBe(true);
    });
    const snapshotToken = result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token;
    expect(snapshotToken).toBeTypeOf("number");
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(snapshotToken);

    act(() => {
      expect(result.current.scheduleReconnect(
        asDirectClient(staleClient),
        new ProtocolClientError("connection_closed", "late stale sidecar error"),
        { forceFullSnapshot: true },
      )).toBe(true);
    });

    // 中文注释：旧 client 的迟到错误只应该关闭旧 client，不能从当前 pending map
    // 取出 live token 后再清掉；否则用户上滚升级出的 reveal intent 会丢失。
    expect(closeAttachForReconnect).toHaveBeenLastCalledWith(asDirectClient(staleClient));
    expect(result.current.controller.terminalSnapshotPendingFullSnapshotTokensRef.current.get(SESSION_ID)?.token).toBe(snapshotToken);
    expect(result.current.controller.terminalSnapshotRevealHistoryTokensRef.current.get(SESSION_ID)).toBe(snapshotToken);

    act(() => {
      if (result.current.controller.attachReconnectTimerRef.current !== undefined) {
        window.clearTimeout(result.current.controller.attachReconnectTimerRef.current);
        result.current.controller.attachReconnectTimerRef.current = undefined;
      }
    });
    unmount();
  });

});
