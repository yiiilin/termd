import RFB, { type RFBDisconnectEvent } from "@novnc/novnc";
import { Keyboard, Loader2, Maximize2, RefreshCcw, Square } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createTranslator, I18nProvider, resolveLocale, useI18n } from "../i18n";
import { BrowserWorkspaceClient, browserWebSocketUrl } from "../protocol/browser-client";
import { toSafeError } from "../protocol/errors";
import type { BrowserState, EffectiveTheme, FileOfferPayload, SafeError, UUID } from "../protocol/types";
import { V070Client } from "../protocol/v070-client";
import { DEFAULT_BROWSER_PREFERENCES, loadBrowserState } from "../state/browser-state";
import { resolveTheme } from "../theme";
import { DestructiveActionDialog } from "./DestructiveActionDialog";
import { FileOfferCenter, type VisibleFileOffer } from "./FileOfferCenter";

interface BrowserViewerProps {
  browserId: UUID;
  serverId: UUID;
}

type ViewerStatus = "connecting" | "connected" | "disconnected" | "stopped" | "error";

const FILE_OFFER_RETRY_BASE_DELAY_MS = 500;
const FILE_OFFER_RETRY_MAX_DELAY_MS = 8_000;

export default function BrowserViewer({ browserId, serverId }: BrowserViewerProps) {
  const [browserState, setBrowserState] = useState<BrowserState>();
  const [loadError, setLoadError] = useState<SafeError>();
  const [systemTheme, setSystemTheme] = useState<EffectiveTheme>(() => preferredSystemTheme());

  useEffect(() => {
    let active = true;
    void loadBrowserState()
      .then((state) => {
        if (active) {
          setBrowserState(state);
        }
      })
      .catch((caught) => {
        if (active) {
          setLoadError(toSafeError(caught));
        }
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const update = () => setSystemTheme(media.matches ? "dark" : "light");
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);

  const preferences = browserState?.preferences ?? DEFAULT_BROWSER_PREFERENCES;
  const locale = resolveLocale(preferences.language);
  const theme = resolveTheme(preferences.theme, systemTheme);
  const t = useMemo(() => createTranslator(locale), [locale]);
  const server = browserState?.pairedServers.find((candidate) => candidate.server_id === serverId);
  const device = browserState?.device;
  const contextError = loadError ?? (browserState && (!server || !device)
    ? { code: "browser_pairing_missing", message: t("browser.viewerPairingMissing") }
    : undefined);

  useEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    document.querySelector('meta[name="theme-color"]')?.setAttribute(
      "content",
      theme === "light" ? "#e8ecea" : "#111416",
    );
  }, [locale, theme]);

  if (!browserState && !contextError) {
    return <div className="browser-viewer-boot" aria-label={t("browser.viewerLoading")}><Loader2 size={18} /></div>;
  }

  return (
    <I18nProvider locale={locale}>
      {server && device && !contextError ? (
        <BrowserViewerSurface browserId={browserId} server={server} device={device} />
      ) : (
        <div className="browser-viewer-fatal" role="alert">
          <span>{contextError?.message ?? t("browser.error.request")}</span>
          <button type="button" onClick={() => window.location.assign("/")}>{t("browser.backToTermd")}</button>
        </div>
      )}
    </I18nProvider>
  );
}

function BrowserViewerSurface({
  browserId,
  server,
  device,
}: {
  browserId: UUID;
  server: NonNullable<BrowserState["pairedServers"]>[number];
  device: NonNullable<BrowserState["device"]>;
}) {
  const { t } = useI18n();
  const targetRef = useRef<HTMLDivElement>(null);
  const rfbRef = useRef<RFB | undefined>(undefined);
  const fileOfferClientRef = useRef<V070Client | undefined>(undefined);
  const client = useMemo(
    () => new BrowserWorkspaceClient(server, device),
    [device.device_id, server.server_id, server.url],
  );
  const [status, setStatus] = useState<ViewerStatus>("connecting");
  const [error, setError] = useState<SafeError>();
  const [connectionRevision, setConnectionRevision] = useState(0);
  const [stopDialogOpen, setStopDialogOpen] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [fileOffers, setFileOffers] = useState<VisibleFileOffer[]>([]);

  useEffect(() => () => client.dispose(), [client]);

  useEffect(() => {
    let active = true;
    let retryAttempt = 0;
    let retryTimer: number | undefined;
    let metadataClient: V070Client | undefined;
    let unsubscribe: (() => void) | undefined;

    const disposeMetadataClient = (clientToDispose?: V070Client) => {
      if (!clientToDispose || metadataClient !== clientToDispose) return;
      unsubscribe?.();
      unsubscribe = undefined;
      metadataClient = undefined;
      if (fileOfferClientRef.current === clientToDispose) {
        fileOfferClientRef.current = undefined;
      }
      clientToDispose.close();
    };
    const connectMetadata = () => {
      if (!active) return;
      const nextClient = new V070Client(server, device);
      metadataClient = nextClient;
      fileOfferClientRef.current = nextClient;
      unsubscribe = nextClient.watchFileOffers((offer: FileOfferPayload) => {
        if (!active) return;
        setFileOffers((current) => current.some((item) => item.offer_id === offer.offer_id)
          ? current
          : [offer, ...current]);
      });
      void nextClient.subscribeMetadata().then(() => {
        if (active && metadataClient === nextClient) retryAttempt = 0;
      }).catch(() => {
        if (!active || metadataClient !== nextClient) return;
        disposeMetadataClient(nextClient);
        const delay = Math.min(
          FILE_OFFER_RETRY_BASE_DELAY_MS * (2 ** Math.min(retryAttempt, 4)),
          FILE_OFFER_RETRY_MAX_DELAY_MS,
        );
        retryAttempt += 1;
        retryTimer = window.setTimeout(() => {
          retryTimer = undefined;
          connectMetadata();
        }, delay);
      });
    };

    connectMetadata();
    return () => {
      active = false;
      if (retryTimer !== undefined) {
        window.clearTimeout(retryTimer);
      }
      disposeMetadataClient(metadataClient);
    };
  }, [device.device_id, server.server_id, server.url]);

  useEffect(() => {
    let active = true;
    let rfb: RFB | undefined;
    const connect = async () => {
      setStatus("connecting");
      setError(undefined);
      const token = await client.accessToken();
      if (!active || !targetRef.current) {
        return;
      }
      targetRef.current.replaceChildren();
      rfb = new RFB(targetRef.current, browserWebSocketUrl(server.url, browserId), {
        shared: true,
        wsProtocols: ["termd.rfb.v1", token],
      });
      rfbRef.current = rfb;
      rfb.scaleViewport = true;
      rfb.resizeSession = false;
      rfb.clipViewport = false;
      rfb.focusOnClick = true;
      rfb.viewOnly = false;
      rfb.compressionLevel = 2;
      rfb.qualityLevel = 7;
      rfb.background = "var(--color-terminal-bg)";
      rfb.addEventListener("connect", () => {
        if (active) {
          setStatus("connected");
          document.title = `Termd Browser - ${server.name || server.server_id.slice(0, 8)}`;
        }
      });
      rfb.addEventListener("disconnect", (event) => {
        if (!active) {
          return;
        }
        const clean = (event as RFBDisconnectEvent).detail?.clean === true;
        setStatus(clean ? "disconnected" : "error");
        if (!clean) {
          setError({ code: "browser_connection_closed", message: t("browser.viewerDisconnected") });
        }
      });
      rfb.addEventListener("securityfailure", () => {
        if (active) {
          setStatus("error");
          setError({ code: "browser_security_failed", message: t("browser.viewerAuthFailed") });
        }
      });
    };
    void connect().catch((caught) => {
      if (active) {
        setStatus("error");
        setError(toSafeError(caught));
      }
    });
    return () => {
      active = false;
      if (rfbRef.current === rfb) {
        rfbRef.current = undefined;
      }
      rfb?.disconnect();
    };
  }, [browserId, client, connectionRevision, server.name, server.url, t]);

  const reconnect = useCallback(() => {
    rfbRef.current?.disconnect();
    setConnectionRevision((revision) => revision + 1);
  }, []);

  const stop = async () => {
    setStopping(true);
    try {
      await client.close(browserId);
      rfbRef.current?.disconnect();
      setStatus("stopped");
      setStopDialogOpen(false);
      window.setTimeout(() => window.close(), 100);
    } catch (caught) {
      setError(toSafeError(caught));
      setStatus("error");
      setStopDialogOpen(false);
    } finally {
      setStopping(false);
    }
  };

  const downloadFileOffer = useCallback(async (offerId: string) => {
    setFileOffers((current) => current.map((offer) => offer.offer_id === offerId
      ? { ...offer, busy: true, error: undefined }
      : offer));
    try {
      const metadataClient = fileOfferClientRef.current;
      if (!metadataClient) throw new Error("file offer connection is unavailable");
      const ready = await metadataClient.prepareFileOfferDownload(offerId);
      navigateToNativeDownload(metadataClient.fileOfferDownloadUrl(ready));
      setFileOffers((current) => current.map((offer) => offer.offer_id === offerId
        ? { ...offer, busy: false }
        : offer));
    } catch {
      setFileOffers((current) => current.map((offer) => offer.offer_id === offerId
        ? { ...offer, busy: false, error: t("fileOffers.downloadFailed") }
        : offer));
    }
  }, [t]);

  return (
    <main className="browser-viewer-shell">
      <header className="browser-viewer-toolbar">
        <div className={`browser-viewer-status ${status}`} role="status">
          <span aria-hidden="true" />
          <strong>{viewerStatusLabel(status, t)}</strong>
        </div>
        <div className="browser-viewer-actions">
          <button
            type="button"
            className="icon-button"
            aria-label={t("browser.sendCtrlAltDel")}
            title={t("browser.sendCtrlAltDel")}
            disabled={status !== "connected"}
            onClick={() => rfbRef.current?.sendCtrlAltDel()}
          >
            <Keyboard size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label={t("browser.fullscreen")}
            title={t("browser.fullscreen")}
            onClick={() => void document.documentElement.requestFullscreen?.()}
          >
            <Maximize2 size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label={t("browser.reconnect")}
            title={t("browser.reconnect")}
            disabled={status === "connecting" || status === "stopped"}
            onClick={reconnect}
          >
            <RefreshCcw size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button browser-viewer-stop"
            aria-label={t("browser.stopAction")}
            title={t("browser.stopAction")}
            disabled={status === "stopped"}
            onClick={() => setStopDialogOpen(true)}
          >
            <Square size={14} aria-hidden="true" />
          </button>
        </div>
      </header>
      <div ref={targetRef} className="browser-viewer-canvas" aria-label={t("browser.remoteCanvas")} />
      <FileOfferCenter
        offers={fileOffers}
        onDownload={(offerId) => void downloadFileOffer(offerId)}
        onDismiss={(offerId) => setFileOffers((current) => current.filter((offer) => offer.offer_id !== offerId))}
      />
      {status === "connecting" ? (
        <div className="browser-viewer-overlay" aria-hidden="true"><Loader2 size={20} /></div>
      ) : null}
      {status === "error" || status === "disconnected" ? (
        <div className="browser-viewer-overlay browser-viewer-reconnect" role="alert">
          <span>{error?.message ?? t("browser.viewerDisconnected")}</span>
          <button type="button" onClick={reconnect}>
            <RefreshCcw size={15} aria-hidden="true" />
            {t("browser.reconnect")}
          </button>
        </div>
      ) : null}
      {status === "stopped" ? (
        <div className="browser-viewer-overlay browser-viewer-reconnect" role="status">
          <span>{t("browser.viewerStopped")}</span>
          <button type="button" onClick={() => window.location.assign("/")}>{t("browser.backToTermd")}</button>
        </div>
      ) : null}
      <DestructiveActionDialog
        open={stopDialogOpen}
        title={t("browser.stopTitle")}
        description={t("browser.stopDescription")}
        target={server.name || server.server_id}
        cancelLabel={t("destructive.cancel")}
        confirmLabel={t("browser.stopConfirm")}
        busyLabel={t("browser.stopping")}
        busy={stopping}
        onCancel={() => setStopDialogOpen(false)}
        onConfirm={() => void stop()}
      />
    </main>
  );
}

function navigateToNativeDownload(url: string): void {
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.hidden = true;
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
}

function preferredSystemTheme(): EffectiveTheme {
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function viewerStatusLabel(status: ViewerStatus, t: ReturnType<typeof useI18n>["t"]): string {
  switch (status) {
    case "connected":
      return t("browser.viewerConnected");
    case "disconnected":
      return t("browser.viewerDisconnectedShort");
    case "stopped":
      return t("browser.viewerStoppedShort");
    case "error":
      return t("browser.viewerError");
    default:
      return t("browser.viewerConnecting");
  }
}
