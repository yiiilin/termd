import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type MutableRefObject,
  type SetStateAction,
} from "react";
import { ProtocolClientError, toSafeError } from "../protocol/errors";
import {
  readEditableSessionFile,
  type SessionFileEditorClient,
} from "../protocol/session-file-editor-client";
import type { SessionGitDiffClient } from "../protocol/session-git-diff-client";
import type { SessionMutationClient } from "../protocol/session-mutation-client";
import type {
  SessionFileLoadersClient,
} from "../protocol/session-sidecar-client";
import type {
  SessionFileEntryPayload,
  SessionGitActionKind,
  SessionFileWrittenPayload,
  SessionGitDiffResultPayload,
  SessionGitFileChangePayload,
  SessionGitWorktreePayload,
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

export interface FileEditorState {
  path: string;
  name: string;
  text: string;
  loading: boolean;
  saving: boolean;
  error?: string;
}

export interface DiffViewerState {
  path: string;
  name: string;
  text: string;
  loading: boolean;
  error?: string;
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
  const [fileEditor, setFileEditor] = useState<FileEditorState | undefined>();
  const [diffViewer, setDiffViewer] = useState<DiffViewerState | undefined>();

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
  authenticatedSessionClient: (sessionId: UUID) => Promise<SessionFileLoadersClient>;
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

interface UseSessionFilesPanelActionsOptions {
  sessionFilesPath?: string;
  sessionFilesFollowTerminalCwd: boolean;
  setSessionFilesPanelTab: (tab: "files" | "git") => void;
  handleSessionFilesFollowTerminalCwdChange: (follow: boolean) => void;
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  loadSessionFiles: (
    sessionId: UUID,
    path?: string,
    requestOptions?: { silent?: boolean; source?: "initial" | "manual" | "follow" },
  ) => Promise<void>;
  loadSessionGit: (sessionId: UUID, requestOptions?: { silent?: boolean }) => Promise<void>;
  resolveDirectoryPath: (currentDirectory: string, input: string) => string;
}

export function useSessionFilesPanelActions(
  options: UseSessionFilesPanelActionsOptions,
) {
  const {
    sessionFilesFollowTerminalCwd,
    setSessionFilesPanelTab,
    handleSessionFilesFollowTerminalCwdChange,
    sessionFilesPath,
    attachedSessionRef,
    loadSessionFiles,
    loadSessionGit,
    resolveDirectoryPath,
  } = options;

  const handleOpenDirectory = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      // 中文注释：用户开始手动浏览目录时，立即退出自动跟随；
      // 否则下一次 follow 轮询会把当前目录打回终端 cwd。
      handleSessionFilesFollowTerminalCwdChange(false);
      void loadSessionFiles(sessionId, path, { source: "manual" });
    },
    [attachedSessionRef, handleSessionFilesFollowTerminalCwdChange, loadSessionFiles],
  );

  const handleGoToFilePath = useCallback(
    (path: string) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      // 中文注释：手动输入路径也属于显式浏览动作，必须和 follow 模式脱钩。
      handleSessionFilesFollowTerminalCwdChange(false);
      void loadSessionFiles(sessionId, resolveDirectoryPath(sessionFilesPath ?? "", path), { source: "manual" });
    },
    [
      attachedSessionRef,
      handleSessionFilesFollowTerminalCwdChange,
      loadSessionFiles,
      resolveDirectoryPath,
      sessionFilesPath,
    ],
  );

  const handleRefreshSessionFiles = useCallback(() => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    void loadSessionFiles(
      sessionId,
      sessionFilesFollowTerminalCwd ? undefined : sessionFilesPath,
      { source: "manual" },
    );
  }, [
    attachedSessionRef,
    loadSessionFiles,
    sessionFilesPath,
    sessionFilesFollowTerminalCwd,
  ]);

  const handleRefreshSessionGit = useCallback(() => {
    const sessionId = attachedSessionRef.current;
    if (!sessionId) {
      return;
    }
    void loadSessionGit(sessionId);
  }, [attachedSessionRef, loadSessionGit]);

  const handleSessionFilesPanelTabChange = useCallback(
    (tab: "files" | "git") => {
      setSessionFilesPanelTab(tab);
      const sessionId = attachedSessionRef.current;
      if (tab === "git" && sessionId) {
        void loadSessionGit(sessionId);
      }
    },
    [attachedSessionRef, loadSessionGit, setSessionFilesPanelTab],
  );

  return {
    handleOpenDirectory,
    handleGoToFilePath,
    handleRefreshSessionFiles,
    handleRefreshSessionGit,
    handleSessionFilesPanelTabChange,
  };
}

interface UseSessionFileEditorOptions {
  attachedSessionId?: UUID;
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  fileEditor: FileEditorState | undefined;
  setFileEditor: Dispatch<SetStateAction<FileEditorState | undefined>>;
  setSessionFilesError: Dispatch<SetStateAction<SafeError | undefined>>;
  resolveSessionScopedClient: (
    sessionId: UUID,
  ) => Promise<{ client: SessionFileEditorClient; ownsClient: boolean }>;
  refreshVisibleDirectory: (sessionId: UUID) => Promise<void>;
  translateError: (caught: unknown) => string;
  textFileMaxBytes: number;
}

interface OpenRemoteFileInput {
  path: string;
  name: string;
  sizeBytes: number;
}

export function useSessionFileEditor(options: UseSessionFileEditorOptions) {
  const {
    attachedSessionId,
    attachedSessionRef,
    fileEditor,
    setFileEditor,
    setSessionFilesError,
    resolveSessionScopedClient,
    refreshVisibleDirectory,
    translateError,
    textFileMaxBytes,
  } = options;
  const fileOpenRequestSeqRef = useRef(0);
  const activeFileOpenRequestRef = useRef<{
    requestId: number;
    sessionId: UUID;
    path: string;
  } | undefined>(undefined);
  const fileSaveRequestSeqRef = useRef(0);
  const activeFileSaveRequestRef = useRef<{
    requestId: number;
    sessionId: UUID;
    path: string;
  } | undefined>(undefined);

  const resetFileEditor = useCallback(() => {
    fileOpenRequestSeqRef.current += 1;
    activeFileOpenRequestRef.current = undefined;
    fileSaveRequestSeqRef.current += 1;
    activeFileSaveRequestRef.current = undefined;
    setFileEditor(undefined);
  }, [setFileEditor]);

  useEffect(() => {
    // 中文注释：session 切换后，旧 session 的 read/save 都必须失效，
    // 否则迟到响应会把已经切走的 editor 重新写回界面。
    resetFileEditor();
  }, [attachedSessionId, resetFileEditor]);

  const beginFileOpenRequest = useCallback((sessionId: UUID, path: string) => {
    const request = {
      requestId: fileOpenRequestSeqRef.current + 1,
      sessionId,
      path,
    };
    fileOpenRequestSeqRef.current = request.requestId;
    activeFileOpenRequestRef.current = request;
    return request;
  }, []);

  const isActiveFileOpenRequest = useCallback((request: { requestId: number; sessionId: UUID; path: string }) => {
    const active = activeFileOpenRequestRef.current;
    return (
      active?.requestId === request.requestId &&
      active.sessionId === request.sessionId &&
      active.path === request.path &&
      attachedSessionRef.current === request.sessionId
    );
  }, [attachedSessionRef]);

  const beginFileSaveRequest = useCallback((sessionId: UUID, path: string) => {
    const request = {
      requestId: fileSaveRequestSeqRef.current + 1,
      sessionId,
      path,
    };
    fileSaveRequestSeqRef.current = request.requestId;
    activeFileSaveRequestRef.current = request;
    return request;
  }, []);

  const isActiveFileSaveRequest = useCallback((request: { requestId: number; sessionId: UUID; path: string }) => {
    const active = activeFileSaveRequestRef.current;
    return (
      active?.requestId === request.requestId &&
      active.sessionId === request.sessionId &&
      active.path === request.path &&
      attachedSessionRef.current === request.sessionId
    );
  }, [attachedSessionRef]);

  const openRemoteFile = useCallback(
    async ({ path, name, sizeBytes }: OpenRemoteFileInput) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      const request = beginFileOpenRequest(sessionId, path);
      if (sizeBytes > textFileMaxBytes) {
        setFileEditor({
          path,
          name,
          text: "",
          loading: false,
          saving: false,
          error: translateError(new ProtocolClientError("file_too_large", "file is too large to edit in browser")),
        });
        return;
      }

      setSessionFilesError(undefined);
      setFileEditor({
        path,
        name,
        text: "",
        loading: true,
        saving: false,
      });
      let sessionClient: { client: SessionFileEditorClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const payload = await readEditableSessionFile(sessionClient.client, sessionId, path, textFileMaxBytes);
        if (!isActiveFileOpenRequest(request)) {
          return;
        }
        setFileEditor({
          path: payload.path,
          name,
          text: new TextDecoder().decode(payload.bytes),
          loading: false,
          saving: false,
        });
      } catch (caught) {
        if (!isActiveFileOpenRequest(request)) {
          return;
        }
        setFileEditor((current) => ({
          path: current?.path ?? path,
          name: current?.name ?? name,
          text: current?.text ?? "",
          loading: false,
          saving: false,
          error: translateError(caught),
        }));
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [
      attachedSessionRef,
      beginFileOpenRequest,
      isActiveFileOpenRequest,
      resolveSessionScopedClient,
      setFileEditor,
      setSessionFilesError,
      textFileMaxBytes,
      translateError,
    ],
  );

  const handleOpenFile = useCallback(
    async (entry: SessionFileEntryPayload) => {
      if (entry.kind !== "file") {
        return;
      }
      await openRemoteFile({
        path: entry.path,
        name: entry.name,
        sizeBytes: entry.size_bytes,
      });
    },
    [openRemoteFile],
  );

  const handleSaveOpenFile = useCallback(
    async (text: string) => {
      const sessionId = attachedSessionRef.current;
      const editor = fileEditor;
      if (!sessionId || !editor) {
        return;
      }
      const request = beginFileSaveRequest(sessionId, editor.path);
      setFileEditor({ ...editor, text, saving: true, error: undefined });
      let sessionClient: { client: SessionFileEditorClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const written: SessionFileWrittenPayload = await sessionClient.client.writeSessionFile(
          sessionId,
          editor.path,
          new TextEncoder().encode(text),
        );
        if (!isActiveFileSaveRequest(request)) {
          return;
        }
        setFileEditor({
          path: written.path,
          name: editor.name,
          text,
          loading: false,
          saving: false,
        });
        if (attachedSessionRef.current === sessionId) {
          await refreshVisibleDirectory(sessionId);
        }
      } catch (caught) {
        if (!isActiveFileSaveRequest(request)) {
          return;
        }
        setFileEditor({
          ...editor,
          text,
          loading: false,
          saving: false,
          error: translateError(caught),
        });
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [
      attachedSessionRef,
      beginFileSaveRequest,
      fileEditor,
      isActiveFileSaveRequest,
      refreshVisibleDirectory,
      resolveSessionScopedClient,
      setFileEditor,
      translateError,
    ],
  );

  return {
    handleOpenFile,
    handleSaveOpenFile,
    resetFileEditor,
    openRemoteFile,
  };
}

interface UseSessionMutationActionsOptions {
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  sessionFilesPath?: string;
  loadSessionFiles: (
    sessionId: UUID,
    path?: string,
    requestOptions?: { silent?: boolean; source?: "initial" | "manual" | "follow" },
  ) => Promise<void>;
  loadSessionGit: (sessionId: UUID, requestOptions?: { silent?: boolean }) => Promise<void>;
  setSessionGitLoading: Dispatch<SetStateAction<boolean>>;
  setSessionGitError: Dispatch<SetStateAction<SafeError | undefined>>;
  setSessionFilesLoading: Dispatch<SetStateAction<boolean>>;
  setSessionFilesError: Dispatch<SetStateAction<SafeError | undefined>>;
  resolveSessionScopedClient: (
    sessionId: UUID,
  ) => Promise<{ client: SessionMutationClient; ownsClient: boolean }>;
}

export function useSessionMutationActions(options: UseSessionMutationActionsOptions) {
  const {
    attachedSessionRef,
    sessionFilesPath,
    loadSessionFiles,
    loadSessionGit,
    setSessionGitLoading,
    setSessionGitError,
    setSessionFilesLoading,
    setSessionFilesError,
    resolveSessionScopedClient,
  } = options;

  const handleSessionGitAction = useCallback(
    async (
      worktree: SessionGitWorktreePayload,
      change: SessionGitFileChangePayload,
      action: SessionGitActionKind,
    ) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionGitLoading(true);
      setSessionGitError(undefined);
      let sessionClient: { client: SessionMutationClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        await sessionClient.client.applySessionGitAction(sessionId, worktree.path, change.path, action);
        if (attachedSessionRef.current === sessionId) {
          await loadSessionGit(sessionId);
        }
      } catch (caught) {
        if (attachedSessionRef.current === sessionId) {
          setSessionGitError(toSafeError(caught));
          setSessionGitLoading(false);
        }
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [
      attachedSessionRef,
      loadSessionGit,
      resolveSessionScopedClient,
      setSessionGitError,
      setSessionGitLoading,
    ],
  );

  const handleDeleteFile = useCallback(
    async (entry: { path: string }) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      setSessionFilesLoading(true);
      setSessionFilesError(undefined);
      let sessionClient: { client: SessionMutationClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        await sessionClient.client.deleteSessionFile(sessionId, entry.path);
        if (attachedSessionRef.current === sessionId) {
          await loadSessionFiles(sessionId, sessionFilesPath, { source: "manual" });
        }
      } catch (caught) {
        if (attachedSessionRef.current === sessionId) {
          setSessionFilesError(toSafeError(caught));
        }
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
        if (attachedSessionRef.current === sessionId) {
          setSessionFilesLoading(false);
        }
      }
    },
    [
      attachedSessionRef,
      loadSessionFiles,
      resolveSessionScopedClient,
      sessionFilesPath,
      setSessionFilesError,
      setSessionFilesLoading,
    ],
  );

  return {
    handleDeleteFile,
    handleSessionGitAction,
  };
}

interface UseSessionGitDiffViewerOptions {
  attachedSessionId?: UUID;
  attachedSessionRef: MutableRefObject<UUID | undefined>;
  setDiffViewer: Dispatch<SetStateAction<DiffViewerState | undefined>>;
  resolveSessionScopedClient: (
    sessionId: UUID,
  ) => Promise<{ client: SessionGitDiffClient; ownsClient: boolean }>;
  basenamePath: (path: string) => string;
  gitGraphLabel: string;
  translateError: (caught: unknown) => string;
}

export function useSessionGitDiffViewer(options: UseSessionGitDiffViewerOptions) {
  const {
    attachedSessionId,
    attachedSessionRef,
    setDiffViewer,
    resolveSessionScopedClient,
    basenamePath,
    gitGraphLabel,
    translateError,
  } = options;
  const gitDiffOpenRequestSeqRef = useRef(0);
  const activeGitDiffOpenRequestRef = useRef<{
    requestId: number;
    sessionId: UUID;
    worktreePath: string;
    filePath?: string;
    staged: boolean;
  } | undefined>(undefined);

  useEffect(() => {
    // 中文注释：session 切换会让未完成的 diff 打开请求全部过期，
    // 避免旧 session 的慢响应在新 session 上重新弹出 diff。
    gitDiffOpenRequestSeqRef.current += 1;
    activeGitDiffOpenRequestRef.current = undefined;
    setDiffViewer(undefined);
  }, [attachedSessionId, setDiffViewer]);

  const beginGitDiffOpenRequest = useCallback(
    (sessionId: UUID, worktreePath: string, filePath: string | undefined, staged: boolean) => {
      const request = {
        requestId: gitDiffOpenRequestSeqRef.current + 1,
        sessionId,
        worktreePath,
        filePath,
        staged,
      };
      gitDiffOpenRequestSeqRef.current = request.requestId;
      activeGitDiffOpenRequestRef.current = request;
      return request;
    },
    [],
  );

  const isActiveGitDiffOpenRequest = useCallback(
    (request: { requestId: number; sessionId: UUID; worktreePath: string; filePath?: string; staged: boolean }) => {
      const active = activeGitDiffOpenRequestRef.current;
      return (
        active?.requestId === request.requestId &&
        active.sessionId === request.sessionId &&
        active.worktreePath === request.worktreePath &&
        active.filePath === request.filePath &&
        active.staged === request.staged &&
        attachedSessionRef.current === request.sessionId
      );
    },
    [attachedSessionRef],
  );

  const handleCloseGitDiff = useCallback(() => {
    // 中文注释：关闭弹窗就视为旧 diff 已经过期；慢响应不能再次把它打开。
    gitDiffOpenRequestSeqRef.current += 1;
    activeGitDiffOpenRequestRef.current = undefined;
    setDiffViewer(undefined);
  }, [setDiffViewer]);

  const handleOpenGitDiff = useCallback(
    async (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged = false) => {
      const sessionId = attachedSessionRef.current;
      if (!sessionId) {
        return;
      }
      const request = beginGitDiffOpenRequest(sessionId, worktree.path, change?.path, staged);
      const path = change?.path ?? worktree.path;
      setDiffViewer({
        path,
        name: change ? basenamePath(change.path) : gitGraphLabel,
        text: "",
        loading: true,
      });
      let sessionClient: { client: SessionGitDiffClient; ownsClient: boolean } | undefined;
      try {
        sessionClient = await resolveSessionScopedClient(sessionId);
        const diff: SessionGitDiffResultPayload = await sessionClient.client.getSessionGitDiff(
          sessionId,
          worktree.path,
          change?.path,
          staged,
        );
        if (!isActiveGitDiffOpenRequest(request)) {
          return;
        }
        setDiffViewer({
          path: diff.file_path ?? diff.worktree_path,
          name: diff.file_path ? basenamePath(diff.file_path) : gitGraphLabel,
          text: diff.diff || "\n",
          loading: false,
        });
      } catch (caught) {
        if (!isActiveGitDiffOpenRequest(request)) {
          return;
        }
        setDiffViewer((current) => ({
          path: current?.path ?? path,
          name: current?.name ?? path,
          text: current?.text ?? "",
          loading: false,
          error: translateError(caught),
        }));
      } finally {
        if (sessionClient?.ownsClient) {
          sessionClient.client.close();
        }
      }
    },
    [
      attachedSessionRef,
      basenamePath,
      beginGitDiffOpenRequest,
      gitGraphLabel,
      isActiveGitDiffOpenRequest,
      resolveSessionScopedClient,
      setDiffViewer,
      translateError,
    ],
  );

  return {
    handleCloseGitDiff,
    handleOpenGitDiff,
  };
}
