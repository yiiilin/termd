import { useCallback, useEffect, useRef, type Dispatch, type MutableRefObject, type SetStateAction } from "react";
import type { DirectClient } from "../protocol/direct-client";
import type {
  RenderableTerminalFramePayload,
  SessionActivityPayload,
  SessionAttachedPayload,
  SessionDataPayload,
  SessionFilesResultPayload,
  SessionGitResultPayload,
  SessionResizedPayload,
  SessionSummaryPayload,
  SafeError,
  TerminalSize,
  UUID,
} from "../protocol/types";
import { sessionDataFromBase64 } from "../protocol/wire";
import type { TerminalOutputItem } from "../components/terminal/types";
import { toSafeError } from "../protocol/errors";
import { recordTermdDiagnostic } from "../diagnostics";

const RECEIVE_LOOP_YIELD_MESSAGES = 64;
const RECEIVE_LOOP_YIELD_BYTES = 256 * 1024;

interface PendingFullSnapshotToken {
  reconnectKey: string;
  token: number;
  claimed: boolean;
}

export interface AttachReconnectOptions {
  lastTerminalSeq?: number;
  forceFullSnapshot?: boolean;
  revealHistory?: boolean;
  snapshotToken?: number;
  sessionId?: UUID;
  reconnectKey?: string;
  skipCurrentClientClose?: boolean;
}

export function useTerminalAttach() {
  const pendingTerminalAttachSessionRef = useRef<UUID | undefined>(undefined);
  const attachedSessionRef = useRef<UUID | undefined>(undefined);
  const autoAttachAttemptedSessionRef = useRef<UUID | undefined>(undefined);
  const attachingSessionIdRef = useRef<UUID | undefined>(undefined);
  const attachRequestIdRef = useRef(0);
  const sessionCreateRequestIdRef = useRef(0);
  const attachSwitchTimerRef = useRef<number | undefined>(undefined);
  const attachSwitchGenerationRef = useRef(0);
  const reattachCurrentSessionOnOpenRef = useRef(false);
  const userDetachedRef = useRef(false);
  const pendingResizeKeyRef = useRef<string | undefined>(undefined);
  const confirmedSessionSizesRef = useRef<Map<UUID, TerminalSize>>(new Map());
  const receiveLoopActiveRef = useRef(false);
  const receiveLoopGenerationRef = useRef(0);
  const terminalOutputQueueRef = useRef<TerminalOutputItem[]>([]);
  const lastRenderedTerminalSeqRef = useRef<Map<UUID, number>>(new Map());
  const terminalOutputResetVersionRef = useRef(0);
  const terminalOutputAppliedResetVersionRef = useRef(0);
  const terminalOutputResetWaitersRef = useRef<Map<number, Set<() => void>>>(new Map());
  const terminalOutputFlushFrameRef = useRef<number | undefined>(undefined);
  const terminalOutputFlushTimerRef = useRef<number | undefined>(undefined);
  const terminalOutputDrainRef = useRef<(() => void) | undefined>(undefined);
  const terminalSnapshotTokenSeqRef = useRef(0);
  const terminalSnapshotRevealHistoryTokensRef = useRef<Map<UUID, number>>(new Map());
  const terminalSnapshotPendingFullSnapshotTokensRef = useRef<Map<UUID, PendingFullSnapshotToken>>(new Map());
  const terminalSnapshotClientFullSnapshotTokensRef = useRef<WeakMap<DirectClient, { sessionId: UUID; token: number }>>(new WeakMap());
  const attachReconnectTimerRef = useRef<number | undefined>(undefined);
  const attachReconnectKeyRef = useRef<string | undefined>(undefined);
  const attachReconnectAttemptsRef = useRef(0);
  const attachReconnectLastErrorRef = useRef<unknown>(undefined);
  const attachReconnectHandlerRef = useRef<(client: DirectClient, caught: unknown, options?: AttachReconnectOptions) => boolean>(() => false);

  return {
    pendingTerminalAttachSessionRef,
    attachedSessionRef,
    autoAttachAttemptedSessionRef,
    attachingSessionIdRef,
    attachRequestIdRef,
    sessionCreateRequestIdRef,
    attachSwitchTimerRef,
    attachSwitchGenerationRef,
    reattachCurrentSessionOnOpenRef,
    userDetachedRef,
    pendingResizeKeyRef,
    confirmedSessionSizesRef,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    terminalOutputQueueRef,
    lastRenderedTerminalSeqRef,
    terminalOutputResetVersionRef,
    terminalOutputAppliedResetVersionRef,
    terminalOutputResetWaitersRef,
    terminalOutputFlushFrameRef,
    terminalOutputFlushTimerRef,
    terminalOutputDrainRef,
    terminalSnapshotTokenSeqRef,
    terminalSnapshotRevealHistoryTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    terminalSnapshotClientFullSnapshotTokensRef,
    attachReconnectTimerRef,
    attachReconnectKeyRef,
    attachReconnectAttemptsRef,
    attachReconnectLastErrorRef,
    attachReconnectHandlerRef,
  };
}

export type TerminalAttachController = ReturnType<typeof useTerminalAttach>;

function yieldToEventLoop(): Promise<void> {
  return new Promise((resolve) => {
    globalThis.setTimeout(resolve, 0);
  });
}

interface UseTerminalReceiveLoopOptions {
  attachClientRef: MutableRefObject<DirectClient | undefined>;
  sessionFilesFollowTerminalCwdRef: MutableRefObject<boolean>;
  applyConfirmedSessionSize: (sessionId: UUID, size: TerminalSize) => void;
  enqueueTerminalOutput: (item: TerminalOutputItem) => void;
  isIgnoredClosingSessionError: (sessionId: UUID, caught: unknown) => boolean;
  markNewOutputIfBackground: (sessionId: UUID) => void;
  setSafeError: (caught: unknown) => void;
  setSessionFiles: (files: SessionFilesResultPayload | undefined) => void;
  setSessionFilesError: (error: SafeError | undefined) => void;
  setSessionFilesLoading: (loading: boolean) => void;
  setSessionGit: (git: SessionGitResultPayload | undefined) => void;
  setSessionGitError: (error: SafeError | undefined) => void;
  setSessionGitLoading: (loading: boolean) => void;
}

export function useTerminalReceiveLoop(
  controller: TerminalAttachController,
  options: UseTerminalReceiveLoopOptions,
) {
  const {
    attachClientRef,
    sessionFilesFollowTerminalCwdRef,
    applyConfirmedSessionSize,
    enqueueTerminalOutput,
    isIgnoredClosingSessionError,
    markNewOutputIfBackground,
    setSafeError,
    setSessionFiles,
    setSessionFilesError,
    setSessionFilesLoading,
    setSessionGit,
    setSessionGitError,
    setSessionGitLoading,
  } = options;
  const {
    attachedSessionRef,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    attachReconnectHandlerRef,
    terminalSnapshotRevealHistoryTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    terminalSnapshotClientFullSnapshotTokensRef,
  } = controller;
  const terminalOutputTraceCountRef = useRef(0);

  return useCallback((client: DirectClient) => {
    const loopGeneration = receiveLoopGenerationRef.current + 1;
    receiveLoopGenerationRef.current = loopGeneration;
    receiveLoopActiveRef.current = true;
    recordTermdDiagnostic("receive_loop_start", {
      loopGeneration,
      attachedSessionId: attachedSessionRef.current,
    });
    const isCurrentLoop = () =>
      receiveLoopActiveRef.current &&
      receiveLoopGenerationRef.current === loopGeneration &&
      attachClientRef.current === client;
    const read = async () => {
      let processedMessages = 0;
      let processedBytes = 0;
      while (isCurrentLoop()) {
        try {
          const inner = await client.receiveInner();
          if (!isCurrentLoop()) {
            return;
          }
          processedMessages += 1;
          if (inner.type === "session_data") {
            const payload = inner.payload as SessionDataPayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES) {
                processedMessages = 0;
                await yieldToEventLoop();
              }
              continue;
            }
            const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
            terminalOutputTraceCountRef.current += 1;
            if (terminalOutputTraceCountRef.current % 256 === 1) {
              recordTermdDiagnostic("receive_loop_session_data", {
                sessionId: payload.session_id,
                bytes: bytes.byteLength,
                sample: terminalOutputTraceCountRef.current,
              });
            }
            enqueueTerminalOutput({ kind: "data", bytes });
            processedBytes += bytes.byteLength;
          } else if (inner.type === "terminal_frame") {
            const payload = inner.payload as RenderableTerminalFramePayload;
            if (payload.session_id !== attachedSessionRef.current) {
              markNewOutputIfBackground(payload.session_id);
              if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES) {
                processedMessages = 0;
                await yieldToEventLoop();
              }
              continue;
            }
            if (payload.kind === "snapshot") {
              applyConfirmedSessionSize(payload.session_id, payload.size);
              const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
              const fullSnapshotToken = terminalSnapshotClientFullSnapshotTokensRef.current.get(client);
              const snapshotToken = fullSnapshotToken?.sessionId === payload.session_id ? fullSnapshotToken.token : undefined;
              const revealToken = terminalSnapshotRevealHistoryTokensRef.current.get(payload.session_id);
              const revealHistory = snapshotToken !== undefined && revealToken === snapshotToken;
              recordTermdDiagnostic("receive_loop_terminal_snapshot", {
                sessionId: payload.session_id,
                baseSeq: payload.base_seq,
                bytes: bytes.byteLength,
                size: payload.size,
                snapshotToken,
                revealToken,
                revealHistory,
              });
              if (revealHistory) {
                terminalSnapshotRevealHistoryTokensRef.current.delete(payload.session_id);
              }
              if (snapshotToken !== undefined) {
                const pendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(payload.session_id);
                if (pendingSnapshot?.token === snapshotToken) {
                  terminalSnapshotPendingFullSnapshotTokensRef.current.delete(payload.session_id);
                }
                terminalSnapshotClientFullSnapshotTokensRef.current.delete(client);
              }
              enqueueTerminalOutput({
                kind: "snapshot",
                bytes,
                baseSeq: payload.base_seq,
                size: payload.size,
                revealHistory,
              });
              processedBytes += bytes.byteLength;
            } else if (payload.kind === "output") {
              const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
              terminalOutputTraceCountRef.current += 1;
              if (terminalOutputTraceCountRef.current % 256 === 1) {
                recordTermdDiagnostic("receive_loop_terminal_output", {
                  sessionId: payload.session_id,
                  terminalSeq: payload.terminal_seq,
                  bytes: bytes.byteLength,
                  sample: terminalOutputTraceCountRef.current,
                });
              }
              enqueueTerminalOutput({
                kind: "output",
                bytes,
                terminalSeq: payload.terminal_seq,
              });
              processedBytes += bytes.byteLength;
            } else if (payload.kind === "resize") {
              recordTermdDiagnostic("receive_loop_terminal_resize", {
                sessionId: payload.session_id,
                terminalSeq: payload.terminal_seq,
                size: payload.size,
              });
              enqueueTerminalOutput({ kind: "resize", terminalSeq: payload.terminal_seq, size: payload.size });
            } else if (payload.kind === "exit") {
              recordTermdDiagnostic("receive_loop_terminal_exit", {
                sessionId: payload.session_id,
                terminalSeq: payload.terminal_seq,
              });
              enqueueTerminalOutput({ kind: "exit", terminalSeq: payload.terminal_seq });
            }
          } else if (inner.type === "session_activity") {
            const payload = inner.payload as SessionActivityPayload;
            markNewOutputIfBackground(payload.session_id);
          } else if (inner.type === "session_files_result") {
            const payload = inner.payload as SessionFilesResultPayload;
            // 非跟随模式下只接受当前请求的直接回写，不再让 daemon 的后台推送覆盖手动浏览目录。
            if (payload.session_id === attachedSessionRef.current && sessionFilesFollowTerminalCwdRef.current) {
              setSessionFiles(payload);
              setSessionFilesError(undefined);
              setSessionFilesLoading(false);
            }
          } else if (inner.type === "session_git_result") {
            const payload = inner.payload as SessionGitResultPayload;
            if (payload.session_id === attachedSessionRef.current) {
              setSessionGit(payload);
              setSessionGitError(undefined);
              setSessionGitLoading(false);
            }
          } else if (inner.type === "session_resized") {
            const payload = inner.payload as SessionResizedPayload;
            applyConfirmedSessionSize(payload.session_id, payload.size);
          }
          if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES || processedBytes >= RECEIVE_LOOP_YIELD_BYTES) {
            processedMessages = 0;
            processedBytes = 0;
            await yieldToEventLoop();
          }
        } catch (caught) {
          // 旧 attach 关闭可能晚于新 attach 启动；只有当前 client 的错误才能切到错误态。
          if (isCurrentLoop()) {
            const sessionId = attachedSessionRef.current;
            recordTermdDiagnostic("receive_loop_error", {
              loopGeneration,
              attachedSessionId: sessionId,
              code: toSafeError(caught).code,
              message: toSafeError(caught).message,
            });
            if (sessionId && isIgnoredClosingSessionError(sessionId, caught)) {
              return;
            }
            if (attachReconnectHandlerRef.current(client, caught)) {
              return;
            }
            setSafeError(caught);
          }
          return;
        }
      }
    };
    void read();
  }, [
    attachReconnectHandlerRef,
    attachedSessionRef,
    applyConfirmedSessionSize,
    attachClientRef,
    enqueueTerminalOutput,
    isIgnoredClosingSessionError,
    markNewOutputIfBackground,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    sessionFilesFollowTerminalCwdRef,
    setSafeError,
    setSessionFiles,
    setSessionFilesError,
    setSessionFilesLoading,
    setSessionGit,
    setSessionGitError,
    setSessionGitLoading,
    terminalSnapshotClientFullSnapshotTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    terminalSnapshotRevealHistoryTokensRef,
  ]);
}

interface UseTerminalReconnectSchedulerOptions {
  attachClientRef: MutableRefObject<DirectClient | undefined>;
  pendingAttachClientRef: MutableRefObject<DirectClient | undefined>;
  activeServerId?: UUID;
  attachedSessionId?: UUID;
  selectedSessionId?: UUID;
  authenticatedClient: (timeoutMs: number) => Promise<DirectClient>;
  attachConnectionTimeoutMs: number;
  reconnectDelaysMs: number[];
  isRetryableConnectionError: (caught: unknown) => boolean;
  isTerminalTransportPaused: () => boolean;
  closeAttachForReconnect: (client?: DirectClient) => boolean;
  discardPendingTerminalOutput: () => void;
  resetAttachReconnectState: () => void;
  setError: (error: SafeError | undefined) => void;
  setStatus: (status: string) => void;
  setSafeError: (caught: unknown) => void;
  setAttachedSessionId: (sessionId: UUID | undefined) => void;
  setSessions: Dispatch<SetStateAction<SessionSummaryPayload[]>>;
  sessionOrderRef: MutableRefObject<UUID[]>;
  sessionPermissionIdsRef: MutableRefObject<Set<UUID>>;
  clearNewOutputMark: (sessionId: UUID) => void;
  clearTerminalOutput: () => number;
  clearTerminalSnapshotRevealHistory: (sessionId?: UUID, snapshotToken?: number) => void;
  waitForTerminalOutputResetApplied: (version: number) => Promise<void>;
  selectSession: (sessionId: UUID | undefined) => void;
  startReceiveLoop: (client: DirectClient) => void;
  loadSessionFiles: (
    sessionId: UUID,
    path?: string,
    options?: { silent?: boolean; source?: "initial" | "manual" | "follow" },
  ) => Promise<void>;
  loadSessionGit: (sessionId: UUID, options?: { silent?: boolean }) => Promise<void>;
  refreshDaemonClients: () => Promise<void>;
  claimAttachClient: (client: DirectClient) => void;
  upsertAttachedSession: (
    current: SessionSummaryPayload[],
    attached: SessionAttachedPayload,
    order: UUID[],
  ) => SessionSummaryPayload[];
}

function isBrowserOffline(): boolean {
  return typeof navigator !== "undefined" && navigator.onLine === false;
}

export function useTerminalReconnectScheduler(
  controller: TerminalAttachController,
  options: UseTerminalReconnectSchedulerOptions,
) {
  const optionsRef = useRef(options);
  useEffect(() => {
    optionsRef.current = options;
  }, [options]);
  const {
    pendingTerminalAttachSessionRef,
    attachedSessionRef,
    userDetachedRef,
    confirmedSessionSizesRef,
    lastRenderedTerminalSeqRef,
    attachReconnectTimerRef,
    attachReconnectKeyRef,
    attachReconnectAttemptsRef,
    attachReconnectLastErrorRef,
    terminalSnapshotTokenSeqRef,
    terminalSnapshotRevealHistoryTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    terminalSnapshotClientFullSnapshotTokensRef,
  } = controller;

  return useCallback((staleClient: DirectClient, caught: unknown, reconnectOptions: AttachReconnectOptions = {}) => {
    const options = optionsRef.current;
    const safeCaught = toSafeError(caught);
    recordTermdDiagnostic("reconnect_requested", {
      code: safeCaught.code,
      message: safeCaught.message,
      reconnectOptions,
      attachedSessionId: attachedSessionRef.current,
      selectedSessionId: options.selectedSessionId,
      userDetached: userDetachedRef.current,
    }, { stack: true });
    if (userDetachedRef.current || !options.isRetryableConnectionError(caught)) {
      recordTermdDiagnostic("reconnect_rejected", {
        code: safeCaught.code,
        userDetached: userDetachedRef.current,
        retryable: options.isRetryableConnectionError(caught),
      });
      return false;
    }
    const sessionId = reconnectOptions.sessionId ?? attachedSessionRef.current ?? options.attachedSessionId ?? options.selectedSessionId;
    if (!sessionId) {
      recordTermdDiagnostic("reconnect_rejected", {
        code: safeCaught.code,
        reason: "missing_session",
      });
      return false;
    }
    const reconnectKey = reconnectOptions.reconnectKey ?? `${options.activeServerId ?? "unknown"}:${sessionId}`;
    const lastTerminalSeq = reconnectOptions.forceFullSnapshot
      ? undefined
      : reconnectOptions.lastTerminalSeq ?? lastRenderedTerminalSeqRef.current.get(sessionId);
    const isFullSnapshot = lastTerminalSeq === undefined;
    if (reconnectOptions.skipCurrentClientClose) {
      // retry catch 已经只清理了本轮重连创建的 pending client；这里按 key 续排，
      // 不能再拿最初的 stale client 去判断“是否属于当前 attach”。
      if (attachReconnectKeyRef.current !== reconnectKey) {
        return true;
      }
      if (isFullSnapshot && reconnectOptions.snapshotToken !== undefined) {
        const currentPendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
        if (currentPendingSnapshot?.token !== reconnectOptions.snapshotToken) {
          return true;
        }
      }
    } else if (!options.closeAttachForReconnect(staleClient)) {
      recordTermdDiagnostic("reconnect_stale_client_closed", {
        reconnectKey,
        sessionId,
      });
      return true;
    }

    let snapshotToken = isFullSnapshot ? reconnectOptions.snapshotToken : undefined;
    if (isFullSnapshot) {
      const pendingFullSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
      const shouldTransferRevealIntent =
        pendingFullSnapshot !== undefined &&
        terminalSnapshotRevealHistoryTokensRef.current.get(sessionId) === pendingFullSnapshot.token;
      if (snapshotToken === undefined && pendingFullSnapshot?.reconnectKey === reconnectKey && !pendingFullSnapshot.claimed) {
        snapshotToken = pendingFullSnapshot.token;
      }
      if (snapshotToken === undefined) {
        terminalSnapshotTokenSeqRef.current += 1;
        snapshotToken = terminalSnapshotTokenSeqRef.current;
      }
      terminalSnapshotPendingFullSnapshotTokensRef.current.set(sessionId, { reconnectKey, token: snapshotToken, claimed: false });
      if (reconnectOptions.revealHistory || shouldTransferRevealIntent) {
        terminalSnapshotRevealHistoryTokensRef.current.set(sessionId, snapshotToken);
      }
    }
    const clearCurrentSnapshotIntent = () => {
      if (isFullSnapshot && snapshotToken !== undefined) {
        options.clearTerminalSnapshotRevealHistory(sessionId, snapshotToken);
      }
    };

    if (attachReconnectKeyRef.current !== reconnectKey) {
      attachReconnectKeyRef.current = reconnectKey;
      attachReconnectAttemptsRef.current = 0;
      attachReconnectLastErrorRef.current = caught;
    } else {
      attachReconnectLastErrorRef.current = caught;
    }

    options.discardPendingTerminalOutput();
    options.setError(undefined);
    recordTermdDiagnostic("reconnect_scheduled", {
      reconnectKey,
      sessionId,
      lastTerminalSeq,
      forceFullSnapshot: lastTerminalSeq === undefined,
      snapshotToken,
      revealHistory: reconnectOptions.revealHistory,
      attempt: attachReconnectAttemptsRef.current,
    });

    if (options.isTerminalTransportPaused()) {
      // 中文注释：offline 期间不主动建新 WebSocket；恢复事件会按当前
      // session 重新进入 handleRetryConnection。hidden/blur 不应暂停 terminal stream。
      clearCurrentSnapshotIntent();
      options.setStatus("ready");
      return true;
    }

    if (attachReconnectTimerRef.current !== undefined) {
      return true;
    }

    if (attachReconnectAttemptsRef.current >= options.reconnectDelaysMs.length) {
      const finalError = attachReconnectLastErrorRef.current ?? caught;
      recordTermdDiagnostic("reconnect_exhausted", {
        reconnectKey,
        sessionId,
        code: toSafeError(finalError).code,
        message: toSafeError(finalError).message,
        snapshotToken,
      });
      clearCurrentSnapshotIntent();
      options.resetAttachReconnectState();
      options.setSafeError(finalError);
      return true;
    }

    const delayMs = options.reconnectDelaysMs[attachReconnectAttemptsRef.current] ?? options.reconnectDelaysMs.at(-1)!;
    attachReconnectAttemptsRef.current += 1;
    options.setStatus("attaching");
    recordTermdDiagnostic("reconnect_timer_set", {
      reconnectKey,
      sessionId,
      delayMs,
      attempt: attachReconnectAttemptsRef.current,
    });
    attachReconnectTimerRef.current = window.setTimeout(() => {
      attachReconnectTimerRef.current = undefined;
      void (async () => {
        let client: DirectClient | undefined;
        try {
          recordTermdDiagnostic("reconnect_timer_fired", {
            reconnectKey,
            sessionId,
            lastTerminalSeq,
            snapshotToken,
          });
          if (options.isTerminalTransportPaused() || isBrowserOffline()) {
            clearCurrentSnapshotIntent();
            options.setStatus("ready");
            return;
          }
          const isCurrentReconnect = () =>
            !userDetachedRef.current && attachReconnectKeyRef.current === reconnectKey;
          const closePendingReconnectClient = () => {
            // 重连计时器可能晚于用户手动切换 session；过期重连只关闭自己创建的连接。
            if (client && options.pendingAttachClientRef.current === client) {
              options.pendingAttachClientRef.current = undefined;
            }
            if (pendingTerminalAttachSessionRef.current === sessionId) {
              pendingTerminalAttachSessionRef.current = undefined;
            }
            client?.close();
            client = undefined;
          };
          client = await options.authenticatedClient(options.attachConnectionTimeoutMs);
          if (!isCurrentReconnect()) {
            clearCurrentSnapshotIntent();
            closePendingReconnectClient();
            return;
          }
          options.pendingAttachClientRef.current = client;
          pendingTerminalAttachSessionRef.current = sessionId;
          const attached = await client.attachSession(
            sessionId,
            {
              ...(lastTerminalSeq !== undefined ? { lastTerminalSeq } : {}),
              timeoutMs: options.attachConnectionTimeoutMs,
            },
          );
          recordTermdDiagnostic("reconnect_attach_ack", {
            reconnectKey,
            sessionId,
            lastTerminalSeq,
            attachedSize: attached.size,
            snapshotToken,
          });
          if (!isCurrentReconnect()) {
            clearCurrentSnapshotIntent();
            client.detachSession(sessionId, "stale_reconnect");
            closePendingReconnectClient();
            return;
          }
          const attachedClient = client;
          client = undefined;
          options.pendingAttachClientRef.current = undefined;
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          // 中文注释：重连拿到 attach ack 后先发布当前 session。
          // reset 期间用户可能已经能在新 Ghostty 里输入；输入不能等 snapshot 开始消费后才生效。
          // 中文注释：reconnect 成功后也要立刻晋升为当前 terminal 主连接，并废弃
          // 所有 metadata sidecar / pending metadata connect，避免迟到 promise 回写。
          options.claimAttachClient(attachedClient);
          if (isFullSnapshot && snapshotToken !== undefined) {
            const pendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(sessionId);
            if (pendingSnapshot?.token === snapshotToken) {
              // 中文注释：token 一旦被某个 attach client claim，后续同 key full resync
              // 不能再复用它；否则旧 client 的 stale cleanup 会误删新一代 reveal intent。
              terminalSnapshotPendingFullSnapshotTokensRef.current.set(sessionId, {
                ...pendingSnapshot,
                claimed: true,
              });
            }
            terminalSnapshotClientFullSnapshotTokensRef.current.set(attachedClient, { sessionId, token: snapshotToken });
          }
          attachedSessionRef.current = sessionId;
          options.sessionPermissionIdsRef.current.add(sessionId);
          confirmedSessionSizesRef.current.set(attached.session_id, attached.size);
          options.selectSession(sessionId);
          options.setAttachedSessionId(sessionId);
          options.setSessions((current) => options.upsertAttachedSession(current, attached, options.sessionOrderRef.current));
          options.clearNewOutputMark(sessionId);
          options.setStatus("attached");
          if (lastTerminalSeq === undefined) {
            // 普通重连会重放完整 snapshot，必须等 TerminalPane 清屏确认后再启动输出消费；
            // 否则旧 Ghostty 的异步回调可能把 snapshot 写进旧实例。
            const resetVersion = options.clearTerminalOutput();
            recordTermdDiagnostic("reconnect_wait_reset_before_snapshot", {
              reconnectKey,
              sessionId,
              resetVersion,
              snapshotToken,
            });
            await options.waitForTerminalOutputResetApplied(resetVersion);
            if (!isCurrentReconnect() || userDetachedRef.current) {
              clearCurrentSnapshotIntent();
              attachedClient.close();
              return;
            }
          }
          if (!isCurrentReconnect() || userDetachedRef.current || options.attachClientRef.current !== attachedClient) {
            clearCurrentSnapshotIntent();
            attachedClient.close();
            return;
          }
          options.resetAttachReconnectState();
          recordTermdDiagnostic("reconnect_start_receive_loop", {
            reconnectKey,
            sessionId,
            lastTerminalSeq,
            snapshotToken,
          });
          options.startReceiveLoop(attachedClient);
          void options.loadSessionFiles(sessionId, undefined, { silent: true, source: "initial" });
          void options.loadSessionGit(sessionId, { silent: true });
          void options.refreshDaemonClients();
        } catch (retryError) {
          if (client && options.pendingAttachClientRef.current === client) {
            options.pendingAttachClientRef.current = undefined;
          }
          if (pendingTerminalAttachSessionRef.current === sessionId) {
            pendingTerminalAttachSessionRef.current = undefined;
          }
          client?.close();
          attachReconnectLastErrorRef.current = retryError;
          recordTermdDiagnostic("reconnect_retry_error", {
            reconnectKey,
            sessionId,
            lastTerminalSeq,
            code: toSafeError(retryError).code,
            message: toSafeError(retryError).message,
            snapshotToken,
          });
          if (!controller.attachReconnectHandlerRef.current(staleClient, retryError, {
            lastTerminalSeq,
            forceFullSnapshot: lastTerminalSeq === undefined,
            revealHistory: reconnectOptions.revealHistory,
            snapshotToken,
            sessionId,
            reconnectKey,
            skipCurrentClientClose: true,
          })) {
            options.resetAttachReconnectState();
            options.setSafeError(retryError);
          }
        }
      })();
    }, delayMs);

    return true;
  }, [
    attachReconnectAttemptsRef,
    attachReconnectKeyRef,
    attachReconnectLastErrorRef,
    attachReconnectTimerRef,
    attachedSessionRef,
    confirmedSessionSizesRef,
    controller.attachReconnectHandlerRef,
    lastRenderedTerminalSeqRef,
    optionsRef,
    pendingTerminalAttachSessionRef,
    terminalSnapshotClientFullSnapshotTokensRef,
    terminalSnapshotPendingFullSnapshotTokensRef,
    terminalSnapshotRevealHistoryTokensRef,
    terminalSnapshotTokenSeqRef,
    userDetachedRef,
  ]);
}
