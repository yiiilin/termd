import type { DaemonClientSummaryPayload, SessionSummaryPayload, UUID } from "../protocol/types";
import { useI18n, type Translate } from "../i18n";
import { fallbackSessionDisplayName, sessionDisplayName } from "../session-names";

interface DaemonClientsPanelProps {
  clients: DaemonClientSummaryPayload[];
  sessions: readonly SessionSummaryPayload[];
  currentDeviceId?: UUID;
}

export function DaemonClientsPanel({
  clients,
  sessions,
  currentDeviceId,
}: DaemonClientsPanelProps) {
  const { t } = useI18n();
  const onlineClients = clients.filter((client) => client.online);
  const sessionNames = new Map(
    sessions.map((session) => [session.session_id, sessionDisplayName(session)]),
  );
  return (
    <section className="panel daemon-clients" aria-label={t("clients.panelAria")}>
      <div className="panel-title">{t("clients.title")}</div>
      {onlineClients.length === 0 ? <div className="empty-list">{t("clients.empty")}</div> : null}
      {onlineClients.map((client) => {
        const label = clientLabel(client, currentDeviceId, t);
        const showPeerIp = Boolean(client.peer_ip && client.peer_ip !== label);
        const attachedSessions = client.attached_session_ids.map((sessionId) => ({
          id: sessionId,
          name: sessionNames.get(sessionId) ?? fallbackSessionDisplayName(sessionId),
        }));
        return (
          <div className="client-row" key={client.client_id}>
            <div className="client-row-main">
              <span className="status-dot online" aria-hidden="true" />
              <strong>{label}</strong>
              <span>{t("clients.online")}</span>
            </div>
            <div
              className="client-row-meta"
              aria-label={attachedSessions.length === 0
                ? t("clients.notViewingSession")
                : t("clients.viewingSessions", { sessions: attachedSessions.map(({ name }) => name).join(", ") })}
            >
              {attachedSessions.length === 0 ? <span>{t("clients.notViewingSession")}</span> : (
                <>
                  <span>{t("clients.viewing")}</span>
                  {attachedSessions.map((session) => <span key={session.id}>{session.name}</span>)}
                </>
              )}
              {showPeerIp ? <span>{client.peer_ip}</span> : null}
            </div>
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
