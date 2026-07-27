import { BellRing, Download, FileArchive, LoaderCircle, X } from "lucide-react";
import { useI18n } from "../i18n";
import type { FileOfferPayload } from "../protocol/types";

export type VisibleFileOffer = FileOfferPayload & {
  busy?: boolean;
  error?: string;
};

interface FileOfferCenterProps {
  offers: VisibleFileOffer[];
  showNotificationPermissionPrompt?: boolean;
  onDownload: (offerId: string) => void;
  onDismiss: (offerId: string) => void;
  onRequestNotificationPermission?: () => void;
  onDismissNotificationPermissionPrompt?: () => void;
}

export function FileOfferCenter({
  offers,
  showNotificationPermissionPrompt = false,
  onDownload,
  onDismiss,
  onRequestNotificationPermission,
  onDismissNotificationPermissionPrompt,
}: FileOfferCenterProps) {
  const { t } = useI18n();
  if (!showNotificationPermissionPrompt && offers.length === 0) return null;

  return (
    <section className="file-offer-center" aria-label={t("fileOffers.aria")} aria-live="polite">
      <div className="file-offer-list">
        {showNotificationPermissionPrompt ? (
          <article className="file-offer-card notification-permission-card">
            <BellRing size={18} aria-hidden="true" />
            <div className="file-offer-content">
              <strong>{t("fileOffers.permissionTitle")}</strong>
              <span>{t("fileOffers.permissionDescription")}</span>
            </div>
            <button type="button" className="file-offer-primary" onClick={onRequestNotificationPermission}>
              {t("fileOffers.enableNotifications")}
            </button>
            <button
              type="button"
              className="icon-button file-offer-dismiss"
              aria-label={t("fileOffers.notNow")}
              title={t("fileOffers.notNow")}
              onClick={onDismissNotificationPermissionPrompt}
            >
              <X size={16} aria-hidden="true" />
            </button>
          </article>
        ) : null}
        {offers.map((offer) => {
          const name = canonicalBasename(offer.path);
          return (
            <article className="file-offer-card" key={offer.offer_id}>
              <FileArchive size={18} aria-hidden="true" />
              <div className="file-offer-content">
                <strong>{name}</strong>
                <code title={offer.path}>{offer.path}</code>
                <span>{formatFileSize(offer.size_bytes)}</span>
                {offer.error ? <span className="file-offer-error" role="alert">{offer.error}</span> : null}
              </div>
              <button
                type="button"
                className="file-offer-primary"
                aria-label={t(offer.busy ? "fileOffers.preparing" : "fileOffers.download", { name })}
                disabled={offer.busy}
                onClick={() => onDownload(offer.offer_id)}
              >
                {offer.busy ? <LoaderCircle className="file-offer-spinner" size={16} aria-hidden="true" /> : <Download size={16} aria-hidden="true" />}
                <span>{t(offer.busy ? "fileOffers.preparingAction" : "fileOffers.downloadAction")}</span>
              </button>
              <button
                type="button"
                className="icon-button file-offer-dismiss"
                aria-label={t("fileOffers.dismiss", { name })}
                title={t("fileOffers.dismiss", { name })}
                onClick={() => onDismiss(offer.offer_id)}
              >
                <X size={16} aria-hidden="true" />
              </button>
            </article>
          );
        })}
      </div>
    </section>
  );
}

function canonicalBasename(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value >= 10 || Number.isInteger(value) ? value.toFixed(0) : value.toFixed(1)} ${units[unitIndex]}`;
}
