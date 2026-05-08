import { Check, MonitorUp, Pencil, Trash2, X } from "lucide-react";
import type { SessionSummaryPayload, UUID } from "../protocol/types";

interface SessionListProps {
  sessions: SessionSummaryPayload[];
  selectedSessionId?: UUID;
  attachedSessionId?: UUID;
  attachedRole?: "controller" | "viewer";
  renamingSessionId?: UUID;
  renameDraft: string;
  onSelect: (sessionId: UUID) => void;
  onAttach: (sessionId: UUID) => void;
  onStartRename: (sessionId: UUID, currentName: string) => void;
  onRenameDraftChange: (name: string) => void;
  onSaveRename: (sessionId: UUID) => void;
  onCancelRename: () => void;
  onClose: (sessionId: UUID) => void;
}

export function SessionList(props: SessionListProps) {
  return (
    <section className="session-list" aria-label="sessions">
      {props.sessions.length === 0 ? <div className="empty-list">No sessions</div> : null}
      {props.sessions.map((session) => {
        const isAttached = session.session_id === props.attachedSessionId;
        const isController = isAttached && props.attachedRole === "controller";
        const displayName = session.name?.trim() || shortSessionId(session.session_id);
        const isRenaming = props.renamingSessionId === session.session_id;
        return (
          <div
            className={session.session_id === props.selectedSessionId ? "session-row selected" : "session-row"}
            key={session.session_id}
            onClick={() => props.onSelect(session.session_id)}
          >
            <div className="session-main">
              {isRenaming ? (
                <form
                  className="session-rename-form"
                  id={`session-rename-${session.session_id}`}
                  onClick={(event) => event.stopPropagation()}
                  onSubmit={(event) => {
                    event.preventDefault();
                    props.onSaveRename(session.session_id);
                  }}
                >
                  <label className="sr-only" htmlFor={`session-name-${session.session_id}`}>
                    Session name
                  </label>
                  <input
                    id={`session-name-${session.session_id}`}
                    aria-label="Session name"
                    value={props.renameDraft}
                    onChange={(event) => props.onRenameDraftChange(event.target.value)}
                    autoFocus
                  />
                </form>
              ) : (
                <>
                  <strong>{displayName}</strong>
                  <span>{session.session_id}</span>
                </>
              )}
            </div>
            <span className="session-size">
              {session.size.cols}x{session.size.rows}
            </span>
            <div className="session-actions" aria-label="Session actions" onClick={(event) => event.stopPropagation()}>
              {isRenaming ? (
                <>
                  <button
                    type="submit"
                    className="icon-button"
                    form={`session-rename-${session.session_id}`}
                    aria-label="Save session name"
                  >
                    <Check size={15} aria-hidden="true" />
                  </button>
                  <button type="button" className="icon-button" aria-label="Cancel rename" onClick={props.onCancelRename}>
                    <X size={15} aria-hidden="true" />
                  </button>
                </>
              ) : (
                <>
                  <button
                    type="button"
                    className="icon-action"
                    disabled={isController}
                    onClick={() => {
                      if (!isController) {
                        props.onAttach(session.session_id);
                      }
                    }}
                  >
                    <MonitorUp size={15} aria-hidden="true" />
                    {isController ? "Attached" : "Attach"}
                  </button>
                  <span className="session-actions-spacer" aria-hidden="true" />
                  <button
                    type="button"
                    className="icon-button"
                    aria-label="Rename session"
                    onClick={() =>
                      props.onStartRename(session.session_id, session.name?.trim() || shortSessionId(session.session_id))
                    }
                  >
                    <Pencil size={15} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button danger"
                    aria-label="Close session"
                    onClick={() => props.onClose(session.session_id)}
                  >
                    <Trash2 size={15} aria-hidden="true" />
                  </button>
                </>
              )}
            </div>
          </div>
        );
      })}
    </section>
  );
}

function shortSessionId(sessionId: UUID): string {
  return sessionId.slice(0, 8);
}
