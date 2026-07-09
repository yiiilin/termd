import { useEffect, useState } from "react";
import { KeyRound, Save, ScanQrCode, Server, Settings, Wifi } from "lucide-react";
import type { PairedServerState, UUID } from "../protocol/types";
import { displayUrlWithoutQueryOrFragment } from "../protocol/url";
import { useI18n, type Translate } from "../i18n";

interface ConnectionPanelProps {
  url: string;
  token: string;
  status: string;
  canSaveUrl?: boolean;
  onManage?: () => void;
  onUrlChange: (url: string) => void;
  onTokenChange: (token: string) => void;
  onPair: () => void;
  onScanQr?: () => void;
  onSaveUrl?: () => void;
  showUrlEditor?: boolean;
}

export function ConnectionPanel(props: ConnectionPanelProps) {
  const [urlEditorOpen, setUrlEditorOpen] = useState(Boolean(props.showUrlEditor));
  const { t } = useI18n();

  useEffect(() => {
    if (props.showUrlEditor) {
      setUrlEditorOpen(true);
    }
  }, [props.showUrlEditor]);

  return (
    <section className="panel" aria-label={t("connection.panelAria")}>
      {urlEditorOpen ? (
        <label>
          <span>{t("connection.wsUrl")}</span>
          <input
            aria-label={t("connection.wsUrl")}
            value={props.url}
            onChange={(event) => props.onUrlChange(event.target.value)}
            spellCheck={false}
          />
        </label>
      ) : (
        <div className="connection-address-summary">
          <div>
            <span>{t("connection.address")}</span>
            <code>{displayUrlWithoutQueryOrFragment(props.url)}</code>
          </div>
          <button type="button" className="icon-button" aria-label={t("connection.editAddress")} onClick={() => setUrlEditorOpen(true)}>
            <Settings size={15} aria-hidden="true" />
          </button>
        </div>
      )}
      <label>
        <span>{t("connection.pairingToken")}</span>
        <input
          aria-label={t("connection.pairingToken")}
          type="password"
          value={props.token}
          onChange={(event) => props.onTokenChange(event.target.value)}
          spellCheck={false}
          autoComplete="off"
        />
      </label>
      <div className="connection-actions">
        <button type="button" onClick={props.onPair} disabled={props.status === "pairing" || !props.token.trim()}>
          <KeyRound size={16} aria-hidden="true" />
          {t("connection.pair")}
        </button>
        {props.onScanQr ? (
          <button type="button" onClick={props.onScanQr} disabled={props.status === "pairing"}>
            <ScanQrCode size={16} aria-hidden="true" />
            {t("connection.scanQr")}
          </button>
        ) : null}
        {props.canSaveUrl ? (
          <button
            type="button"
            onClick={props.onSaveUrl}
            disabled={props.status === "saving_url" || !props.url.trim() || !urlEditorOpen}
          >
            <Save size={16} aria-hidden="true" />
            {t("connection.saveUrl")}
          </button>
        ) : null}
        {props.onManage ? (
          <button type="button" onClick={props.onManage}>
            <Server size={16} aria-hidden="true" />
            {t("connection.manageDaemons")}
          </button>
        ) : null}
      </div>
    </section>
  );
}

interface ConnectionStatusPanelProps {
  url: string;
  status: string;
  servers?: { server: PairedServerState; label: string }[];
  activeServerId?: UUID;
  onServerChange?: (serverId: UUID) => void;
  onEdit?: () => void;
  onManage?: () => void;
}

export function ConnectionStatusPanel(props: ConnectionStatusPanelProps) {
  const { t } = useI18n();
  return (
    <section className="panel connection-status" aria-label={t("connection.statusAria")}>
      <div className="connection-status-main">
        <Wifi size={16} aria-hidden="true" />
        <strong>{connectionLabel(props.status, t)}</strong>
        {props.onEdit ? (
          <button type="button" className="icon-button" aria-label={t("connection.editConnection")} onClick={props.onEdit}>
            <Settings size={15} aria-hidden="true" />
          </button>
        ) : null}
        {props.onManage ? (
          <button type="button" className="icon-button" aria-label={t("connection.manageDaemons")} onClick={props.onManage}>
            <Server size={15} aria-hidden="true" />
          </button>
        ) : null}
      </div>
      {props.servers && props.servers.length > 1 ? (
        <label className="daemon-switcher">
          <span>{t("connection.daemon")}</span>
          <select
            aria-label={t("connection.daemon")}
            value={props.activeServerId ?? ""}
            onChange={(event) => props.onServerChange?.(event.target.value)}
          >
            {props.servers.map((item) => (
              <option key={item.server.server_id} value={item.server.server_id}>
                {item.label}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      <div className="server-url">{displayUrlWithoutQueryOrFragment(props.url)}</div>
    </section>
  );
}

function connectionLabel(status: string, t: Translate): string {
  if (status === "idle" || status === "connecting" || status === "listing") {
    return t("app.connectionChecking");
  }
  return t("app.connectionReady");
}
