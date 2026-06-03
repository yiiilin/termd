import { useCallback, useEffect, useRef, useState, type MutableRefObject } from "react";
import type { DirectClient } from "../protocol/direct-client";
import { toSafeError } from "../protocol/errors";
import type {
  SessionFilesResultPayload,
  SessionGitResultPayload,
  SafeError,
  UUID,
} from "../protocol/types";

export interface FileTransferProgressState {
  sessionId: UUID;
  transferId: number;
  name: string;
  offsetBytes: number;
  sizeBytes: number;
  phase?: "sending" | "committing" | "confirmed";
  completed?: boolean;
}

export function useSessionFiles() {
  const [sessionFiles, setSessionFiles] = useState<SessionFilesResultPayload | undefined>();
  const [sessionFilesLoading, setSessionFilesLoading] = useState(false);
  const [sessionFilesError, setSessionFilesError] = useState<SafeError | undefined>();
  const [sessionFilesFollowTerminalCwd, setSessionFilesFollowTerminalCwd] = useState(true);
  const [sessionFileUploadProgress, setSessionFileUploadProgress] = useState<FileTransferProgressState | undefined>();
  const [sessionFileDownloadProgress, setSessionFileDownloadProgress] = useState<FileTransferProgressState | undefined>();
  const [sessionFilesPanelTab, setSessionFilesPanelTab] = useState<"files" | "git">("files");
  const [sessionGit, setSessionGit] = useState<SessionGitResultPayload | undefined>();
  const [sessionGitLoading, setSessionGitLoading] = useState(false);
  const [sessionGitError, setSessionGitError] = useState<SafeError | undefined>();
  const [fileEditor, setFileEditor] = useState<{
    path: string;
    name: string;
    text: string;
    loading: boolean;
    saving: boolean;
    error?: string;
  } | undefined>();
  const [diffViewer, setDiffViewer] = useState<{
    path: string;
    name: string;
    text: string;
    loading: boolean;
    error?: string;
  } | undefined>();

  const sessionFilesFollowTerminalCwdRef = useRef(sessionFilesFollowTerminalCwd);
  const sessionFilesLoadingRef = useRef(sessionFilesLoading);
  const sessionGitLoadingRef = useRef(sessionGitLoading);
  const sessionFileUploadProgressClearTimeoutRef = useRef<number | undefined>(undefined);
  const sessionFileDownloadProgressClearTimeoutRef = useRef<number | undefined>(undefined);
  const fileTransferIdRef = useRef(0);
  const activeUploadTransferIdRef = useRef<number | undefined>(undefined);
  const activeDownloadTransferIdRef = useRef<number | undefined>(undefined);
  const sessionFilesRequestSeqRef = useRef(0);
  const sessionGitRequestSeqRef = useRef(0);
  const sessionFilesFollowRefreshInFlightRef = useRef(false);

  useEffect(() => {
    sessionFilesFollowTerminalCwdRef.current = sessionFilesFollowTerminalCwd;
  }, [sessionFilesFollowTerminalCwd]);

  useEffect(() => {
    sessionFilesLoadingRef.current = sessionFilesLoading;
  }, [sessionFilesLoading]);

  useEffect(() => {
    sessionGitLoadingRef.current = sessionGitLoading;
  }, [sessionGitLoading]);

  const visibleProgressForSession = useCallback(
    (sessionId: UUID | undefined) => ({
      uploadProgress:
        sessionId && sessionFileUploadProgress?.sessionId === sessionId
          ? sessionFileUploadProgress
          : undefined,
      downloadProgress:
        sessionId && sessionFileDownloadProgress?.sessionId === sessionId
          ? sessionFileDownloadProgress
          : undefined,
    }),
    [sessionFileDownloadProgress, sessionFileUploadProgress],
  );

  const nextFileTransferId = useCallback(() => {
    fileTransferIdRef.current += 1;
    return fileTransferIdRef.current;
  }, []);

  const clearSessionFileUploadProgressTimer = useCallback(() => {
    if (sessionFileUploadProgressClearTimeoutRef.current !== undefined) {
      window.clearTimeout(sessionFileUploadProgressClearTimeoutRef.current);
      sessionFileUploadProgressClearTimeoutRef.current = undefined;
    }
  }, []);

  const clearSessionFileDownloadProgressTimer = useCallback(() => {
    if (sessionFileDownloadProgressClearTimeoutRef.current !== undefined) {
      window.clearTimeout(sessionFileDownloadProgressClearTimeoutRef.current);
      sessionFileDownloadProgressClearTimeoutRef.current = undefined;
    }
  }, []);

  const updateUploadProgressForTransfer = useCallback((
    transferId: number,
    sessionId: UUID,
    progress: Omit<FileTransferProgressState, "sessionId" | "transferId">,
  ) => {
    // 中文注释：上传使用独立的会话操作 client；用户切到其他 session 时传输仍可继续。
    // 因此进度只能按 transferId 判定是否过期，不能用当前 attached session 过滤掉。
    if (activeUploadTransferIdRef.current !== transferId) {
      return;
    }
    setSessionFileUploadProgress((current) => {
      if (!current || current.transferId !== transferId) {
        return current;
      }
      const offsetBytes = Math.max(current.offsetBytes, progress.offsetBytes);
      const completed = current.completed || progress.completed;
      // 中文注释：HTTP 上传现在允许 2 并发；sent progress 和 committed progress
      // 可能乱序到达。UI 状态必须只前进，不能被较旧的 non-eof 响应拉回。
      return {
        sessionId,
        transferId,
        ...progress,
        offsetBytes,
        phase: completed ? "confirmed" : progress.phase,
        completed,
      };
    });
  }, []);

  const updateDownloadProgressForTransfer = useCallback((
    transferId: number,
    sessionId: UUID,
    progress: Omit<FileTransferProgressState, "sessionId" | "transferId">,
  ) => {
    // 中文注释：下载同样可能跨 session 切换继续执行，进度条应保持到传输完成或失败。
    if (activeDownloadTransferIdRef.current !== transferId) {
      return;
    }
    setSessionFileDownloadProgress((current) => {
      if (!current || current.transferId !== transferId) {
        return current;
      }
      return { sessionId, transferId, ...progress };
    });
  }, []);

  const scheduleUploadProgressClear = useCallback((transferId: number) => {
    if (activeUploadTransferIdRef.current !== transferId) {
      return;
    }
    clearSessionFileUploadProgressTimer();
    sessionFileUploadProgressClearTimeoutRef.current = window.setTimeout(() => {
      if (activeUploadTransferIdRef.current === transferId) {
        activeUploadTransferIdRef.current = undefined;
        setSessionFileUploadProgress((current) => current?.transferId === transferId ? undefined : current);
      }
      sessionFileUploadProgressClearTimeoutRef.current = undefined;
    }, 1200);
  }, [clearSessionFileUploadProgressTimer]);

  const scheduleDownloadProgressClear = useCallback((transferId: number) => {
    if (activeDownloadTransferIdRef.current !== transferId) {
      return;
    }
    clearSessionFileDownloadProgressTimer();
    sessionFileDownloadProgressClearTimeoutRef.current = window.setTimeout(() => {
      if (activeDownloadTransferIdRef.current === transferId) {
        activeDownloadTransferIdRef.current = undefined;
        setSessionFileDownloadProgress((current) => current?.transferId === transferId ? undefined : current);
      }
      sessionFileDownloadProgressClearTimeoutRef.current = undefined;
    }, 1200);
  }, [clearSessionFileDownloadProgressTimer]);

  const handleSessionFilesFollowTerminalCwdChange = useCallback((follow: boolean) => {
    sessionFilesFollowTerminalCwdRef.current = follow;
    setSessionFilesFollowTerminalCwd(follow);
  }, []);

  const clearFileTransferProgressTimers = useCallback(() => {
    clearSessionFileUploadProgressTimer();
    clearSessionFileDownloadProgressTimer();
  }, [clearSessionFileDownloadProgressTimer, clearSessionFileUploadProgressTimer]);

  const clearSessionFilesState = useCallback(() => {
    sessionFilesRequestSeqRef.current += 1;
    sessionGitRequestSeqRef.current += 1;
    sessionFilesFollowRefreshInFlightRef.current = false;
    setSessionFiles(undefined);
    setSessionFilesError(undefined);
    setSessionFilesLoading(false);
    setSessionGit(undefined);
    setSessionGitError(undefined);
    setSessionGitLoading(false);
    setFileEditor(undefined);
  }, []);

  return {
    sessionFiles,
    setSessionFiles,
    sessionFilesLoading,
    setSessionFilesLoading,
    sessionFilesError,
    setSessionFilesError,
    sessionFilesFollowTerminalCwd,
    setSessionFilesFollowTerminalCwd,
    sessionFileUploadProgress,
    setSessionFileUploadProgress,
    sessionFileDownloadProgress,
    setSessionFileDownloadProgress,
    sessionFilesPanelTab,
    setSessionFilesPanelTab,
    sessionGit,
    setSessionGit,
    sessionGitLoading,
    setSessionGitLoading,
    sessionGitError,
    setSessionGitError,
    fileEditor,
    setFileEditor,
    diffViewer,
    setDiffViewer,
    sessionFilesFollowTerminalCwdRef,
    sessionFilesLoadingRef,
    sessionGitLoadingRef,
    sessionFileUploadProgressClearTimeoutRef,
    sessionFileDownloadProgressClearTimeoutRef,
    fileTransferIdRef,
    activeUploadTransferIdRef,
    activeDownloadTransferIdRef,
    sessionFilesRequestSeqRef,
    sessionGitRequestSeqRef,
    sessionFilesFollowRefreshInFlightRef,
    visibleProgressForSession,
    nextFileTransferId,
    clearSessionFileUploadProgressTimer,
    clearSessionFileDownloadProgressTimer,
    updateUploadProgressForTransfer,
    updateDownloadProgressForTransfer,
    scheduleUploadProgressClear,
    scheduleDownloadProgressClear,
    handleSessionFilesFollowTerminalCwdChange,
    clearFileTransferProgressTimers,
    clearSessionFilesState,
  };
}

export type SessionFilesController = ReturnType<typeof useSessionFiles>;

interface UseSessionFileLoadersOptions {
  authenticatedSessionClient: (sessionId: UUID) => Promise<DirectClient>;
  activeServerId?: UUID;
  activeServerIdRef: MutableRefObject<UUID | undefined>;
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  attachedSessionId?: UUID;
  connectionReady: boolean;
  followPollIntervalMs: number;
}

export function useSessionFileLoaders(
  controller: SessionFilesController,
  options: UseSessionFileLoadersOptions,
) {
  const {
    sessionFilesRequestSeqRef,
    sessionGitRequestSeqRef,
    sessionFilesFollowTerminalCwdRef,
    sessionFilesLoadingRef,
    sessionGitLoadingRef,
    sessionFilesFollowRefreshInFlightRef,
    sessionFilesFollowTerminalCwd,
    sessionFilesLoading,
    setSessionFiles,
    setSessionFilesLoading,
    setSessionFilesError,
    setSessionGit,
    setSessionGitLoading,
    setSessionGitError,
  } = controller;
  const {
    authenticatedSessionClient,
    activeServerId,
    activeServerIdRef,
    attachedSessionRef,
    attachedSessionId,
    connectionReady,
    followPollIntervalMs,
  } = options;

  const loadSessionFiles = useCallback(
    async (
      sessionId: UUID,
      path?: string,
      requestOptions: { silent?: boolean; source?: "initial" | "manual" | "follow" } = {},
    ) => {
      const silent = Boolean(requestOptions.silent);
      const source = requestOptions.source ?? (path === undefined ? "initial" : "manual");
      if (silent && sessionFilesLoadingRef.current) {
        return;
      }
      // 中文注释：silent follow/reconnect refresh 不能推进可见请求序号；
      // 否则手动打开目录的 finally 会被跳过，文件面板可能永远停在 loading。
      const requestSeq = silent ? sessionFilesRequestSeqRef.current : sessionFilesRequestSeqRef.current + 1;
      if (!silent) {
        sessionFilesRequestSeqRef.current = requestSeq;
      }
      if (!silent) {
        setSessionFilesLoading(true);
        setSessionFilesError(undefined);
      }
      try {
        const client = await authenticatedSessionClient(sessionId);
        // 文件树当前位置是 daemon 端 session 共享状态；不传 path 时由 daemon 返回当前共享目录。
        const files = await client.listSessionFiles(sessionId, path);
        const isCurrentRequest = requestSeq === sessionFilesRequestSeqRef.current;
        const allowsFollowResult = source !== "follow" || sessionFilesFollowTerminalCwdRef.current;
        if (!isCurrentRequest || !allowsFollowResult) {
          return;
        }
        setSessionFiles(files);
        setSessionFilesError(undefined);
      } catch (caught) {
        if (!silent && requestSeq === sessionFilesRequestSeqRef.current) {
          // 文件列表是终端旁路信息；失败时只收敛到右侧 panel，不打断已 attach 的终端会话。
          setSessionFiles(undefined);
          setSessionFilesError(toSafeError(caught));
        }
      } finally {
        if (!silent && requestSeq === sessionFilesRequestSeqRef.current) {
          setSessionFilesLoading(false);
        }
      }
    },
    [
      authenticatedSessionClient,
      sessionFilesFollowTerminalCwdRef,
      sessionFilesLoadingRef,
      sessionFilesRequestSeqRef,
      setSessionFiles,
      setSessionFilesError,
      setSessionFilesLoading,
    ],
  );

  const loadSessionGit = useCallback(
    async (sessionId: UUID, requestOptions: { silent?: boolean } = {}) => {
      const silent = Boolean(requestOptions.silent);
      if (silent && sessionGitLoadingRef.current) {
        return;
      }
      const requestServerId = activeServerId;
      // 中文注释：静默 Git refresh 只补后台状态，不能抢占用户刚触发的可见 Git 请求。
      const requestSeq = silent ? sessionGitRequestSeqRef.current : sessionGitRequestSeqRef.current + 1;
      if (!silent) {
        sessionGitRequestSeqRef.current = requestSeq;
      }
      const isCurrentRequest = () =>
        requestSeq === sessionGitRequestSeqRef.current &&
        activeServerIdRef.current === requestServerId &&
        attachedSessionRef.current === sessionId;
      if (!silent) {
        setSessionGitLoading(true);
        setSessionGitError(undefined);
      }
      try {
        const client = await authenticatedSessionClient(sessionId);
        const git = await client.getSessionGit(sessionId);
        if (!isCurrentRequest()) {
          return;
        }
        setSessionGit(git);
        setSessionGitError(undefined);
      } catch (caught) {
        if (!silent && isCurrentRequest()) {
          setSessionGit(undefined);
          setSessionGitError(toSafeError(caught));
        }
      } finally {
        if (!silent && isCurrentRequest()) {
          setSessionGitLoading(false);
        }
      }
    },
    [
      activeServerId,
      activeServerIdRef,
      attachedSessionRef,
      authenticatedSessionClient,
      sessionGitLoadingRef,
      sessionGitRequestSeqRef,
      setSessionGit,
      setSessionGitError,
      setSessionGitLoading,
    ],
  );

  useEffect(() => {
    if (
      !attachedSessionId ||
      !connectionReady ||
      !sessionFilesFollowTerminalCwd ||
      sessionFilesLoading
    ) {
      return undefined;
    }

    const refreshFromTerminalCwd = () => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId || sessionFilesFollowRefreshInFlightRef.current) {
        return;
      }
      sessionFilesFollowRefreshInFlightRef.current = true;
      // 跟随模式必须不传 path；daemon 会按当前 PTY cwd 返回文件树位置。
      void loadSessionFiles(sessionId, undefined, { silent: true, source: "follow" }).finally(() => {
        sessionFilesFollowRefreshInFlightRef.current = false;
      });
    };

    const timer = window.setInterval(refreshFromTerminalCwd, followPollIntervalMs);
    return () => window.clearInterval(timer);
  }, [
    attachedSessionId,
    attachedSessionRef,
    connectionReady,
    followPollIntervalMs,
    loadSessionFiles,
    sessionFilesFollowRefreshInFlightRef,
    sessionFilesFollowTerminalCwd,
    sessionFilesLoading,
  ]);

  return {
    loadSessionFiles,
    loadSessionGit,
  };
}
