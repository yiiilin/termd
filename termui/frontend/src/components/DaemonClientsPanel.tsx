import { Trash2 } from "lucide-react";
import type { DaemonClientSummaryPayload, UUID } from "../protocol/types";

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
  return (
    <section className="panel daemon-clients" aria-label="daemon clients">
      <div className="panel-title">Clients</div>
      {clients.length === 0 ? <div className="empty-list">No clients</div> : null}
      {clients.map((client) => {
        const label = clientLabel(client, currentDeviceId);
        const showPeerIp = Boolean(client.peer_ip && client.peer_ip !== label);
        const forgetting = forgettingClientIds?.has(client.device_id) ?? false;
        return (
          <div className="client-row" key={client.client_id}>
            <div className="client-row-main">
              <span className={client.online ? "status-dot online" : "status-dot offline"} aria-hidden="true" />
              <strong>{label}</strong>
              <span>{client.online ? "online" : "offline"}</span>
            </div>
            <div className="client-row-meta">
              <span>{attachedLabel(client.attached_session_ids)}</span>
              {showPeerIp ? <span>{client.peer_ip}</span> : null}
            </div>
            {!client.online && onForgetOfflineClient ? (
              <button
                type="button"
                className="icon-button danger client-forget-button"
                aria-label={`Delete offline client ${label}`}
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

function clientLabel(client: DaemonClientSummaryPayload, currentDeviceId?: UUID): string {
  if (client.device_id === currentDeviceId) {
    return "This browser";
  }
  return client.name?.trim() || client.peer_ip || "Web client";
}

function attachedLabel(sessionIds: string[]): string {
  if (sessionIds.length === 0) {
    return "detached";
  }
  if (sessionIds.length === 1) {
    return "attached";
  }
  return `attached ${sessionIds.length} sessions`;
}
