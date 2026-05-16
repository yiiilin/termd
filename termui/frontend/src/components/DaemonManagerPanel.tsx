import { Check, Pencil, Trash2, X } from "lucide-react";
import type { PairedServerState, UUID } from "../protocol/types";
import { useI18n } from "../i18n";

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
  const { t } = useI18n();
  return (
    <section className="panel daemon-manager" aria-label={t("daemons.managerAria")}>
      <div className="panel-title">{t("daemons.title")}</div>
      {props.servers.length === 0 ? <div className="empty-list">{t("daemons.empty")}</div> : null}
      {props.servers.map((item) => {
        const label = daemonLabel(item.server, item.label);
        const active = item.server.server_id === props.activeServerId;
        const renaming = item.server.server_id === props.renamingServerId;

        return (
          <div className={active ? "daemon-manager-row active" : "daemon-manager-row"} key={item.server.server_id}>
            {renaming ? (
              <label className="daemon-rename-form">
                <span>{t("daemons.name")}</span>
                <input
                  aria-label={t("daemons.name")}
                  value={props.renameDraft}
                  onChange={(event) => props.onRenameDraftChange(event.target.value)}
                  autoFocus
                />
              </label>
            ) : (
              <div className="daemon-manager-main">
                <strong>{label}</strong>
                <span>{item.server.url}</span>
              </div>
            )}
            <div className="daemon-manager-actions">
              {renaming ? (
                <>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t("daemons.saveName")}
                    onClick={() => props.onSaveRename(item.server.server_id)}
                  >
                    <Check size={15} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t("daemons.cancelRename")}
                    onClick={props.onCancelRename}
                  >
                    <X size={15} aria-hidden="true" />
                  </button>
                </>
              ) : (
                <>
                  <button
                    type="button"
                    // 当前 daemon 只表达选择状态；进入工作台统一走页面级 Open workspace，避免管理动作隐式跳转。
                    onClick={() => props.onSelect(item.server.server_id)}
                    disabled={active}
                    aria-label={t("daemons.useDaemon", { label })}
                  >
                    {active ? t("daemons.active") : t("daemons.use")}
                  </button>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t("daemons.renameDaemon", { label })}
                    onClick={() => props.onStartRename(item.server.server_id, label)}
                  >
                    <Pencil size={15} aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    className="icon-button danger"
                    aria-label={t("daemons.deleteDaemon", { label })}
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
