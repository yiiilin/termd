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
import { buildAttachFramePayload, encodeSupervisorTerminalServerFrame } from "../protocol/supervisor-terminal";
import type {
  Envelope,
  SessionAttachedPayload,
  SessionCwdChangedPayload,
  SessionFilesResultPayload,
  TerminalSize,
  UUID,
} from "../protocol/types";
import type { TerminalOutputItem } from "../components/terminal/types";

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
    // 中文注释：receive loop 是单条顺序读取；空队列时保持挂起，避免测试因为“没有更多消息”
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
    type: "attach_frame",
    payload: buildAttachFramePayload(
      SESSION_ID,
      encodeSupervisorTerminalServerFrame({
        type: "attach_sync",
        session_id: SESSION_ID,
        base_seq: 1,
        snapshot: {
          size: DEFAULT_SIZE,
          process_id: 7,
          retained_output_bytes: new TextEncoder().encode(text),
        },
        frames: [],
      }),
    ),
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

function settleWithin<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<never>((_, reject) => {
      setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
    }),
  ]);
}

function useReconnectHarness(input: {
  authenticatedClient: (timeoutMs: number, signal?: AbortSignal) => Promise<DirectClient>;
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
    closeAttachForReconnect: input.closeAttachForReconnect ?? ((client?: DirectClient) => {
      const belongsToCurrentAttach =
        !client ||
        attachClientRef.current === client ||
        pendingAttachClientRef.current === client;
      if (!belongsToCurrentAttach) {
        client?.close();
        return false;
      }
      controller.pendingTerminalAttachAbortControllerRef.current?.abort();
      controller.pendingTerminalAttachAbortControllerRef.current = undefined;
      return true;
    }),
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
    claimAttachClient: (client) => {
      attachClientRef.current = client;
    },
    upsertAttachedSession: (current, attached) => [
      ...current.filter((session) => session.session_id !== attached.session_id),
      attached,
    ],
  });
  controller.attachReconnectHandlerRef.current = scheduleReconnect;

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
      expect(
        result.current.scheduleReconnect(
          asDirectClient(firstReconnectClient as unknown as FakeDirectClient),
          new ProtocolClientError("terminal_resync", "second resync while first reset is pending"),
          { forceFullSnapshot: true },
        ),
      ).toBe(true);
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

describe("useTerminalReceiveLoop", () => {
  it("旧 supervisor 同时返回 retained_output 和 snapshot frame 时只渲染 frames.snapshot", async () => {
    const prompt = "root@xieyilin-dev:~# ";
    const client = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 7,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new TextEncoder().encode(prompt),
            },
            frames: [
              {
                kind: "snapshot",
                session_id: SESSION_ID,
                base_seq: 7,
                size: DEFAULT_SIZE,
                data_bytes: new TextEncoder().encode(prompt),
              },
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 7,
                data_bytes: new TextEncoder().encode(prompt),
              },
            ],
          }),
        ),
      },
    ]);
    const output: TerminalOutputItem[] = [];
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: false };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: (item) => output.push(item),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(output).toHaveLength(1);
    });
    expect(output[0]).toEqual(expect.objectContaining({ kind: "snapshot", baseSeq: 7 }));
    expect(new TextDecoder().decode(output[0]?.kind === "snapshot" ? output[0].bytes : new Uint8Array())).toBe(prompt);
  });

  it("attach_sync 里 snapshot 已覆盖的 pre-sync output 不会再被补回", async () => {
    const prompt = "root@xieyilin-dev:~# ";
    const client = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 7,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new TextEncoder().encode(prompt),
            },
            frames: [
              {
                kind: "snapshot",
                session_id: SESSION_ID,
                base_seq: 7,
                size: DEFAULT_SIZE,
                data_bytes: new TextEncoder().encode(prompt),
              },
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 7,
                data_bytes: new TextEncoder().encode(prompt),
              },
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 8,
                data_bytes: new TextEncoder().encode("next-line\n"),
              },
            ],
          }),
        ),
      },
    ]);
    const output: TerminalOutputItem[] = [];
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: false };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: (item) => output.push(item),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(output).toHaveLength(2);
    });
    expect(output[0]).toEqual(expect.objectContaining({ kind: "snapshot", baseSeq: 7 }));
    expect(output[1]).toEqual(expect.objectContaining({ kind: "output", terminalSeq: 8 }));
    expect(output.some((item) => item.kind === "output" && item.terminalSeq === 7)).toBe(false);
    expect(new TextDecoder().decode(output[0]?.kind === "snapshot" ? output[0].bytes : new Uint8Array())).toBe(prompt);
    expect(new TextDecoder().decode(output[1]?.kind === "output" ? output[1].bytes : new Uint8Array())).toBe("next-line\n");
  });

  it("先到的 terminal_frame 被后续 snapshot 覆盖时不会从 pre-sync buffer 补回", async () => {
    const prompt = "root@xieyilin-dev:~# ";
    const client = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "terminal_frame",
            session_id: SESSION_ID,
            frame: {
              kind: "output",
              session_id: SESSION_ID,
              terminal_seq: 7,
              data_bytes: new TextEncoder().encode("buffered-duplicate\n"),
            },
          }),
        ),
      },
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 7,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new Uint8Array(),
            },
            frames: [
              {
                kind: "snapshot",
                session_id: SESSION_ID,
                base_seq: 7,
                size: DEFAULT_SIZE,
                data_bytes: new TextEncoder().encode(prompt),
              },
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 8,
                data_bytes: new TextEncoder().encode("fresh-tail\n"),
              },
            ],
          }),
        ),
      },
    ]);
    const output: TerminalOutputItem[] = [];
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: false };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: (item) => output.push(item),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(output).toHaveLength(2);
    });
    expect(output[0]).toEqual(expect.objectContaining({ kind: "snapshot", baseSeq: 7 }));
    expect(output[1]).toEqual(expect.objectContaining({ kind: "output", terminalSeq: 8 }));
    const renderedText = output
      .map((item) =>
        item.kind === "snapshot" || item.kind === "output"
          ? new TextDecoder().decode(item.bytes)
          : "",
      )
      .join("");
    expect(renderedText).toBe(`${prompt}fresh-tail\n`);
    expect(renderedText).not.toContain("buffered-duplicate");
  });

  it("tail-only attach_sync 首帧等于 base_seq 时从首帧前一位播种", async () => {
    const client = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 2,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new Uint8Array(),
            },
            frames: [
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 2,
                data_bytes: new TextEncoder().encode("beta\n"),
              },
            ],
          }),
        ),
      },
    ]);
    const output: TerminalOutputItem[] = [];
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: false };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: (item) => output.push(item),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      controller.lastRenderedTerminalSeqRef.current.set(SESSION_ID, 1);
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(output).toHaveLength(2);
    });
    expect(output[0]).toEqual({ kind: "sync", baseSeq: 1 });
    expect(output[1]).toEqual(expect.objectContaining({ kind: "output", terminalSeq: 2 }));
    expect(new TextDecoder().decode(output[1]?.kind === "output" ? output[1].bytes : new Uint8Array())).toBe("beta\n");
  });

  it("大 attach_sync 处理中途变 stale 时不会把尾部 output 写进下一代终端队列", async () => {
    const client = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 1,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new TextEncoder().encode("snapshot-before-stale\n"),
            },
            frames: [
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 2,
                data_bytes: new TextEncoder().encode("late-output-1\n"),
              },
              {
                kind: "output",
                session_id: SESSION_ID,
                terminal_seq: 3,
                data_bytes: new TextEncoder().encode("late-output-2\n"),
              },
            ],
          }),
        ),
      },
    ]);
    const output: TerminalOutputItem[] = [];
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: false };
    let enqueueCount = 0;
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: (item) => {
          output.push(item);
          enqueueCount += 1;
          if (enqueueCount === 1) {
            // 中文注释：模拟用户在旧 loop 还没处理完大 attach_sync 时切 session/reconnect。
            // 第一帧 snapshot 已经入队后，剩余 output 必须被 generation 栅栏挡住。
            controller.receiveLoopActiveRef.current = false;
            controller.receiveLoopGenerationRef.current += 1;
          }
        },
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(output).toHaveLength(1);
      expect(output[0]).toEqual(expect.objectContaining({ kind: "snapshot" }));
    });
    await act(async () => {
      await Promise.resolve();
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(output).toHaveLength(1);
    expect(output.some((item) => item.kind === "output")).toBe(false);
  });

  it("tail reconnect 遇到空 attach_sync 且 base_seq 前跳时会升级成 full snapshot", async () => {
    const output: TerminalOutputItem[] = [];
    const initialClient = new FakeDirectClient();
    const gapReconnectClient = new FakeDirectClient([
      {
        type: "attach_frame",
        payload: buildAttachFramePayload(
          SESSION_ID,
          encodeSupervisorTerminalServerFrame({
            type: "attach_sync",
            session_id: SESSION_ID,
            base_seq: 22,
            snapshot: {
              size: DEFAULT_SIZE,
              process_id: 7,
              retained_output_bytes: new Uint8Array(),
            },
            frames: [],
          }),
        ),
      },
    ]);
    const fullSnapshotReconnectClient = new FakeDirectClient([terminalSnapshot("history-after-gap\n")]);
    const reconnectClients = [gapReconnectClient, fullSnapshotReconnectClient];
    const authenticatedClient = vi.fn(async () => {
      const client = reconnectClients.shift();
      if (!client) {
        throw new Error("unexpected reconnect client request");
      }
      return asDirectClient(client);
    });
    const { result, unmount } = renderHook(() => useReconnectHarness({
      authenticatedClient,
      output,
      reconnectDelaysMs: [0, 0],
    }));

    act(() => {
      result.current.controller.attachedSessionRef.current = SESSION_ID;
      result.current.controller.lastRenderedTerminalSeqRef.current.set(SESSION_ID, 5);
      result.current.attachClientRef.current = asDirectClient(initialClient);
      expect(result.current.scheduleReconnect(
        asDirectClient(initialClient),
        new ProtocolClientError("connection_closed", "tail reconnect"),
      )).toBe(true);
    });

    await waitFor(() => expect(gapReconnectClient.attachCalls).toHaveLength(1));
    await waitFor(() => expect(fullSnapshotReconnectClient.attachCalls).toHaveLength(1));
    expect(gapReconnectClient.attachCalls[0]?.options.lastTerminalSeq).toBe(5);
    expect(fullSnapshotReconnectClient.attachCalls[0]?.options.lastTerminalSeq).toBeUndefined();
    await waitFor(() => {
      expect(output).toEqual([
        expect.objectContaining({
          kind: "snapshot",
          revealHistory: false,
        }),
      ]);
    });

    act(() => {
      result.current.controller.receiveLoopActiveRef.current = false;
      result.current.controller.receiveLoopGenerationRef.current += 1;
    });
    unmount();
  });

  it("跟随 terminal cwd 时收到 session_cwd_changed 会静默重拉 session.files", async () => {
    const loadSessionFiles = vi.fn(async () => undefined);
    const requestFollowSessionFilesRefresh = vi.fn(async () => undefined);
    const client = new FakeDirectClient([
      {
        type: "session_cwd_changed",
        payload: {
          session_id: SESSION_ID,
          cwd: "/tmp/follow-cwd",
        } satisfies SessionCwdChangedPayload,
      },
    ]);
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: true };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: vi.fn(),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
        loadSessionFiles,
        requestFollowSessionFilesRefresh,
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(requestFollowSessionFilesRefresh).toHaveBeenCalledWith(SESSION_ID);
    });
    expect(loadSessionFiles).not.toHaveBeenCalled();

    act(() => {
      controller.receiveLoopActiveRef.current = false;
      controller.receiveLoopGenerationRef.current += 1;
    });
  });

  it("跟随 terminal cwd 时仍兼容旧 daemon 被动推送 session_files_result", async () => {
    const handlePassiveSessionFilesResult = vi.fn();
    const legacyFiles = {
      session_id: SESSION_ID,
      path: "/tmp/legacy-follow",
      entries: [
        {
          name: "legacy.txt",
          path: "/tmp/legacy-follow/legacy.txt",
          kind: "file",
          size_bytes: 6,
          modified_at_ms: null,
        },
      ],
    } satisfies SessionFilesResultPayload;
    const client = new FakeDirectClient([
      {
        type: "session_files_result",
        payload: legacyFiles,
      },
    ]);
    const controller = renderHook(() => useTerminalAttach()).result.current;
    const attachClientRef = { current: asDirectClient(client) };
    const sessionFilesFollowTerminalCwdRef = { current: true };
    const startReceiveLoop = renderHook(() =>
      useTerminalReceiveLoop(controller, {
        attachClientRef,
        sessionFilesFollowTerminalCwdRef,
        applyConfirmedSessionSize: vi.fn(),
        enqueueTerminalOutput: vi.fn(),
        isIgnoredClosingSessionError: vi.fn(() => false),
        markNewOutputIfBackground: vi.fn(),
        setSafeError: vi.fn(),
        setSessionFiles: vi.fn(),
        setSessionFilesError: vi.fn(),
        setSessionFilesLoading: vi.fn(),
        setSessionGit: vi.fn(),
        setSessionGitError: vi.fn(),
        setSessionGitLoading: vi.fn(),
        handlePassiveSessionFilesResult,
      }),
    ).result.current;

    act(() => {
      controller.attachedSessionRef.current = SESSION_ID;
      startReceiveLoop(asDirectClient(client));
    });

    await waitFor(() => {
      expect(handlePassiveSessionFilesResult).toHaveBeenCalledWith(legacyFiles);
    });

    act(() => {
      controller.receiveLoopActiveRef.current = false;
      controller.receiveLoopGenerationRef.current += 1;
    });
  });

  it("stale client 的迟到错误不会中断当前 pending reconnect attach", async () => {
    const initialClient = new FakeDirectClient();
    const firstAttach = deferred<SessionAttachedPayload>();
    const firstReconnectClient = {
      attachCalls: [] as Array<{ sessionId: UUID; options: { lastTerminalSeq?: number; timeoutMs?: number; signal?: AbortSignal } }>,
      closed: false,
      attachSession: vi.fn(async (
        sessionId: UUID,
        options: { lastTerminalSeq?: number; timeoutMs?: number; signal?: AbortSignal } = {},
      ) => {
        firstReconnectClient.attachCalls.push({ sessionId, options });
        return firstAttach.promise;
      }),
      receiveInner: async () => new Promise<Envelope>(() => undefined),
      detachSession: () => {
        firstReconnectClient.closed = true;
      },
      close: () => {
        firstReconnectClient.closed = true;
      },
    };
    const secondReconnectClient = new FakeDirectClient();
    const clients = [asDirectClient(firstReconnectClient as unknown as FakeDirectClient), asDirectClient(secondReconnectClient)];
    const authenticatedClient = vi.fn(async (_timeoutMs: number, signal?: AbortSignal) => {
      expect(signal?.aborted).toBe(false);
      const client = clients.shift();
      if (!client) {
        throw new Error("unexpected reconnect client request");
      }
      return client;
    });

    const { result, unmount } = renderHook(() => useReconnectHarness({
      authenticatedClient,
      output: [],
      reconnectDelaysMs: [0, 0],
    }));

    act(() => {
      result.current.controller.attachedSessionRef.current = SESSION_ID;
      result.current.attachClientRef.current = asDirectClient(initialClient);
      expect(result.current.scheduleReconnect(
        asDirectClient(initialClient),
        new ProtocolClientError("connection_closed", "first reconnect"),
      )).toBe(true);
    });

    await waitFor(() => expect(firstReconnectClient.attachSession).toHaveBeenCalledTimes(1));

    act(() => {
      result.current.scheduleReconnect(
        asDirectClient(new FakeDirectClient()),
        new ProtocolClientError("connection_closed", "late stale client error"),
        { reconnectKey: `${SERVER_ID}:${SESSION_ID}` },
      );
    });

    await expect(settleWithin(firstAttach.promise, 50, "stale reconnect should stay pending")).rejects.toThrow(
      "stale reconnect should stay pending timed out",
    );
    expect(firstReconnectClient.closed).toBe(false);
    expect(secondReconnectClient.attachCalls).toHaveLength(0);

    act(() => {
      result.current.controller.receiveLoopActiveRef.current = false;
      result.current.controller.receiveLoopGenerationRef.current += 1;
    });
    unmount();
  });
});
