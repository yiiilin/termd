import { UsersRound, X } from "lucide-react";
import { useI18n } from "../i18n";
import type { DaemonClientSummaryPayload, SessionSummaryPayload } from "../protocol/types";
import { DaemonClientsPanel } from "./DaemonClientsPanel";
import { useModalFocus } from "./useModalFocus";

interface ClientsDialogProps {
  clients: DaemonClientSummaryPayload[];
  sessions: SessionSummaryPayload[];
  currentDeviceId?: string;
  onClose: () => void;
}

/**
 * 客户端列表模态框：工作台顶栏「客户端」按钮打开，
 * 居中弹窗展示在线设备与连接状态（替代早期下拉 popover）。
 */
export function ClientsDialog({
  clients,
  sessions,
  currentDeviceId,
  onClose,
}: ClientsDialogProps) {
  const { t } = useI18n();
  const dialogRef = useModalFocus({ open: true, onClose });

  return (
    <div
      className="modal-backdrop clients-dialog-backdrop"
      role="presentation"
      onMouseDown={(event) => event.target === event.currentTarget && onClose()}
    >
      <section
        ref={dialogRef}
        className="clients-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="clients-dialog-title"
      >
        <header className="clients-dialog-header">
          <div className="clients-dialog-title-group">
            <UsersRound size={16} aria-hidden="true" />
            <h2 id="clients-dialog-title">{t("clients.title")}</h2>
          </div>
          <button
            type="button"
            className="icon-button"
            aria-label={t("settings.close")}
            onClick={onClose}
          >
            <X size={16} aria-hidden="true" />
          </button>
        </header>
        <div className="clients-dialog-body">
          <DaemonClientsPanel
            clients={clients}
            sessions={sessions}
            currentDeviceId={currentDeviceId}
          />
        </div>
      </section>
    </div>
  );
}
