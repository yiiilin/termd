import { useEffect, useRef, useState } from "react";
import { ArrowUp, Download, File, FilePenLine, Folder, Link2, PanelRightClose, RefreshCw, Trash2, Upload } from "lucide-react";
import type { SafeError, SessionFileEntryPayload, SessionFilesResultPayload, UUID } from "../protocol/types";

interface SessionFilesPanelProps {
  attachedSessionId?: UUID;
  files?: SessionFilesResultPayload;
  loading: boolean;
  error?: SafeError;
  onOpenDirectory: (path: string) => void;
  onOpenFile: (entry: SessionFileEntryPayload) => void;
  onGoToPath: (path: string) => void;
  onRefresh: () => void;
  onUpload: (file: globalThis.File) => void;
  onDownload: (entry: SessionFileEntryPayload) => void;
  onDelete: (entry: SessionFileEntryPayload) => void;
  onHide: () => void;
}

export function SessionFilesPanel({
  attachedSessionId,
  files,
  loading,
  error,
  onOpenDirectory,
  onOpenFile,
  onGoToPath,
  onRefresh,
  onUpload,
  onDownload,
  onDelete,
  onHide,
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
      <header className="files-panel-header">
        <div className="files-title">
          <Folder size={15} aria-hidden="true" />
          <span>Files</span>
        </div>
        <span className="files-path">{currentPath || "/"}</span>
        <button type="button" className="icon-button files-hide-button" aria-label="Hide files panel" onClick={onHide}>
          <PanelRightClose size={16} aria-hidden="true" />
        </button>
      </header>
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
    </aside>
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
