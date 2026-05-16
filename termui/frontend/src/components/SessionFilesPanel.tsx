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

const GIT_SPLIT_MIN_PANE_HEIGHT = 24;
const GIT_SPLIT_FALLBACK_PANEL_HEIGHT = 360;

interface SessionFilesPanelProps {
  attachedSessionId?: UUID;
  activeTab: "files" | "git";
  files?: SessionFilesResultPayload;
  loading: boolean;
  error?: SafeError;
  git?: SessionGitResultPayload;
  gitLoading: boolean;
  gitError?: SafeError;
  followTerminalCwd: boolean;
  onTabChange: (tab: "files" | "git") => void;
  onOpenDirectory: (path: string) => void;
  onOpenFile: (entry: SessionFileEntryPayload) => void;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
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
  git,
  gitLoading,
  gitError,
  followTerminalCwd,
  onTabChange,
  onOpenDirectory,
  onOpenFile,
  onOpenGitFile,
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
  const entries = files?.entries ?? [];
  const currentPath = files?.path ?? "";
  const hasCachedEntries = entries.length > 0;
  const [pathDraft, setPathDraft] = useState(currentPath);
  const uploadRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    setPathDraft(currentPath);
  }, [currentPath]);

  return (
    <aside className="files-panel" aria-label="session files">
      {onResizePointerDown ? (
        <div
          className="files-panel-edge-resizer"
          role="separator"
          aria-label="Resize files panel"
          aria-orientation="vertical"
          tabIndex={0}
          onPointerDown={onResizePointerDown}
          onKeyDown={onResizeKeyDown}
        />
      ) : null}
      <header className="files-panel-header">
        <div className="files-tabs" role="tablist" aria-label="Files panel view">
          <button
            type="button"
            className="files-tab"
            role="tab"
            aria-selected={activeTab === "files"}
            onClick={() => onTabChange("files")}
          >
            <Folder size={14} aria-hidden="true" />
            <span>Files</span>
          </button>
          <button
            type="button"
            className="files-tab"
            role="tab"
            aria-selected={activeTab === "git"}
            onClick={() => onTabChange("git")}
          >
            <GitBranch size={14} aria-hidden="true" />
            <span>Git</span>
          </button>
        </div>
        <button type="button" className="icon-button files-hide-button" aria-label="Hide files panel" onClick={onHide}>
          <PanelRightClose size={16} aria-hidden="true" />
        </button>
      </header>
      {activeTab === "files" ? (
        <div className="files-tab-body" role="tabpanel" aria-label="Files">
          <div className="files-toolbar">
            <button
              type="button"
              className="icon-button"
              aria-label="Parent directory"
              disabled={!attachedSessionId || loading || !currentPath}
              onClick={() => onGoToPath(parentPath(currentPath))}
            >
              <ArrowUp size={15} aria-hidden="true" />
            </button>
            <label className="files-path-field">
              <span className="sr-only">Current directory</span>
              <input
                aria-label="Current directory"
                value={pathDraft}
                disabled={!attachedSessionId || loading}
                onChange={(event) => setPathDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    onGoToPath(pathDraft);
                  }
                }}
              />
            </label>
            <button
              type="button"
              className="files-go-button"
              disabled={!attachedSessionId || loading}
              onClick={() => onGoToPath(pathDraft)}
            >
              Go
            </button>
            <button
              type="button"
              className="icon-button"
              aria-label="Refresh files"
              disabled={!attachedSessionId || loading}
              onClick={onRefresh}
            >
              <RefreshCw size={15} aria-hidden="true" />
            </button>
            <button
              type="button"
              className="icon-button"
              aria-label="Upload"
              disabled={!attachedSessionId || loading}
              onClick={() => uploadRef.current?.click()}
            >
              <Upload size={15} aria-hidden="true" />
            </button>
            <input
              ref={uploadRef}
              className="files-upload-input"
              aria-label="Upload file"
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
            {!attachedSessionId ? <div className="files-empty">detached</div> : null}
            {attachedSessionId && loading && !hasCachedEntries ? (
              <div className="files-empty">
                <RefreshCw size={14} aria-hidden="true" />
                loading
              </div>
            ) : null}
            {attachedSessionId && !loading && error ? <div className="files-empty">unavailable</div> : null}
            {attachedSessionId && !loading && !error && entries.length === 0 ? (
              <div className="files-empty">empty directory</div>
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
                  />
                ))
              : null}
          </div>
          <footer className="files-follow-footer">
            <label className="files-follow-toggle">
              <input
                type="checkbox"
                checked={followTerminalCwd}
                disabled={!attachedSessionId}
                onChange={(event) => onFollowTerminalCwdChange(event.currentTarget.checked)}
              />
              <span>Follow terminal cwd</span>
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
          onGitAction={onGitAction}
        />
      )}
    </aside>
  );
}

function GitPanel({
  attachedSessionId,
  git,
  loading,
  error,
  onRefresh,
  onOpenGitFile,
  onGitAction,
}: {
  attachedSessionId?: UUID;
  git?: SessionGitResultPayload;
  loading: boolean;
  error?: SafeError;
  onRefresh: () => void;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  const worktrees = git?.worktrees ?? [];
  const graph = git?.graph ?? [];
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
      aria-label="Git"
      style={splitActive ? ({ "--git-changes-pane-height": `${changesPaneHeight}px` } as CSSProperties) : undefined}
    >
      <section ref={statusPaneRef} className="git-status-pane" aria-label="Git status">
        <header className="git-section-header">
          <button
            type="button"
            className="git-section-toggle"
            aria-expanded={!filesCollapsed}
            aria-label={`${filesCollapsed ? "Expand" : "Collapse"} Git changes`}
            onClick={() => setFilesCollapsed((collapsed) => !collapsed)}
          >
            {filesCollapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
            <span>Changes</span>
          </button>
          <span className="git-repo-label" title={git?.repository_root ?? git?.cwd ?? ""}>
            {git?.repository_root ? lastPathSegment(git.repository_root) : "Repository"}
          </span>
          <button
            type="button"
            className="icon-button"
            aria-label="Refresh Git"
            disabled={!attachedSessionId || loading}
            onClick={onRefresh}
          >
            <RefreshCw size={15} aria-hidden="true" />
          </button>
        </header>
        {!filesCollapsed ? (
          <div className="git-section-body git-status-body" role="tree" aria-label="Git changes tree">
            {!attachedSessionId ? <div className="files-empty">detached</div> : null}
            {attachedSessionId && loading && !git ? (
              <div className="files-empty">
                <RefreshCw size={14} aria-hidden="true" />
                loading
              </div>
            ) : null}
            {attachedSessionId && !loading && error ? <div className="files-empty">unavailable</div> : null}
            {attachedSessionId && !error && git?.error ? <div className="files-empty">{git.error}</div> : null}
            {attachedSessionId && !error && git && !git.error && worktrees.length === 0 ? (
              <div className="files-empty">clean repository</div>
            ) : null}
            {attachedSessionId && !error && !git?.error
              ? worktrees.map((worktree) => (
                  <GitWorktree
                    key={worktree.path}
                    worktree={worktree}
                    onOpenGitFile={onOpenGitFile}
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
          aria-label="Resize Git graph"
          aria-orientation="horizontal"
          tabIndex={0}
          onPointerDown={handleGraphResizePointerDown}
          onPointerMove={handleGraphResizePointerMove}
          onPointerUp={finishGraphResize}
          onPointerCancel={finishGraphResize}
          onKeyDown={handleGraphResizeKeyDown}
        />
      ) : null}
      <section className="git-graph-pane" aria-label="Git graph">
        <header className="git-section-header">
          <button
            type="button"
            className="git-section-toggle"
            aria-expanded={!graphCollapsed}
            aria-label={`${graphCollapsed ? "Expand" : "Collapse"} Git graph`}
            onClick={() => setGraphCollapsed((collapsed) => !collapsed)}
          >
            {graphCollapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
            <span>Graph</span>
          </button>
        </header>
        {!graphCollapsed ? (
          graph.length > 0 ? <GitGraph lines={graph} /> : <div className="files-empty">no commits</div>
        ) : null}
      </section>
    </div>
  );
}

function GitWorktree({
  worktree,
  onOpenGitFile,
  onGitAction,
}: {
  worktree: SessionGitWorktreePayload;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  const label = worktree.branch ?? worktree.head ?? "detached";
  const [collapsed, setCollapsed] = useState(false);

  return (
    <article className="git-worktree" role="treeitem" aria-label={`${label} changes`} aria-expanded={!collapsed}>
      <header className="git-worktree-header">
        <button
          type="button"
          className="git-worktree-toggle"
          aria-expanded={!collapsed}
          aria-label={`${collapsed ? "Expand" : "Collapse"} ${label} worktree`}
          onClick={() => setCollapsed((current) => !current)}
          title={label}
        >
          {collapsed ? <ChevronRight size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}
          <span className="git-worktree-branch" title={label}>
            {label}
          </span>
        </button>
        <span className="git-worktree-floating-meta" aria-hidden="true">
          {worktree.head ? <span className="git-worktree-head">{worktree.head}</span> : null}
          {worktree.is_current ? <span className="git-worktree-current">cwd</span> : null}
        </span>
      </header>
      {!collapsed ? (
        <div className="git-worktree-body">
          <div className="git-worktree-path" title={worktree.path}>
            {worktree.path}
          </div>
          <GitChangeSection
            title="Staged"
            worktree={worktree}
            changes={worktree.staged}
            emptyText="no staged changes"
            action="unstage"
            actionLabel="Unstage"
            onOpenGitFile={onOpenGitFile}
            onGitAction={onGitAction}
          />
          <GitChangeSection
            title="Unstaged"
            worktree={worktree}
            changes={worktree.unstaged}
            emptyText="no unstaged changes"
            action="stage"
            actionLabel="Stage"
            onOpenGitFile={onOpenGitFile}
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
  actionLabel,
  onOpenGitFile,
  onGitAction,
}: {
  title: string;
  worktree: SessionGitWorktreePayload;
  changes: SessionGitFileChangePayload[];
  emptyText: string;
  action: SessionGitActionKind;
  actionLabel: string;
  onOpenGitFile: (worktree: SessionGitWorktreePayload, change: SessionGitFileChangePayload) => void;
  onGitAction: (
    worktree: SessionGitWorktreePayload,
    change: SessionGitFileChangePayload,
    action: SessionGitActionKind,
  ) => void;
}) {
  return (
    <section className="git-change-section">
      <h3>{title}</h3>
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
                  aria-label={`Open ${change.path}`}
                  title={`Open ${change.path}`}
                  onClick={() => onOpenGitFile(worktree, change)}
                >
                  <FilePenLine size={13} aria-hidden="true" />
                </button>
                <button
                  type="button"
                  className="icon-button"
                  aria-label={`${actionLabel} ${change.path}`}
                  title={`${actionLabel} ${change.path}`}
                  onClick={() => onGitAction(worktree, change, action)}
                >
                  {action === "stage" ? <Plus size={13} aria-hidden="true" /> : <Minus size={13} aria-hidden="true" />}
                </button>
                <button
                  type="button"
                  className="icon-button danger"
                  aria-label={`Discard ${change.path}`}
                  title={`Discard ${change.path}`}
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
  return (
    <div className="git-graph-lines" aria-label="Git graph commits">
      {lines.map((line, index) => {
        const parsed = parseGitGraphLine(line);
        return (
          <div key={`${index}-${line}`} className="git-graph-row">
            <div className="git-graph-lanes" aria-hidden="true">
              {parsed.lanes.map((lane, laneIndex) => (
                <span
                  key={`${laneIndex}-${lane}`}
                  className={`git-graph-lane ${gitGraphLaneClass(lane)}`}
                  style={gitGraphLaneStyle(laneIndex)}
                />
              ))}
            </div>
            {parsed.commit ? <GitGraphCommit commit={parsed.commit} /> : null}
          </div>
        );
      })}
    </div>
  );
}

function GitGraphCommit({ commit }: { commit: string }) {
  const parsed = parseGitCommitText(commit);

  return (
    <span className="git-graph-commit" title={commit}>
      {parsed.hash ? <span className="git-graph-hash">{parsed.hash}</span> : null}
      <span className="git-graph-message">{parsed.message}</span>
      {parsed.ref ? <span className="git-graph-ref">{parsed.ref}</span> : null}
    </span>
  );
}

function SessionFileRow({
  entry,
  onOpenDirectory,
  onOpenFile,
  onDownload,
  onDelete,
}: {
  entry: SessionFileEntryPayload;
  onOpenDirectory: (path: string) => void;
  onOpenFile: (entry: SessionFileEntryPayload) => void;
  onDownload: (entry: SessionFileEntryPayload) => void;
  onDelete: (entry: SessionFileEntryPayload) => void;
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
          <button type="button" className="icon-button" aria-label={`Open ${entry.name}`} onClick={() => onOpenDirectory(entry.path)}>
            <Folder size={14} aria-hidden="true" />
          </button>
        ) : (
          <>
            <button type="button" className="icon-button" aria-label={`Edit ${entry.name}`} onClick={() => onOpenFile(entry)}>
              <FilePenLine size={14} aria-hidden="true" />
            </button>
            <button type="button" className="icon-button" aria-label={`Download ${entry.name}`} onClick={() => onDownload(entry)}>
              <Download size={14} aria-hidden="true" />
            </button>
          </>
        )}
        <button
          type="button"
          className="icon-button danger"
          aria-label={`Delete ${entry.name}`}
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

function parseGitCommitText(commit: string): { hash: string; message: string; ref?: string } {
  const [hash = "", ...rest] = commit.split(/\s+/);
  const text = rest.join(" ").trim();
  const refMatch = text.match(/^\(([^)]+)\)\s*(.*)$/);
  if (refMatch) {
    return { hash, ref: refMatch[1], message: refMatch[2] || text };
  }
  return { hash, message: text || commit };
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
