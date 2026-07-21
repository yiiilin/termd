import { useEffect, useRef } from "react";
import { AlertTriangle, Loader2 } from "lucide-react";
import { useModalFocus } from "./useModalFocus";

interface DestructiveActionDialogProps {
  open: boolean;
  title: string;
  description: string;
  target: string;
  cancelLabel: string;
  confirmLabel: string;
  busyLabel: string;
  busy?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

export function DestructiveActionDialog({
  open,
  title,
  description,
  target,
  cancelLabel,
  confirmLabel,
  busy = false,
  busyLabel,
  onCancel,
  onConfirm,
}: DestructiveActionDialogProps) {
  const confirmStartedRef = useRef(false);
  const requestCancel = () => {
    if (!busy && !confirmStartedRef.current) {
      onCancel();
    }
  };
  const dialogRef = useModalFocus({ open, onClose: requestCancel });

  useEffect(() => {
    if (!open || !busy) {
      confirmStartedRef.current = false;
    }
  }, [busy, open]);

  if (!open) {
    return null;
  }

  const requestConfirm = () => {
    if (busy || confirmStartedRef.current) {
      return;
    }
    confirmStartedRef.current = true;
    onConfirm();
  };

  return (
    <div
      className="modal-backdrop destructive-action-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          requestCancel();
        }
      }}
    >
      <section
        ref={dialogRef}
        className="destructive-action-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-busy={busy}
        aria-labelledby="destructive-action-title"
        aria-describedby="destructive-action-description"
      >
        <header className="destructive-action-header">
          <AlertTriangle size={18} aria-hidden="true" />
          <h2 id="destructive-action-title">{title}</h2>
        </header>
        <div className="destructive-action-body">
          <p id="destructive-action-description">{description}</p>
          <div className="destructive-action-target" title={target}>{target}</div>
          <div className="destructive-action-actions">
            <button type="button" disabled={busy} onClick={requestCancel}>{cancelLabel}</button>
            <button type="button" className="danger-action" disabled={busy} onClick={requestConfirm}>
              {busy ? <Loader2 size={15} aria-hidden="true" /> : null}
              {busy ? busyLabel : confirmLabel}
            </button>
          </div>
        </div>
      </section>
    </div>
  );
}
