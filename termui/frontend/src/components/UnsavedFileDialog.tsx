import { AlertTriangle, Loader2 } from "lucide-react";
import { useModalFocus } from "./useModalFocus";

interface UnsavedFileDialogProps {
  open: boolean;
  path: string;
  saving?: boolean;
  error?: string;
  title: string;
  description: string;
  saveLabel: string;
  savingLabel: string;
  discardLabel: string;
  stayLabel: string;
  onSave: () => void;
  onDiscard: () => void;
  onStay: () => void;
}

export function UnsavedFileDialog({
  open,
  path,
  saving = false,
  error,
  title,
  description,
  saveLabel,
  savingLabel,
  discardLabel,
  stayLabel,
  onSave,
  onDiscard,
  onStay,
}: UnsavedFileDialogProps) {
  const requestStay = () => {
    if (!saving) {
      onStay();
    }
  };
  const dialogRef = useModalFocus({ open, onClose: requestStay });

  if (!open) {
    return null;
  }

  return (
    <div
      className="file-editor-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          requestStay();
        }
      }}
    >
      <section
        ref={dialogRef}
        className="unsaved-file-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-busy={saving}
        aria-labelledby="unsaved-file-title"
        aria-describedby="unsaved-file-description"
      >
        <header className="unsaved-file-header">
          <div className="unsaved-file-title-group">
            <AlertTriangle size={17} aria-hidden="true" />
            <div>
              <h2 id="unsaved-file-title">{title}</h2>
              <span title={path}>{path}</span>
            </div>
          </div>
        </header>

        <div className="unsaved-file-body">
          <p id="unsaved-file-description">{description}</p>
          {error ? <div className="file-editor-error" role="alert">{error}</div> : null}
          <div className="unsaved-file-actions">
            <button type="button" disabled={saving} onClick={requestStay}>{stayLabel}</button>
            <button type="button" className="danger-action" disabled={saving} onClick={onDiscard}>{discardLabel}</button>
            <button type="button" className="primary-action" disabled={saving} onClick={onSave}>
              {saving ? <Loader2 size={15} aria-hidden="true" /> : null}
              {saving ? savingLabel : saveLabel}
            </button>
          </div>
        </div>
      </section>
    </div>
  );
}
