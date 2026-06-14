import { useCallback, useEffect, useRef, type Dispatch, type MutableRefObject, type SetStateAction } from "react";
import type { DirectClient } from "../protocol/direct-client";
import type {
  AttachFramePayload,
  SessionActivityPayload,
  SessionAttachedPayload,
  SessionCwdChangedPayload,
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
import { ProtocolClientError, toSafeError } from "../protocol/errors";
import { recordTermdDiagnostic } from "../diagnostics";
import { decodeSupervisorTerminalServerFrame } from "../protocol/supervisor-terminal";

const RECEIVE_LOOP_YIELD_MESSAGES = 64;
const RECEIVE_LOOP_YIELD_BYTES = 256 * 1024;
const STALE_RECEIVE_LOOP_ABORT = Symbol("stale_receive_loop_abort");

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
  const pendingTerminalAttachAbortControllerRef = useRef<AbortController | undefined>(undefined);
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
    pendingTerminalAttachAbortControllerRef,
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
  handlePassiveSessionFilesResult?: (files: SessionFilesResultPayload) => void;
  loadSessionFiles?: (
    sessionId: UUID,
    path?: string,
    options?: { silent?: boolean; source?: "initial" | "manual" | "follow" },
  ) => Promise<void>;
  requestFollowSessionFilesRefresh?: (sessionId?: UUID) => Promise<void>;
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
    handlePassiveSessionFilesResult,
    loadSessionFiles,
    requestFollowSessionFilesRefresh,
  } = options;
  const {
    attachedSessionRef,
    receiveLoopActiveRef,
    receiveLoopGenerationRef,
    attachReconnectHandlerRef,
    lastRenderedTerminalSeqRef,
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
    const throwIfLoopStale = () => {
      if (!isCurrentLoop()) {
        // 中文注释：disconnect/reconnect 会先推进 generation，再关闭旧 transport。
        // 如果旧 loop 此时正在处理一个大 attach_sync/output batch，中途必须立刻停下，
        // 不能再把旧字节排进下一代 xterm 队列。
        throw STALE_RECEIVE_LOOP_ABORT;
      }
    };
    const enqueueTerminalOutputIfCurrent = (item: TerminalOutputItem) => {
      throwIfLoopStale();
      enqueueTerminalOutput(item);
    };
    const applyConfirmedSessionSizeIfCurrent = (sessionId: UUID, size: TerminalSize) => {
      throwIfLoopStale();
      applyConfirmedSessionSize(sessionId, size);
    };
    const primedSessions = new Set<UUID>();
    const bufferedPreSyncFrames = new Map<UUID, TerminalOutputItem[]>();
    const markSessionPrimed = (sessionId: UUID) => {
      primedSessions.add(sessionId);
    };
    const bufferPreSyncFrame = (sessionId: UUID, item: TerminalOutputItem) => {
      const pending = bufferedPreSyncFrames.get(sessionId) ?? [];
      pending.push(item);
      bufferedPreSyncFrames.set(sessionId, pending);
    };
    const flushBufferedPreSyncFrames = (
      sessionId: UUID,
      options: { seedSequence?: boolean; coveredTerminalSeq?: number } = {},
    ) => {
      const pending = bufferedPreSyncFrames.get(sessionId);
      if (!pending || pending.length === 0) {
        return;
      }
      bufferedPreSyncFrames.delete(sessionId);
      const filteredPending = pending.filter((item) => {
        if (options.coveredTerminalSeq === undefined) {
          return true;
        }
        if (item.kind !== "output" && item.kind !== "resize" && item.kind !== "exit") {
          return true;
        }
        return item.terminalSeq > options.coveredTerminalSeq;
      });
      if (filteredPending.length === 0) {
        return;
      }
      if (options.seedSequence) {
        const firstLiveFrame = filteredPending.find((item) => item.kind === "output" || item.kind === "resize" || item.kind === "exit");
        if (firstLiveFrame && "terminalSeq" in firstLiveFrame) {
          enqueueTerminalOutputIfCurrent({
            kind: "sync",
            baseSeq: Math.max(0, firstLiveFrame.terminalSeq - 1),
          });
        }
      }
      for (const item of filteredPending) {
        enqueueTerminalOutputIfCurrent(item);
      }
    };
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
          if (inner.type === "attach_frame") {
            const payload = inner.payload as AttachFramePayload;
            if (payload.session_id !== attachedSessionRef.current) {
              throwIfLoopStale();
              markNewOutputIfBackground(payload.session_id);
              if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES) {
                processedMessages = 0;
                await yieldToEventLoop();
              }
              continue;
            }
            const frameBytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
            const frame = decodeSupervisorTerminalServerFrame(frameBytes);
            if (frame.type === "attach_sync") {
              const previousRenderedSeq = lastRenderedTerminalSeqRef.current.get(frame.session_id);
              const fullSnapshotToken = terminalSnapshotClientFullSnapshotTokensRef.current.get(client);
              const snapshotToken = fullSnapshotToken?.sessionId === frame.session_id ? fullSnapshotToken.token : undefined;
              const frameSnapshotBaseSeq = frame.frames.reduce<number | undefined>((current, terminalFrame) => {
                if (terminalFrame.kind !== "snapshot") {
                  return current;
                }
                return Math.max(current ?? 0, terminalFrame.base_seq);
              }, undefined);
              const hasFrameSnapshot = frameSnapshotBaseSeq !== undefined;
              const hasSnapshotSeed =
                snapshotToken !== undefined ||
                frame.snapshot.retained_output_bytes.byteLength > 0 ||
                hasFrameSnapshot;
              const tailFramesCoverBaseSeq = (() => {
                if (previousRenderedSeq === undefined || frame.base_seq <= previousRenderedSeq) {
                  return true;
                }
                let expectedSeq = previousRenderedSeq + 1;
                for (const terminalFrame of frame.frames) {
                  if (terminalFrame.kind === "snapshot") {
                    return true;
                  }
                  const terminalSeq = terminalFrame.kind === "output" || terminalFrame.kind === "resize" || terminalFrame.kind === "exit"
                    ? terminalFrame.terminal_seq
                    : undefined;
                  if (terminalSeq === undefined) {
                    continue;
                  }
                  if (terminalSeq < expectedSeq) {
                    continue;
                  }
                  if (terminalSeq !== expectedSeq) {
                    return false;
                  }
                  expectedSeq += 1;
                  if (expectedSeq > frame.base_seq) {
                    return true;
                  }
                }
                return expectedSeq > frame.base_seq;
              })();
              if (
                !hasSnapshotSeed &&
                snapshotToken === undefined &&
                previousRenderedSeq !== undefined &&
                frame.base_seq > previousRenderedSeq &&
                !tailFramesCoverBaseSeq
              ) {
                const gapError = new ProtocolClientError(
                  "terminal_resync",
                  "attach sync advanced beyond the last rendered terminal sequence without a snapshot seed",
                );
                recordTermdDiagnostic("receive_loop_attach_sync_gap_requires_full_snapshot", {
                  sessionId: frame.session_id,
                  previousRenderedSeq,
                  attachBaseSeq: frame.base_seq,
                });
                if (attachReconnectHandlerRef.current(client, gapError, {
                  forceFullSnapshot: true,
                  sessionId: frame.session_id,
                })) {
                  return;
                }
                setSafeError(gapError);
                return;
              }
              markSessionPrimed(frame.session_id);
              applyConfirmedSessionSizeIfCurrent(frame.session_id, frame.snapshot.size);
              const revealToken = terminalSnapshotRevealHistoryTokensRef.current.get(frame.session_id);
              const revealHistory = snapshotToken !== undefined && revealToken === snapshotToken;
              let snapshotTokensConsumed = false;
              let snapshotCoveredSeq: number | undefined = frameSnapshotBaseSeq;
              const enqueueSnapshotFrame = (input: { bytes: Uint8Array; baseSeq: number; size: TerminalSize }) => {
                throwIfLoopStale();
                snapshotCoveredSeq = Math.max(snapshotCoveredSeq ?? 0, input.baseSeq);
                if (!snapshotTokensConsumed) {
                  if (revealHistory) {
                    terminalSnapshotRevealHistoryTokensRef.current.delete(frame.session_id);
                  }
                  if (snapshotToken !== undefined) {
                    const pendingSnapshot = terminalSnapshotPendingFullSnapshotTokensRef.current.get(frame.session_id);
                    if (pendingSnapshot?.token === snapshotToken) {
                      terminalSnapshotPendingFullSnapshotTokensRef.current.delete(frame.session_id);
                    }
                    terminalSnapshotClientFullSnapshotTokensRef.current.delete(client);
                  }
                  snapshotTokensConsumed = true;
                }
                enqueueTerminalOutputIfCurrent({
                  kind: "snapshot",
                  bytes: input.bytes,
                  baseSeq: input.baseSeq,
                  size: input.size,
                  revealHistory,
                });
                processedBytes += input.bytes.byteLength;
              };
              // 中文注释：带 `last_terminal_seq` 的 attach_sync 可能只表示“在现有本地屏幕上续传 tail”，
              // 此时 retained_output 会被刻意置空。不能再把它当成一次空 snapshot reset，
              // 否则重连或复用同一 session 会直接把现有 xterm 画面清掉。
              // 中文注释：新 supervisor 会把首屏放进 frames.snapshot；旧 supervisor 可能同时
              // 带 retained_output 和 frames.snapshot。只要存在权威 snapshot frame，就忽略
              // legacy retained_output，避免同一屏 prompt 被写入两次。
              if (!hasFrameSnapshot && (snapshotToken !== undefined || frame.snapshot.retained_output_bytes.byteLength > 0)) {
                enqueueSnapshotFrame({
                  bytes: frame.snapshot.retained_output_bytes,
                  baseSeq: frame.base_seq,
                  size: frame.snapshot.size,
                });
              }
              let tailOnlySyncSeed: number | undefined;
              if (!hasSnapshotSeed) {
                for (const terminalFrame of frame.frames) {
                  if (terminalFrame.kind === "output" || terminalFrame.kind === "resize" || terminalFrame.kind === "exit") {
                    tailOnlySyncSeed = Math.max(0, terminalFrame.terminal_seq - 1);
                    break;
                  }
                }
                // 中文注释：tail-only attach_sync 的 base_seq 表示“同步响应覆盖到的最新序号”，
                // 不是 writer 下一帧前的游标。若首条 tail 本身就是 base_seq，必须播种到
                // 首帧前一位；只有空 attach_sync 才推进到 base_seq，让 gap 检测升级 full snapshot。
                enqueueTerminalOutputIfCurrent({
                  kind: "sync",
                  baseSeq: tailOnlySyncSeed ?? frame.base_seq,
                });
              }
              for (const terminalFrame of frame.frames) {
                if (terminalFrame.kind === "snapshot") {
                  applyConfirmedSessionSizeIfCurrent(frame.session_id, terminalFrame.size);
                  const bytes = terminalFrame.data_bytes ?? sessionDataFromBase64(terminalFrame.data_base64 ?? "");
                  enqueueSnapshotFrame({
                    bytes,
                    baseSeq: terminalFrame.base_seq,
                    size: terminalFrame.size,
                  });
                } else if (terminalFrame.kind === "output") {
                  if (snapshotCoveredSeq !== undefined && terminalFrame.terminal_seq <= snapshotCoveredSeq) {
                    continue;
                  }
                  const bytes = terminalFrame.data_bytes ?? sessionDataFromBase64(terminalFrame.data_base64 ?? "");
                  enqueueTerminalOutputIfCurrent({
                    kind: "output",
                    bytes,
                    terminalSeq: terminalFrame.terminal_seq,
                  });
                  processedBytes += bytes.byteLength;
                } else if (terminalFrame.kind === "resize") {
                  if (snapshotCoveredSeq !== undefined && terminalFrame.terminal_seq <= snapshotCoveredSeq) {
                    continue;
                  }
                  enqueueTerminalOutputIfCurrent({
                    kind: "resize",
                    terminalSeq: terminalFrame.terminal_seq,
                    size: terminalFrame.size,
                  });
                } else if (terminalFrame.kind === "exit") {
                  if (snapshotCoveredSeq !== undefined && terminalFrame.terminal_seq <= snapshotCoveredSeq) {
                    continue;
                  }
                  enqueueTerminalOutputIfCurrent({
                    kind: "exit",
                    terminalSeq: terminalFrame.terminal_seq,
                  });
                }
              }
              flushBufferedPreSyncFrames(frame.session_id, {
                seedSequence: !hasSnapshotSeed,
                coveredTerminalSeq: snapshotCoveredSeq,
              });
            } else if (frame.type === "terminal_frame") {
              if (frame.frame.kind === "snapshot") {
                markSessionPrimed(frame.session_id);
                applyConfirmedSessionSizeIfCurrent(frame.session_id, frame.frame.size);
                const bytes = frame.frame.data_bytes ?? sessionDataFromBase64(frame.frame.data_base64 ?? "");
                enqueueTerminalOutputIfCurrent({
                  kind: "snapshot",
                  bytes,
                  baseSeq: frame.frame.base_seq,
                  size: frame.frame.size,
                  revealHistory: false,
                });
                processedBytes += bytes.byteLength;
              } else if (frame.frame.kind === "output") {
                const bytes = frame.frame.data_bytes ?? sessionDataFromBase64(frame.frame.data_base64 ?? "");
                const outputItem: TerminalOutputItem = {
                  kind: "output",
                  bytes,
                  terminalSeq: frame.frame.terminal_seq,
                };
                if (
                  !primedSessions.has(frame.session_id) &&
                  lastRenderedTerminalSeqRef.current.get(frame.session_id) === undefined
                ) {
                  bufferPreSyncFrame(frame.session_id, outputItem);
                } else {
                  enqueueTerminalOutputIfCurrent(outputItem);
                  processedBytes += bytes.byteLength;
                }
              } else if (frame.frame.kind === "resize") {
                const resizeItem: TerminalOutputItem = {
                  kind: "resize",
                  terminalSeq: frame.frame.terminal_seq,
                  size: frame.frame.size,
                };
                if (
                  !primedSessions.has(frame.session_id) &&
                  lastRenderedTerminalSeqRef.current.get(frame.session_id) === undefined
                ) {
                  bufferPreSyncFrame(frame.session_id, resizeItem);
                } else {
                  enqueueTerminalOutputIfCurrent(resizeItem);
                }
              } else if (frame.frame.kind === "exit") {
                const exitItem: TerminalOutputItem = {
                  kind: "exit",
                  terminalSeq: frame.frame.terminal_seq,
                };
                if (
                  !primedSessions.has(frame.session_id) &&
                  lastRenderedTerminalSeqRef.current.get(frame.session_id) === undefined
                ) {
                  bufferPreSyncFrame(frame.session_id, exitItem);
                } else {
                  enqueueTerminalOutputIfCurrent(exitItem);
                }
              }
            } else if (frame.type === "heartbeat_ping") {
              throwIfLoopStale();
              client.sendSupervisorTerminalHeartbeatPong(payload.session_id, frame.nonce);
            } else if (frame.type === "close") {
              throw new ProtocolClientError(frame.reason, frame.message ?? frame.reason);
            }
          } else if (inner.type === "session_activity") {
            const payload = inner.payload as SessionActivityPayload;
            throwIfLoopStale();
            markNewOutputIfBackground(payload.session_id);
          } else if (inner.type === "session_cwd_changed") {
            const payload = inner.payload as SessionCwdChangedPayload;
            throwIfLoopStale();
            if (
              payload.session_id === attachedSessionRef.current &&
              sessionFilesFollowTerminalCwdRef.current
            ) {
              if (requestFollowSessionFilesRefresh) {
                void requestFollowSessionFilesRefresh(payload.session_id);
              } else {
                void loadSessionFiles?.(payload.session_id, undefined, {
                  silent: true,
                  source: "follow",
                });
              }
            }
          } else if (inner.type === "session_files_result") {
            const payload = inner.payload as SessionFilesResultPayload;
            throwIfLoopStale();
            // 中文注释：现在 cwd 变化只推轻事件，文件树由显式 `session.files` 请求回写。
            // 这里保留兼容旧 daemon 的被动结果接收路径，避免 mixed-version 场景闪退。
            if (
              payload.session_id === attachedSessionRef.current &&
              sessionFilesFollowTerminalCwdRef.current
            ) {
              handlePassiveSessionFilesResult?.(payload);
            }
          } else if (inner.type === "session_git_result") {
            const payload = inner.payload as SessionGitResultPayload;
            throwIfLoopStale();
            if (payload.session_id === attachedSessionRef.current) {
              setSessionGit(payload);
              setSessionGitError(undefined);
              setSessionGitLoading(false);
            }
          } else if (inner.type === "session_resized") {
            const payload = inner.payload as SessionResizedPayload;
            applyConfirmedSessionSizeIfCurrent(payload.session_id, payload.size);
          }
          if (processedMessages >= RECEIVE_LOOP_YIELD_MESSAGES || processedBytes >= RECEIVE_LOOP_YIELD_BYTES) {
            processedMessages = 0;
            processedBytes = 0;
            await yieldToEventLoop();
          }
        } catch (caught) {
          if (caught === STALE_RECEIVE_LOOP_ABORT) {
            return;
          }
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
    handlePassiveSessionFilesResult,
    loadSessionFiles,
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
  authenticatedClient: (timeoutMs: number, signal?: AbortSignal) => Promise<DirectClient>;
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
  sessionFilesAutoRefreshPath?: () => string | undefined;
  loadSessionGit: (sessionId: UUID, options?: { silent?: boolean }) => Promise<void>;
  refreshDaemonClients: () => Promise<void>;
  claimAttachClient: (client: DirectClient) => void;
  onAttachTransportReady?: (client: DirectClient, sessionId: UUID) => Promise<void> | void;
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
    pendingTerminalAttachAbortControllerRef,
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
        let attachAbortController: AbortController | undefined;
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
            if (
              attachAbortController &&
              pendingTerminalAttachAbortControllerRef.current === attachAbortController
            ) {
              pendingTerminalAttachAbortControllerRef.current = undefined;
            }
            client?.close();
            client = undefined;
          };
          attachAbortController = new AbortController();
          pendingTerminalAttachAbortControllerRef.current = attachAbortController;
          client = await options.authenticatedClient(
            options.attachConnectionTimeoutMs,
            attachAbortController.signal,
          );
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
              signal: attachAbortController.signal,
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
          if (
            attachAbortController &&
            pendingTerminalAttachAbortControllerRef.current === attachAbortController
          ) {
            pendingTerminalAttachAbortControllerRef.current = undefined;
          }
          // 中文注释：重连拿到 attach ack 后先发布当前 session。
          // reset 期间用户可能已经能在新 xterm 里输入；输入不能等 snapshot 开始消费后才生效。
          // 中文注释：reconnect 成功后也要立刻晋升为当前 terminal 主连接，并废弃
          // 所有 metadata sidecar / pending metadata connect，避免迟到 promise 回写。
          options.claimAttachClient(attachedClient);
          await options.onAttachTransportReady?.(attachedClient, sessionId);
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
            // 否则旧 xterm 的异步回调可能把 snapshot 写进旧实例。
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
          void options.loadSessionFiles(sessionId, options.sessionFilesAutoRefreshPath?.(), { silent: true, source: "initial" });
          void options.loadSessionGit(sessionId, { silent: true });
          void options.refreshDaemonClients();
        } catch (retryError) {
          if (
            attachAbortController &&
            pendingTerminalAttachAbortControllerRef.current === attachAbortController
          ) {
            pendingTerminalAttachAbortControllerRef.current = undefined;
          }
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
