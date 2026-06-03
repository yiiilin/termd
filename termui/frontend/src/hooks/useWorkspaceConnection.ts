import { useEffect, useRef, useState, type MutableRefObject } from "react";
import type { DirectClient } from "../protocol/direct-client";
import type { UUID } from "../protocol/types";

export interface WorkspaceAutoRetryStatus {
  phase: "idle" | "scheduled" | "exhausted";
  retryKey?: string;
  attempts: number;
}

export function useWorkspaceConnection() {
  const attachClientRef = useRef<DirectClient | undefined>(undefined);
  const pendingAttachClientRef = useRef<DirectClient | undefined>(undefined);
  const workspaceClientPromiseRef = useRef<Promise<DirectClient> | undefined>(undefined);
  const workspaceClientAbortControllerRef = useRef<AbortController | undefined>(undefined);
  const workspaceClientGenerationRef = useRef(0);
  const connectionAutoRetryTimerRef = useRef<number | undefined>(undefined);
  const connectionAutoRetryKeyRef = useRef<string | undefined>(undefined);
  const connectionAutoRetryAttemptsRef = useRef(0);

  return {
    attachClientRef,
    pendingAttachClientRef,
    workspaceClientPromiseRef,
    workspaceClientAbortControllerRef,
    workspaceClientGenerationRef,
    connectionAutoRetryTimerRef,
    connectionAutoRetryKeyRef,
    connectionAutoRetryAttemptsRef,
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
