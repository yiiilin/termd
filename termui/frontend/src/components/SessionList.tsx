import { Check, Pencil, Trash2, X } from "lucide-react";
import type { SessionSummaryPayload, UUID } from "../protocol/types";
import { sessionDisplayName } from "../session-names";

interface SessionListProps {
  sessions: SessionSummaryPayload[];
  selectedSessionId?: UUID;
  renamingSessionId?: UUID;
  renameDraft: string;
  canSaveRename: boolean;
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
        const displayName = sessionDisplayName(session);
        const isRenaming = props.renamingSessionId === session.session_id;
        return (
          <div
            className={session.session_id === props.selectedSessionId ? "session-row selected" : "session-row"}
            key={session.session_id}
            role="button"
            tabIndex={0}
            aria-label={`Open ${displayName}`}
            onClick={() => props.onAttach(session.session_id)}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) {
                return;
              }
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                props.onAttach(session.session_id);
              }
            }}
          >
            <div className="session-row-heading">
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
                  <strong>{displayName}</strong>
                )}
              </div>
              <div className="session-actions" aria-label="Session actions" onClick={(event) => event.stopPropagation()}>
                {isRenaming ? (
                  <>
                    <button
                      type="submit"
                      className="icon-button"
                      form={`session-rename-${session.session_id}`}
                      aria-label="Save session name"
                      disabled={!props.canSaveRename}
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
                      className="icon-button"
                      aria-label="Rename session"
                      onClick={() => props.onStartRename(session.session_id, displayName)}
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
          </div>
        );
      })}
    </section>
  );
}
