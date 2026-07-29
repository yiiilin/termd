import {
  ExternalLink,
  Globe2,
  Loader2,
  RefreshCcw,
  Square,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useI18n } from "../i18n";
import {
  BrowserWorkspaceClient,
  browserViewerPath,
  type BrowserSession,
} from "../protocol/browser-client";
import { toSafeError } from "../protocol/errors";
import type { DeviceState, PairedServerState, SafeError, UUID } from "../protocol/types";
import { DestructiveActionDialog } from "./DestructiveActionDialog";
import { useModalFocus } from "./useModalFocus";

interface BrowserWorkspaceDialogProps {
  open: boolean;
  server: PairedServerState;
  device: DeviceState;
  onClose: () => void;
}

interface ViewportPreset {
  value: string;
  width: number;
  height: number;
}

const VIEWPORT_PRESETS: ViewportPreset[] = [
  { value: "1280x800", width: 1280, height: 800 },
  { value: "1440x900", width: 1440, height: 900 },
  { value: "1920x1080", width: 1920, height: 1080 },
];

export function BrowserWorkspaceDialog({
  open,
  server,
  device,
  onClose,
}: BrowserWorkspaceDialogProps) {
  const { locale, t } = useI18n();
  const dialogRef = useModalFocus({ open, onClose });
  const urlInputRef = useRef<HTMLInputElement>(null);
  const client = useMemo(
    () => new BrowserWorkspaceClient(server, device),
    [device.device_id, server.server_id, server.url],
  );
  const [sessions, setSessions] = useState<BrowserSession[]>([]);
  const [url, setUrl] = useState("https://");
  const [viewport, setViewport] = useState("1440x900");
  const [loading, setLoading] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<SafeError>();
  const [pendingStop, setPendingStop] = useState<BrowserSession>();
  const [stopping, setStopping] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(undefined);
    try {
      setSessions(await client.list());
    } catch (caught) {
      setError(toSafeError(caught));
    } finally {
      setLoading(false);
    }
  }, [client]);

  useEffect(() => () => client.dispose(), [client]);

  useEffect(() => {
    if (!open) {
      return;
    }
    void refresh();
    const frame = window.requestAnimationFrame(() => urlInputRef.current?.focus());
    return () => window.cancelAnimationFrame(frame);
  }, [open, refresh]);

  if (!open) {
    return null;
  }

  const create = async () => {
    const preset = VIEWPORT_PRESETS.find((candidate) => candidate.value === viewport) ?? VIEWPORT_PRESETS[1];
    const reservedWindow = reserveViewerWindow(t("browser.preparing"));
    setCreating(true);
    setError(undefined);
    try {
      const session = await client.create({
        url: url.trim(),
        width: preset.width,
        height: preset.height,
      });
      setSessions((current) => [session, ...current.filter((candidate) => candidate.browser_id !== session.browser_id)]);
      navigateViewer(server.server_id, session.browser_id, reservedWindow);
    } catch (caught) {
      reservedWindow?.close();
      setError(toSafeError(caught));
    } finally {
      setCreating(false);
    }
  };

  const stop = async () => {
    if (!pendingStop) {
      return;
    }
    setStopping(true);
    setError(undefined);
    try {
      await client.close(pendingStop.browser_id);
      setSessions((current) => current.filter((candidate) => candidate.browser_id !== pendingStop.browser_id));
      setPendingStop(undefined);
    } catch (caught) {
      setError(toSafeError(caught));
    } finally {
      setStopping(false);
    }
  };

  return (
    <div
      className="modal-backdrop browser-workspace-backdrop"
      role="presentation"
      onMouseDown={(event) => event.target === event.currentTarget && onClose()}
    >
      <section
        ref={dialogRef}
        className="browser-workspace-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="browser-workspace-title"
      >
        <header className="browser-workspace-header">
          <div className="browser-workspace-title-group">
            <Globe2 size={18} aria-hidden="true" />
            <div>
              <h2 id="browser-workspace-title">{t("browser.title")}</h2>
              <span>{server.name || new URL(server.url).host}</span>
            </div>
          </div>
          <div className="browser-workspace-header-actions">
            <button
              type="button"
              className="icon-button"
              aria-label={t("browser.refresh")}
              title={t("browser.refresh")}
              disabled={loading}
              onClick={() => void refresh()}
            >
              <RefreshCcw size={16} aria-hidden="true" />
            </button>
            <button type="button" className="icon-button" aria-label={t("browser.close")} onClick={onClose}>
              <X size={16} aria-hidden="true" />
            </button>
          </div>
        </header>

        <form
          className="browser-create-form"
          onSubmit={(event) => {
            event.preventDefault();
            if (!creating && browserUrlIsValid(url)) {
              void create();
            }
          }}
        >
          <label className="browser-url-field">
            <span>{t("browser.url")}</span>
            <input
              ref={urlInputRef}
              type="url"
              inputMode="url"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              value={url}
              placeholder="https://example.com"
              onChange={(event) => setUrl(event.currentTarget.value)}
            />
          </label>
          <label className="browser-viewport-field">
            <span>{t("browser.viewport")}</span>
            <select value={viewport} onChange={(event) => setViewport(event.currentTarget.value)}>
              {VIEWPORT_PRESETS.map((preset) => (
                <option key={preset.value} value={preset.value}>{preset.value}</option>
              ))}
            </select>
          </label>
          <button
            type="submit"
            className="browser-create-button"
            disabled={creating || !browserUrlIsValid(url)}
          >
            {creating ? <Loader2 size={15} aria-hidden="true" /> : <Globe2 size={15} aria-hidden="true" />}
            {creating ? t("browser.creating") : t("browser.create")}
          </button>
        </form>

        {error ? (
          <div className="browser-workspace-error" role="alert">
            <span>{browserErrorMessage(error, t)}</span>
            <button type="button" onClick={() => setError(undefined)} aria-label={t("browser.dismissError")}>
              <X size={14} aria-hidden="true" />
            </button>
          </div>
        ) : null}

        <div className="browser-session-list" aria-label={t("browser.sessions")} aria-busy={loading}>
          {loading && sessions.length === 0 ? (
            <div className="browser-session-empty"><Loader2 size={17} aria-hidden="true" /></div>
          ) : sessions.length === 0 ? (
            <div className="browser-session-empty">{t("browser.empty")}</div>
          ) : sessions.map((session) => (
            <article className="browser-session-row" key={session.browser_id}>
              <div className="browser-session-status" aria-label={t("browser.running")}>
                <span aria-hidden="true" />
              </div>
              <div className="browser-session-main">
                <strong title={session.display_url}>{session.display_url}</strong>
                <span>
                  {session.width}x{session.height}
                  <b aria-hidden="true">/</b>
                  {new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" }).format(session.created_at_ms)}
                </span>
              </div>
              <div className="browser-session-actions">
                <button
                  type="button"
                  className="icon-button"
                  aria-label={t("browser.open", { url: session.display_url })}
                  title={t("browser.openAction")}
                  onClick={() => navigateViewer(server.server_id, session.browser_id)}
                >
                  <ExternalLink size={15} aria-hidden="true" />
                </button>
                <button
                  type="button"
                  className="icon-button browser-stop-button"
                  aria-label={t("browser.stop", { url: session.display_url })}
                  title={t("browser.stopAction")}
                  onClick={() => setPendingStop(session)}
                >
                  <Square size={14} aria-hidden="true" />
                </button>
              </div>
            </article>
          ))}
        </div>
      </section>

      <DestructiveActionDialog
        open={Boolean(pendingStop)}
        title={t("browser.stopTitle")}
        description={t("browser.stopDescription")}
        target={pendingStop?.display_url ?? ""}
        cancelLabel={t("destructive.cancel")}
        confirmLabel={t("browser.stopConfirm")}
        busyLabel={t("browser.stopping")}
        busy={stopping}
        onCancel={() => setPendingStop(undefined)}
        onConfirm={() => void stop()}
      />
    </div>
  );
}

function browserUrlIsValid(raw: string): boolean {
  try {
    const url = new URL(raw.trim());
    return (url.protocol === "http:" || url.protocol === "https:") && Boolean(url.hostname) && !url.username && !url.password;
  } catch {
    return false;
  }
}

function reserveViewerWindow(label: string): Window | null {
  const reserved = window.open("about:blank", "_blank");
  if (!reserved) {
    return null;
  }
  reserved.opener = null;
  reserved.document.title = label;
  reserved.document.body.textContent = label;
  reserved.document.body.style.cssText = "margin:0;min-height:100vh;display:grid;place-items:center;background:#111416;color:#d9e2df;font:14px system-ui,sans-serif";
  return reserved;
}

function navigateViewer(serverId: UUID, browserId: UUID, target?: Window | null): void {
  const path = browserViewerPath(serverId, browserId);
  if (target && !target.closed) {
    target.location.replace(path);
    return;
  }
  const opened = window.open(path, "_blank");
  if (opened) {
    opened.opener = null;
    return;
  }
  window.location.assign(path);
}

function browserErrorMessage(error: SafeError, t: ReturnType<typeof useI18n>["t"]): string {
  switch (error.code) {
    case "browser_chromium_unavailable":
      return t("browser.error.chromium");
    case "browser_runtime_unavailable":
    case "browser_runtime_install_failed":
      return t("browser.error.runtime");
    case "browser_capacity_exceeded":
      return t("browser.error.capacity");
    case "browser_start_failed":
      return t("browser.error.start");
    default:
      return error.message || t("browser.error.request");
  }
}
