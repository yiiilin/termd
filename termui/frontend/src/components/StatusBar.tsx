import type { SafeError, UUID } from "../protocol/types";
import { translateSafeErrorMessage, useI18n, type Translate } from "../i18n";

interface StatusBarProps {
  status: string;
  sessionId?: UUID;
  error?: SafeError;
}

export function StatusBar({ status, sessionId, error }: StatusBarProps) {
  const { t } = useI18n();
  return (
    <footer className="status-bar">
      <span>{connectionStateLabel(status, t)}</span>
      <span>{sessionId ? t("status.attached") : t("status.detached")}</span>
      {error ? (
        <span className="status-error">
          {error.code}: {translateSafeErrorMessage(error, t)}
        </span>
      ) : null}
    </footer>
  );
}

function connectionStateLabel(status: string, t: Translate): string {
  switch (status) {
    case "idle":
      return t("connectionState.idle");
    case "connecting":
      return t("connectionState.connecting");
    case "pairing":
      return t("connectionState.pairing");
    case "ready":
      return t("connectionState.ready");
    case "saving_url":
      return t("connectionState.savingUrl");
    case "listing":
      return t("connectionState.listing");
    case "attaching":
      return t("connectionState.attaching");
    case "attached":
      return t("connectionState.attached");
    case "creating":
      return t("connectionState.creating");
    case "error":
      return t("connectionState.error");
    default:
      return status;
  }
}
