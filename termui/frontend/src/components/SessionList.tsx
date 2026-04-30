import { MonitorUp } from "lucide-react";
import type { SessionSummaryPayload, UUID } from "../protocol/types";

interface SessionListProps {
  sessions: SessionSummaryPayload[];
  selectedSessionId?: UUID;
  onSelect: (sessionId: UUID) => void;
  onAttach: (sessionId: UUID) => void;
}

export function SessionList(props: SessionListProps) {
  return (
    <section className="session-list" aria-label="sessions">
      {props.sessions.length === 0 ? <div className="empty-list">No sessions</div> : null}
      {props.sessions.map((session) => (
        <div
          className={session.session_id === props.selectedSessionId ? "session-row selected" : "session-row"}
          key={session.session_id}
          onClick={() => props.onSelect(session.session_id)}
        >
          <div>
            <strong>{session.session_id}</strong>
            <span>{session.state}</span>
          </div>
          <span>
            {session.size.cols}x{session.size.rows}
          </span>
          <button
            type="button"
            className="icon-action"
            onClick={(event) => {
              event.stopPropagation();
              props.onAttach(session.session_id);
            }}
          >
            <MonitorUp size={15} aria-hidden="true" />
            Attach
          </button>
        </div>
      ))}
    </section>
  );
}
