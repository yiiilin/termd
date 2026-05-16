import { Trash2 } from "lucide-react";
import type { DaemonClientSummaryPayload, UUID } from "../protocol/types";
import { useI18n, type Translate } from "../i18n";

interface DaemonClientsPanelProps {
  clients: DaemonClientSummaryPayload[];
  currentDeviceId?: UUID;
  forgettingClientIds?: ReadonlySet<UUID>;
  onForgetOfflineClient?: (deviceId: UUID) => void;
}

export function DaemonClientsPanel({
  clients,
  currentDeviceId,
  forgettingClientIds,
  onForgetOfflineClient,
}: DaemonClientsPanelProps) {
  const { t } = useI18n();
  return (
    <section className="panel daemon-clients" aria-label={t("clients.panelAria")}>
      <div className="panel-title">{t("clients.title")}</div>
      {clients.length === 0 ? <div className="empty-list">{t("clients.empty")}</div> : null}
      {clients.map((client) => {
        const label = clientLabel(client, currentDeviceId, t);
        const showPeerIp = Boolean(client.peer_ip && client.peer_ip !== label);
        const forgetting = forgettingClientIds?.has(client.device_id) ?? false;
        return (
          <div className="client-row" key={client.client_id}>
            <div className="client-row-main">
              <span className={client.online ? "status-dot online" : "status-dot offline"} aria-hidden="true" />
              <strong>{label}</strong>
              <span>{client.online ? t("clients.online") : t("clients.offline")}</span>
            </div>
            <div className="client-row-meta">
              <span>{attachedLabel(client.attached_session_ids, t)}</span>
              {showPeerIp ? <span>{client.peer_ip}</span> : null}
            </div>
            {!client.online && onForgetOfflineClient ? (
              <button
                type="button"
                className="icon-button danger client-forget-button"
                aria-label={t("clients.deleteOffline", { label })}
                disabled={forgetting}
                aria-busy={forgetting}
                onClick={() => onForgetOfflineClient(client.device_id)}
              >
                <Trash2 size={15} aria-hidden="true" />
              </button>
            ) : null}
          </div>
        );
      })}
    </section>
  );
}

function clientLabel(client: DaemonClientSummaryPayload, currentDeviceId: UUID | undefined, t: Translate): string {
  if (client.device_id === currentDeviceId) {
    return t("clients.thisBrowser");
  }
  return client.name?.trim() || client.peer_ip || t("clients.webClient");
}

function attachedLabel(sessionIds: string[], t: Translate): string {
  if (sessionIds.length === 0) {
    return t("clients.detached");
  }
  if (sessionIds.length === 1) {
    return t("clients.attached");
  }
  return t("clients.attachedSessions", { count: sessionIds.length });
}
