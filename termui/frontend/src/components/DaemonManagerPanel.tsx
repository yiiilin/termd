import { Check, Pencil, Trash2, X } from "lucide-react";
import type { PairedServerState, UUID } from "../protocol/types";

export interface DaemonManagerOption {
  server: PairedServerState;
  label: string;
}

interface DaemonManagerPanelProps {
  servers: DaemonManagerOption[];
  activeServerId?: UUID;
  renamingServerId?: UUID;
  renameDraft: string;
  onSelect: (serverId: UUID) => void;
  onStartRename: (serverId: UUID, currentName: string) => void;
  onRenameDraftChange: (name: string) => void;
  onSaveRename: (serverId: UUID) => void;
  onCancelRename: () => void;
  onForget: (serverId: UUID) => void;
}

export function DaemonManagerPanel(props: DaemonManagerPanelProps) {
  return (
    <section className="panel daemon-manager" aria-label="daemon manager">
      <div className="panel-title">Daemons</div>
      {props.servers.length === 0 ? <div className="empty-list">No daemons</div> : null}
      {props.servers.map((item) => {
        const label = daemonLabel(item.server, item.label);
        const active = item.server.server_id === props.activeServerId;
        const renaming = item.server.server_id === props.renamingServerId;

        return (
          <div className={active ? "daemon-manager-row active" : "daemon-manager-row"} key={item.server.server_id}>
            {renaming ? (
              <label className="daemon-rename-form">
                <span>Daemon name</span>
                <input
                  aria-label="Daemon name"
                  value={props.renameDraft}
                  onChange={(event) => props.onRenameDraftChange(event.target.value)}
                  autoFocus
                />
              </label>
            ) : (
              <div className="daemon-manager-main">
                <strong>{label}</strong>
                <span>{shortId(item.server.server_id)}</span>
                <span>{item.server.url}</span>
              </div>
            )}
            <div className="daemon-manager-actions">
              {renaming ? (
                <>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label="Save daemon name"
                    onClick={() => props.onSaveRename(item.server.server_id)}
                  >
                    <Check size={15} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label="Cancel daemon rename"
                    onClick={props.onCancelRename}
                  >
                    <X size={15} aria-hidden="true" />
                  </button>
                </>
              ) : (
                <>
                  <button
                    type="button"
                    onClick={() => props.onSelect(item.server.server_id)}
                    disabled={active}
                    aria-label={`Use daemon ${label}`}
                  >
                    {active ? "Active" : "Use"}
                  </button>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={`Rename daemon ${label}`}
                    onClick={() => props.onStartRename(item.server.server_id, label)}
                  >
                    <Pencil size={15} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button danger"
                    aria-label={`Delete daemon ${label}`}
                    onClick={() => props.onForget(item.server.server_id)}
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

function daemonLabel(server: PairedServerState, fallback: string): string {
  return server.name?.trim() || fallback;
}

function shortId(value: string): string {
  return value.slice(0, 8);
}
