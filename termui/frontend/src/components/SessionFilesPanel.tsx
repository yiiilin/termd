import { useEffect, useRef, useState } from "react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import {
  ArrowUp,
  ChevronDown,
  ChevronRight,
  Download,
  File,
  FilePenLine,
  Folder,
  GitBranch,
  Link2,
  Minus,
  PanelRightClose,
  Plus,
  RefreshCw,
  Trash2,
  Undo2,
  Upload,
} from "lucide-react";
import type {
  SafeError,
  SessionFileEntryPayload,
  SessionFilesResultPayload,
  SessionGitActionKind,
  SessionGitFileChangePayload,
  SessionGitResultPayload,
  SessionGitWorktreePayload,
  UUID,
} from "../protocol/types";
import { useI18n, type Translate } from "../i18n";

const GIT_SPLIT_MIN_PANE_HEIGHT = 24;
const GIT_SPLIT_FALLBACK_PANEL_HEIGHT = 360;

interface SessionFilesPanelProps {
  attachedSessionId?: UUID;
  activeTab: "files" | "git";
  files?: SessionFilesResultPayload;
  loading: boolean;
  error?: SafeError;
  uploadProgress?: {
    name: string;
    offsetBytes: number;
    sizeBytes: number;
    phase?: "sending" | "committing" | "confirmed";
    completed?: boolean;
  };
  downloadProgress?: {
    name: string;
    offsetBytes: number;
    sizeBytes: number;
    completed?: boolean;
  };
  git?: SessionGitResultPayload;
  gitLoading: boolean;
  gitError?: SafeError;
  followTerminalCwd: boolean;
  onTabChange: (tab: "files" | "git") => void;
  onOpenDirectory: (path: string) => void;
  onOpenFile: (entry: SessionFileEntryPayload) => void;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onOpenGitDiff: (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged?: boolean) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
  onGoToPath: (path: string) => void;
  onRefresh: () => void;
  onRefreshGit: () => void;
  onFollowTerminalCwdChange: (follow: boolean) => void;
  onUpload: (file: globalThis.File) => void;
  onDownload: (entry: SessionFileEntryPayload) => void;
  onDelete: (entry: SessionFileEntryPayload) => void;
  onHide: () => void;
  onResizePointerDown?: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onResizeKeyDown?: (event: ReactKeyboardEvent<HTMLDivElement>) => void;
}

export function SessionFilesPanel({
  attachedSessionId,
  activeTab,
  files,
  loading,
  error,
  uploadProgress,
  downloadProgress,
  git,
  gitLoading,
  gitError,
  followTerminalCwd,
  onTabChange,
  onOpenDirectory,
  onOpenFile,
  onOpenGitFile,
  onOpenGitDiff,
  onGitAction,
  onGoToPath,
  onRefresh,
  onRefreshGit,
  onFollowTerminalCwdChange,
  onUpload,
  onDownload,
  onDelete,
  onHide,
  onResizePointerDown,
  onResizeKeyDown,
}: SessionFilesPanelProps) {
  const { t } = useI18n();
  const entries = files?.entries ?? [];
  const currentPath = files?.path ?? "";
  const hasCachedEntries = entries.length > 0;
  const transferProgress = uploadProgress
    ? { ...uploadProgress, label: t(uploadProgress.phase === "committing" ? "files.uploadCommitting" : "files.uploadProgress", { name: uploadProgress.name }) }
    : downloadProgress
      ? { ...downloadProgress, label: t("files.downloadProgress", { name: downloadProgress.name }) }
      : undefined;
  const [pathDraft, setPathDraft] = useState(currentPath);
  const [pathDraftDirty, setPathDraftDirty] = useState(false);
  const submittedPathDraftRef = useRef<string | undefined>(undefined);
  const uploadRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (!pathDraftDirty || submittedPathDraftRef.current === currentPath) {
      setPathDraft(currentPath);
      setPathDraftDirty(false);
      submittedPathDraftRef.current = undefined;
    }
  }, [currentPath, pathDraftDirty]);

  useEffect(() => {
    // 切换 session 时草稿必须跟随新的文件树根，避免把上一个 session 的手输路径带过去。
    setPathDraft(currentPath);
    setPathDraftDirty(false);
    submittedPathDraftRef.current = undefined;
  }, [attachedSessionId]);

  const submitPathDraft = () => {
    submittedPathDraftRef.current = pathDraft;
    onGoToPath(pathDraft);
  };

  const goToParentPath = () => {
    const nextPath = parentPath(currentPath);
    submittedPathDraftRef.current = nextPath;
    setPathDraft(nextPath);
    setPathDraftDirty(true);
    onGoToPath(nextPath);
  };

  return (
    <aside className="files-panel" aria-label={t("files.panelAria")}>
      {onResizePointerDown ? (
        <div
          className="files-panel-edge-resizer"
          role="separator"
          aria-label={t("files.resizePanel")}
          aria-orientation="vertical"
          tabIndex={0}
          onPointerDown={onResizePointerDown}
          onKeyDown={onResizeKeyDown}
        />
      ) : null}
      <header className="files-panel-header">
        <div className="files-tabs" role="tablist" aria-label={t("files.panelView")}>
          <button
            type="button"
            className="files-tab"
            role="tab"
            aria-selected={activeTab === "files"}
            onClick={() => onTabChange("files")}
          >
            <Folder size={14} aria-hidden="true" />
            <span>{t("app.files")}</span>
          </button>
          <button
            type="button"
            className="files-tab"
            role="tab"
            aria-selected={activeTab === "git"}
            onClick={() => onTabChange("git")}
          >
            <GitBranch size={14} aria-hidden="true" />
            <span>{t("app.git")}</span>
          </button>
        </div>
        <button type="button" className="icon-button files-hide-button" aria-label={t("files.hidePanel")} onClick={onHide}>
          <PanelRightClose size={16} aria-hidden="true" />
        </button>
      </header>
      {activeTab === "files" ? (
        <div className="files-tab-body" role="tabpanel" aria-label={t("app.files")}>
          <div className="files-toolbar">
            <button
              type="button"
              className="icon-button"
              aria-label={t("files.parentDirectory")}
              disabled={!attachedSessionId || loading || !currentPath}
              onClick={goToParentPath}
            >
              <ArrowUp size={15} aria-hidden="true" />
            </button>
            <label className="files-path-field">
              <span className="sr-only">{t("files.currentDirectory")}</span>
              <input
                aria-label={t("files.currentDirectory")}
                value={pathDraft}
                disabled={!attachedSessionId || loading}
                onChange={(event) => {
                  submittedPathDraftRef.current = undefined;
                  setPathDraftDirty(true);
                  setPathDraft(event.target.value);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    submitPathDraft();
                  }
                }}
              />
            </label>
            <button
              type="button"
              className="files-go-button"
              disabled={!attachedSessionId || loading}
              onClick={submitPathDraft}
            >
              {t("files.go")}
            </button>
            <button
              type="button"
              className="icon-button"
              aria-label={t("files.refresh")}
              disabled={!attachedSessionId || loading}
              onClick={onRefresh}
            >
              <RefreshCw size={15} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-button"
              aria-label={t("files.upload")}
              disabled={!attachedSessionId || loading}
              onClick={() => uploadRef.current?.click()}
            >
              <Upload size={15} aria-hidden="true" />
            </button>
            <input
              ref={uploadRef}
              className="files-upload-input"
              aria-label={t("files.uploadFile")}
              type="file"
              onChange={(event) => {
                const file = event.target.files?.[0];
                event.currentTarget.value = "";
                if (file) {
                  onUpload(file);
                }
              }}
            />
          </div>
          <div className="files-list">
            {!attachedSessionId ? <div className="files-empty">{t("files.detached")}</div> : null}
            {attachedSessionId && loading && !hasCachedEntries ? (
              <div className="files-empty">
                <RefreshCw size={14} aria-hidden="true" />
                {t("files.loading")}
              </div>
            ) : null}
            {attachedSessionId && !loading && error ? <div className="files-empty">{t("files.unavailable")}</div> : null}
            {attachedSessionId && !loading && !error && entries.length === 0 ? (
              <div className="files-empty">{t("files.emptyDirectory")}</div>
            ) : null}
            {/*
              刷新目录或保存文件时保留旧列表，避免按钮在短暂 loading 期间消失；
              daemon 返回新目录后会用新的 session_files_result 覆盖这里的缓存。
            */}
            {attachedSessionId && !error && hasCachedEntries
              ? entries.map((entry) => (
                  <SessionFileRow
                    key={entry.path}
                    entry={entry}
                    onOpenDirectory={onOpenDirectory}
                    onOpenFile={onOpenFile}
                    onDownload={onDownload}
                    onDelete={onDelete}
                    t={t}
                  />
                ))
              : null}
          </div>
          {transferProgress ? (
            <div
              className="files-transfer-progress"
              role="status"
              aria-label={transferProgress.label}
            >
              <div className="files-transfer-name" title={transferProgress.name}>
                {transferProgress.name}
              </div>
              <div className="files-transfer-bar" aria-hidden="true">
                <div
                  className="files-transfer-bar-fill"
                  style={{ "--files-transfer-progress": `${uploadProgressPercent(transferProgress)}%` } as CSSProperties}
                />
              </div>
            </div>
          ) : null}
          <footer className="files-follow-footer">
            <label className="files-follow-toggle">
              <input
                type="checkbox"
                checked={followTerminalCwd}
                disabled={!attachedSessionId}
                onChange={(event) => onFollowTerminalCwdChange(event.currentTarget.checked)}
              />
              <span>{t("files.followCwd")}</span>
            </label>
          </footer>
        </div>
      ) : (
        <GitPanel
          attachedSessionId={attachedSessionId}
          git={git}
          loading={gitLoading}
          error={gitError}
          onRefresh={onRefreshGit}
          onOpenGitFile={onOpenGitFile}
          onOpenGitDiff={onOpenGitDiff}
          onGitAction={onGitAction}
        />
      )}
    </aside>
  );
}

function uploadProgressPercent(progress: { offsetBytes: number; sizeBytes: number; completed?: boolean }): number {
  if (progress.sizeBytes <= 0) {
    return progress.completed ? 100 : 0;
  }
  const percent = Math.max(0, Math.min(100, (progress.offsetBytes / progress.sizeBytes) * 100));
  return progress.completed ? percent : Math.min(99, percent);
}

function GitPanel({
  attachedSessionId,
  git,
  loading,
  error,
  onRefresh,
  onOpenGitFile,
  onOpenGitDiff,
  onGitAction,
}: {
  attachedSessionId?: UUID;
  git?: SessionGitResultPayload;
  loading: boolean;
  error?: SafeError;
  onRefresh: () => void;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onOpenGitDiff: (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged?: boolean) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  const { t } = useI18n();
  const worktrees = git?.worktrees ?? [];
  const graph = git?.graph ?? [];
  const currentWorktree = worktrees.find((worktree) => worktree.is_current) ?? worktrees[0];
  const repositoryPath = git?.repository_root ?? git?.cwd ?? "";
  const repositoryName = repositoryPath ? lastPathSegment(repositoryPath) : t("git.repository");
  const branchLabel = currentWorktree?.branch ?? currentWorktree?.head ?? t("git.detached");
  const stagedCount = worktrees.reduce((count, worktree) => count + worktree.staged.length, 0);
  const unstagedCount = worktrees.reduce((count, worktree) => count + worktree.unstaged.length, 0);
  const commitCount = graph.filter((line) => line.includes("*")).length;
  const hasChanges = stagedCount + unstagedCount > 0;
  const hasError = Boolean(error || git?.error);
  const statusTone = !attachedSessionId || hasError ? "muted" : loading && !git ? "loading" : hasChanges ? "dirty" : "clean";
  const statusLabel = !attachedSessionId
    ? t("files.detached")
    : hasError
      ? t("files.unavailable")
      : loading && !git
        ? t("files.loading")
        : hasChanges
          ? t("git.dirty")
          : t("git.clean");
  const [filesCollapsed, setFilesCollapsed] = useState(false);
  const [graphCollapsed, setGraphCollapsed] = useState(false);
  const [changesPaneHeight, setChangesPaneHeight] = useState<number | undefined>();
  const [graphResizing, setGraphResizing] = useState(false);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const statusPaneRef = useRef<HTMLElement | null>(null);
  const graphResizeRef = useRef<{
    pointerId: number;
    startY: number;
    startHeight: number;
  } | null>(null);

  const applyGraphSplitHeight = (height: number) => {
    setChangesPaneHeight(clampGitSplitHeight(height, graphPanelHeight(panelRef.current)));
  };

  const handleGraphResizePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (filesCollapsed || graphCollapsed) {
      return;
    }
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    graphResizeRef.current = {
      pointerId: event.pointerId,
      startY: event.clientY,
      startHeight: changesPaneHeight ?? statusPaneHeight(statusPaneRef.current),
    };
    setGraphResizing(true);
  };

  const handleGraphResizePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const drag = graphResizeRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    applyGraphSplitHeight(drag.startHeight + event.clientY - drag.startY);
  };

  const finishGraphResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    const drag = graphResizeRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    graphResizeRef.current = null;
    event.currentTarget.releasePointerCapture?.(event.pointerId);
    setGraphResizing(false);
  };

  const handleGraphResizeKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const current = changesPaneHeight ?? statusPaneHeight(statusPaneRef.current);
    const panelHeight = graphPanelHeight(panelRef.current);
    if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      applyGraphSplitHeight(current + (event.key === "ArrowDown" ? 24 : -24));
    } else if (event.key === "Home") {
      event.preventDefault();
      setChangesPaneHeight(GIT_SPLIT_MIN_PANE_HEIGHT);
    } else if (event.key === "End") {
      event.preventDefault();
      setChangesPaneHeight(clampGitSplitHeight(panelHeight, panelHeight));
    }
  };

  const splitActive = changesPaneHeight !== undefined && !filesCollapsed && !graphCollapsed;

  return (
    <div
      ref={panelRef}
      className={`git-panel git-panel-compact${splitActive ? " git-panel-split-overridden" : ""}${graphResizing ? " git-graph-resizing" : ""}${filesCollapsed ? " git-files-collapsed" : ""}${graphCollapsed ? " git-graph-collapsed" : ""}`}
      role="tabpanel"
      aria-label={t("app.git")}
      style={splitActive ? ({ "--git-changes-pane-height": `${changesPaneHeight}px` } as CSSProperties) : undefined}
    >
      <header className="git-overview" aria-label={t("git.workspace")}>
        <div className="git-overview-topline">
          <div className="git-overview-identity">
            <span className="git-overview-kicker">{t("git.workspace")}</span>
            <strong className="git-overview-repository" title={repositoryPath}>
              {repositoryName}
            </strong>
            <span className="git-overview-path" title={repositoryPath}>
              {repositoryPath || t("git.repository")}
            </span>
          </div>
          <div className={`git-overview-status git-overview-status-${statusTone}`} role="status">
            <span className="git-status-dot" aria-hidden="true" />
            <span>{statusLabel}</span>
          </div>
        </div>
        <div className="git-overview-bottomline">
          <div className="git-overview-branch" title={branchLabel}>
            <GitBranch size={14} aria-hidden="true" />
            <span>{currentWorktree?.head ? t("git.head") : branchLabel}</span>
            {currentWorktree?.head ? <code title={currentWorktree.head}>{currentWorktree.head}</code> : null}
          </div>
          <div className="git-overview-metrics" aria-label={t("git.summary")}>
            <GitMetric label={t("git.staged")} count={stagedCount} tone="staged" />
            <GitMetric label={t("git.unstaged")} count={unstagedCount} tone="unstaged" />
            <GitMetric label={t("git.commits")} count={commitCount} tone="commits" />
          </div>
          <button
            type="button"
            className="icon-button git-overview-refresh"
            aria-label={t("git.refresh")}
            disabled={!attachedSessionId || loading}
            onClick={onRefresh}
          >
            <RefreshCw size={15} aria-hidden="true" />
          </button>
        </div>
      </header>
      <section ref={statusPaneRef} className="git-status-pane" aria-label={t("git.status")}>
        <header className="git-section-header">
          <button
            type="button"
            className="git-section-toggle"
            aria-expanded={!filesCollapsed}
            aria-label={filesCollapsed ? t("git.expandChanges") : t("git.collapseChanges")}
            onClick={() => setFilesCollapsed((collapsed) => !collapsed)}
          >
            {filesCollapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
            <span>{t("git.changes")}</span>
          </button>
          <span className="git-section-count">{stagedCount + unstagedCount}</span>
        </header>
        {!filesCollapsed ? (
          <div className="git-section-body git-status-body" role="tree" aria-label={t("git.changesTree")}>
            {!attachedSessionId ? <div className="files-empty">{t("files.detached")}</div> : null}
            {attachedSessionId && loading && !git ? (
              <div className="files-empty">
                <RefreshCw size={14} aria-hidden="true" />
                {t("files.loading")}
              </div>
            ) : null}
            {attachedSessionId && !loading && error ? <div className="files-empty">{t("files.unavailable")}</div> : null}
            {attachedSessionId && !error && git?.error ? <div className="files-empty">{git.error}</div> : null}
            {attachedSessionId && !error && git && !git.error && worktrees.length === 0 ? (
              <div className="files-empty">{t("git.cleanRepository")}</div>
            ) : null}
            {attachedSessionId && !error && !git?.error
              ? worktrees.map((worktree) => (
                  <GitWorktree
                    key={worktree.path}
                    worktree={worktree}
                    onOpenGitFile={onOpenGitFile}
                    onOpenGitDiff={onOpenGitDiff}
                    onGitAction={onGitAction}
                  />
                ))
              : null}
          </div>
        ) : null}
      </section>
      {!filesCollapsed && !graphCollapsed ? (
        <div
          className="git-graph-resizer"
          role="separator"
          aria-label={t("git.resizeGraph")}
          aria-orientation="horizontal"
          tabIndex={0}
          onPointerDown={handleGraphResizePointerDown}
          onPointerMove={handleGraphResizePointerMove}
          onPointerUp={finishGraphResize}
          onPointerCancel={finishGraphResize}
          onKeyDown={handleGraphResizeKeyDown}
        />
      ) : null}
      <section className="git-graph-pane" aria-label={t("git.graph")}>
        <header className="git-section-header">
          <button
            type="button"
            className="git-section-toggle"
            aria-expanded={!graphCollapsed}
            aria-label={graphCollapsed ? t("git.expandGraph") : t("git.collapseGraph")}
            onClick={() => setGraphCollapsed((collapsed) => !collapsed)}
          >
            {graphCollapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
            <span>{t("git.graph")}</span>
          </button>
          <span className="git-section-count">{commitCount}</span>
        </header>
        {!graphCollapsed ? (
          graph.length > 0 ? <GitGraph lines={graph} /> : <div className="files-empty">{t("git.noCommits")}</div>
        ) : null}
      </section>
    </div>
  );
}

function GitMetric({ label, count, tone }: { label: string; count: number; tone: "staged" | "unstaged" | "commits" }) {
  return (
    <span className={`git-metric git-metric-${tone}`}>
      <strong>{count}</strong>
      <span>{label}</span>
    </span>
  );
}

function GitWorktree({
  worktree,
  onOpenGitFile,
  onOpenGitDiff,
  onGitAction,
}: {
  worktree: SessionGitWorktreePayload;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onOpenGitDiff: (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged?: boolean) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  const { t } = useI18n();
  const label = worktree.branch ?? worktree.head ?? t("git.detached");
  const [collapsed, setCollapsed] = useState(false);

  return (
    <article className="git-worktree" role="treeitem" aria-label={t("git.worktreeChanges", { label })} aria-expanded={!collapsed}>
      <header className="git-worktree-header">
        <button
          type="button"
          className="git-worktree-toggle"
          aria-expanded={!collapsed}
          aria-label={collapsed ? t("git.expandWorktree", { label }) : t("git.collapseWorktree", { label })}
          onClick={() => setCollapsed((current) => !current)}
          title={label}
        >
          {collapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
          <span className="git-worktree-branch" title={label}>
            {label}
          </span>
        </button>
        <span className="git-worktree-floating-meta git-worktree-meta">
          {worktree.head ? <span className="git-worktree-head">{worktree.head}</span> : null}
          {worktree.is_current ? <span className="git-worktree-current">{t("git.cwd")}</span> : null}
        </span>
      </header>
      {!collapsed ? (
        <div className="git-worktree-body">
          <div className="git-worktree-path" title={worktree.path}>
            {worktree.path}
          </div>
          <GitChangeSection
            title={t("git.staged")}
            worktree={worktree}
            changes={worktree.staged}
            emptyText={t("git.noStaged")}
            action="unstage"
            staged
            onOpenGitFile={onOpenGitFile}
            onOpenGitDiff={onOpenGitDiff}
            onGitAction={onGitAction}
          />
          <GitChangeSection
            title={t("git.unstaged")}
            worktree={worktree}
            changes={worktree.unstaged}
            emptyText={t("git.noUnstaged")}
            action="stage"
            onOpenGitFile={onOpenGitFile}
            onOpenGitDiff={onOpenGitDiff}
            onGitAction={onGitAction}
          />
        </div>
      ) : null}
    </article>
  );
}

function GitChangeSection({
  title,
  worktree,
  changes,
  emptyText,
  action,
  staged = false,
  onOpenGitFile,
  onOpenGitDiff,
  onGitAction,
}: {
  title: string;
  worktree: SessionGitWorktreePayload;
  changes: SessionGitFileChangePayload[];
  emptyText: string;
  action: SessionGitActionKind;
  staged?: boolean;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onOpenGitDiff: (worktree: SessionGitWorktreePayload, change?: SessionGitFileChangePayload, staged?: boolean) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  const { t } = useI18n();
  return (
    <section className="git-change-section">
      <h3>
        <span>{title}</span>
        <span className={`git-change-count git-change-count-${staged ? "staged" : "unstaged"}`}>{changes.length}</span>
      </h3>
      {changes.length > 0 ? (
        <div className="git-change-list">
          {changes.map((change) => (
            <div
              key={`${change.status}-${change.path}`}
              className="git-change-row"
              role="treeitem"
              aria-label={`${change.status.trim() || change.status} ${change.path}`}
            >
              <span className="git-change-status">{change.status}</span>
              <span className="git-change-path" title={change.path}>
                {change.path}
              </span>
              <span className="git-change-actions git-change-floating-actions">
                <button
                  type="button"
                  className="icon-button"
                  aria-label={t("git.diffFile", { path: change.path })}
                  title={t("git.diffFile", { path: change.path })}
                  onClick={() => onOpenGitDiff(worktree, change, staged)}
                >
                  <File size={13} aria-hidden="true" />
                </button>
                <button
                  type="button"
                  className="icon-button"
                  aria-label={t("git.openFile", { path: change.path })}
                  title={t("git.openFile", { path: change.path })}
                  onClick={() => onOpenGitFile(worktree, change)}
                >
                  <FilePenLine size={13} aria-hidden="true" />
                </button>
                <button
                  type="button"
                  className="icon-button"
                  aria-label={
                    action === "stage"
                      ? t("git.stageFile", { path: change.path })
                      : t("git.unstageFile", { path: change.path })
                  }
                  title={
                    action === "stage"
                      ? t("git.stageFile", { path: change.path })
                      : t("git.unstageFile", { path: change.path })
                  }
                  onClick={() => onGitAction(worktree, change, action)}
                >
                  {action === "stage" ? <Plus size={13} aria-hidden="true" /> : <Minus size={13} aria-hidden="true" />}
                </button>
                <button
                  type="button"
                  className="icon-button danger"
                  aria-label={t("git.discardFile", { path: change.path })}
                  title={t("git.discardFile", { path: change.path })}
                  onClick={() => onGitAction(worktree, change, "discard")}
                >
                  <Undo2 size={13} aria-hidden="true" />
                </button>
              </span>
            </div>
          ))}
        </div>
      ) : (
        <div className="git-change-empty">{emptyText}</div>
      )}
    </section>
  );
}

function GitGraph({ lines }: { lines: string[] }) {
  const { t } = useI18n();
  return (
    <div className="git-graph-lines" aria-label={t("git.commits")}>
      {lines.map((line, index) => {
        const parsed = parseGitGraphLine(line);
        const commit = parsed.commit ? parseGitCommitText(parsed.commit) : undefined;
        const rowClassName = [
          "git-graph-row",
          commit ? "git-graph-row-commit" : "git-graph-row-connector",
          commit?.isHead ? "git-graph-row-head" : "",
        ]
          .filter(Boolean)
          .join(" ");
        return (
          <div key={`${index}-${line}`} className={rowClassName}>
            <div className="git-graph-lanes" aria-hidden="true">
              {parsed.lanes.map((lane, laneIndex) => (
                <span
                  key={`${laneIndex}-${lane}`}
                  className={`git-graph-lane ${gitGraphLaneClass(lane)}`}
                  style={gitGraphLaneStyle(laneIndex)}
                />
              ))}
            </div>
            {commit ? <GitGraphCommit commit={commit} rawCommit={parsed.commit} /> : null}
          </div>
        );
      })}
    </div>
  );
}

type GitGraphRefTone = "head" | "branch" | "remote" | "tag";

interface GitGraphRef {
  label: string;
  tone: GitGraphRefTone;
}

interface GitGraphCommitText {
  hash: string;
  message: string;
  refs: GitGraphRef[];
  isHead: boolean;
}

function GitGraphCommit({ commit, rawCommit }: { commit: GitGraphCommitText; rawCommit: string }) {
  return (
    <div className={`git-graph-commit${commit.isHead ? " git-graph-commit-head" : ""}`} title={rawCommit}>
      {commit.hash ? <span className="git-graph-hash">{commit.hash}</span> : null}
      {commit.message ? <span className="git-graph-message">{commit.message}</span> : null}
      {commit.refs.length > 0 ? (
        <span className="git-graph-refs" title={commit.refs.map((ref) => ref.label).join(", ")}>
          {commit.refs.map((ref) => (
            <span key={`${ref.tone}-${ref.label}`} className={`git-graph-ref git-graph-ref-${ref.tone}`}>
              {ref.label}
            </span>
          ))}
        </span>
      ) : null}
    </div>
  );
}

function SessionFileRow({
  entry,
  onOpenDirectory,
  onOpenFile,
  onDownload,
  onDelete,
  t,
}: {
  entry: SessionFileEntryPayload;
  onOpenDirectory: (path: string) => void;
  onOpenFile: (entry: SessionFileEntryPayload) => void;
  onDownload: (entry: SessionFileEntryPayload) => void;
  onDelete: (entry: SessionFileEntryPayload) => void;
  t: Translate;
}) {
  const isDirectory = entry.kind === "directory";

  return (
    <div className="file-row">
      <span className={`file-icon ${entry.kind}`} aria-hidden="true">
        {entry.kind === "directory" ? <Folder size={15} /> : null}
        {entry.kind === "symlink" ? <Link2 size={15} /> : null}
        {entry.kind !== "directory" && entry.kind !== "symlink" ? <File size={15} /> : null}
      </span>
      <span className="file-name" title={entry.path}>
        {entry.name}
      </span>
      <span className="file-size">{formatBytes(entry.size_bytes)}</span>
      <span className="file-actions">
        {isDirectory ? (
          <button type="button" className="icon-button" aria-label={t("files.open", { name: entry.name })} onClick={() => onOpenDirectory(entry.path)}>
            <Folder size={14} aria-hidden="true" />
          </button>
        ) : (
          <>
            <button type="button" className="icon-button" aria-label={t("files.edit", { name: entry.name })} onClick={() => onOpenFile(entry)}>
              <FilePenLine size={14} aria-hidden="true" />
            </button>
            <button type="button" className="icon-button" aria-label={t("files.download", { name: entry.name })} onClick={() => onDownload(entry)}>
              <Download size={14} aria-hidden="true" />
            </button>
          </>
        )}
        <button
          type="button"
          className="icon-button danger"
          aria-label={t("files.delete", { name: entry.name })}
          onClick={() => onDelete(entry)}
        >
          <Trash2 size={14} aria-hidden="true" />
        </button>
      </span>
    </div>
  );
}

function lastPathSegment(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index >= 0 ? trimmed.slice(index + 1) || trimmed : trimmed;
}

function parseGitGraphLine(line: string): { lanes: string[]; commit: string } {
  const commitMarkerIndex = line.indexOf("*");
  if (commitMarkerIndex < 0) {
    return { lanes: line.split(""), commit: "" };
  }

  return {
    // lane 保留到 commit 节点为止，后面的 commit 文本单独排版。
    lanes: line.slice(0, commitMarkerIndex + 1).split(""),
    commit: line.slice(commitMarkerIndex + 1).trim(),
  };
}

function parseGitCommitText(commit: string): GitGraphCommitText {
  const [hash = "", ...rest] = commit.split(/\s+/);
  const text = rest.join(" ").trim();
  const refMatch = text.match(/^\(([^)]+)\)\s*(.*)$/);
  if (refMatch) {
    const refs = parseGitGraphRefs(refMatch[1]);
    return { hash, refs, isHead: refs.some((ref) => ref.tone === "head"), message: refMatch[2] || text };
  }
  return { hash, refs: [], isHead: false, message: text || commit };
}

function parseGitGraphRefs(refText: string): GitGraphRef[] {
  return refText
    .split(",")
    .map((ref) => ref.trim())
    .filter(Boolean)
    .flatMap((ref) => {
      const headTarget = ref.match(/^HEAD\s*->\s*(.+)$/);
      if (headTarget) {
        return [
          { label: "HEAD", tone: "head" as const },
          { label: headTarget[1], tone: gitGraphRefTone(headTarget[1]) },
        ];
      }
      if (ref === "HEAD") {
        return [{ label: ref, tone: "head" as const }];
      }
      if (ref.startsWith("tag: ")) {
        return [{ label: ref.slice(5), tone: "tag" as const }];
      }
      return [{ label: ref, tone: gitGraphRefTone(ref) }];
    });
}

function gitGraphRefTone(ref: string): GitGraphRefTone {
  if (/^(origin|upstream|remote)\//.test(ref)) {
    return "remote";
  }
  return "branch";
}

function gitGraphLaneClass(lane: string): string {
  switch (lane) {
    case "*":
      return "git-graph-node";
    case "|":
      return "git-graph-rail";
    case "/":
      return "git-graph-diagonal-left";
    case "\\":
      return "git-graph-diagonal-right";
    case "_":
    case "-":
      return "git-graph-horizontal";
    case " ":
      return "git-graph-space";
    default:
      return "git-graph-rail";
  }
}

function gitGraphLaneStyle(index: number): CSSProperties {
  return { "--git-graph-color": gitGraphLaneColor(index) } as CSSProperties;
}

function gitGraphLaneColor(index: number): string {
  const colors = ["#4ea1ff", "#f0883e", "#3fb950", "#d2a8ff", "#ff7b72", "#56d4dd"];
  return colors[index % colors.length];
}

function graphPanelHeight(panel: HTMLDivElement | null): number {
  return panel?.getBoundingClientRect().height || GIT_SPLIT_FALLBACK_PANEL_HEIGHT;
}

function statusPaneHeight(statusPane: HTMLElement | null): number {
  return statusPane?.getBoundingClientRect().height || GIT_SPLIT_FALLBACK_PANEL_HEIGHT / 2;
}

function clampGitSplitHeight(height: number, panelHeight: number): number {
  // Graph 和 Changes 至少保留一行标题高度，避免拖动后某一侧被压到不可操作。
  const maxHeight = Math.max(GIT_SPLIT_MIN_PANE_HEIGHT, panelHeight - GIT_SPLIT_MIN_PANE_HEIGHT);
  return Math.max(GIT_SPLIT_MIN_PANE_HEIGHT, Math.min(height, maxHeight));
}

function parentPath(path: string): string {
  const trimmed = path.trim();
  if (!trimmed || trimmed === "/") {
    return "/";
  }
  const withoutTrailing = trimmed.replace(/\/+$/, "");
  const index = withoutTrailing.lastIndexOf("/");
  if (index <= 0) {
    return "/";
  }
  return withoutTrailing.slice(0, index);
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return "-";
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }

  return `${value >= 10 ? value.toFixed(0) : value.toFixed(1)} ${units[unitIndex]}`;
}
