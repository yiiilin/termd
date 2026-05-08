import type { DaemonClientSummaryPayload } from "../protocol/types";

interface DaemonClientsPanelProps {
  clients: DaemonClientSummaryPayload[];
}

export function DaemonClientsPanel({ clients }: DaemonClientsPanelProps) {
  return (
    <section className="panel daemon-clients" aria-label="daemon clients">
      <div className="panel-title">Clients</div>
      {clients.length === 0 ? <div className="empty-list">No clients</div> : null}
      {clients.map((client) => (
        <div className="client-row" key={client.client_id}>
          <div className="client-row-main">
            <span className={client.online ? "status-dot online" : "status-dot offline"} aria-hidden="true" />
            <strong>{client.peer_ip ?? "unknown ip"}</strong>
            <span>{client.online ? "online" : "offline"}</span>
          </div>
          <div className="client-row-meta">
            <span>{attachedLabel(client.attached_session_ids)}</span>
            <span>{shortId(client.device_id)}</span>
            <span>{shortId(client.client_id)}</span>
          </div>
        </div>
      ))}
    </section>
  );
}

function shortId(value: string): string {
  return value.slice(0, 8);
}

function attachedLabel(sessionIds: string[]): string {
  if (sessionIds.length === 0) {
    return "detached";
  }
  if (sessionIds.length === 1) {
    return `attached ${shortId(sessionIds[0])}`;
  }
  return `attached ${sessionIds.length}`;
}
