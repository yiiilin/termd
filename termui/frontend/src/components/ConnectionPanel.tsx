import { KeyRound, Save, ScanQrCode, Settings, Wifi } from "lucide-react";

interface ConnectionPanelProps {
  url: string;
  token: string;
  status: string;
  canSaveUrl?: boolean;
  onUrlChange: (url: string) => void;
  onTokenChange: (token: string) => void;
  onPair: () => void;
  onScanQr?: () => void;
  onSaveUrl?: () => void;
}

export function ConnectionPanel(props: ConnectionPanelProps) {
  return (
    <section className="panel" aria-label="connection">
      <label>
        <span>WS URL</span>
        <input
          aria-label="WS URL"
          value={props.url}
          onChange={(event) => props.onUrlChange(event.target.value)}
          spellCheck={false}
        />
      </label>
      <label>
        <span>Pairing token</span>
        <input
          aria-label="Pairing token"
          value={props.token}
          onChange={(event) => props.onTokenChange(event.target.value)}
          spellCheck={false}
          autoComplete="off"
        />
      </label>
      <div className="connection-actions">
        <button type="button" onClick={props.onPair} disabled={props.status === "pairing" || !props.token.trim()}>
          <KeyRound size={16} aria-hidden="true" />
          Pair
        </button>
        {props.onScanQr ? (
          <button type="button" onClick={props.onScanQr} disabled={props.status === "pairing"}>
            <ScanQrCode size={16} aria-hidden="true" />
            Scan QR
          </button>
        ) : null}
        {props.canSaveUrl ? (
          <button
            type="button"
            onClick={props.onSaveUrl}
            disabled={props.status === "saving_url" || !props.url.trim()}
          >
            <Save size={16} aria-hidden="true" />
            Save URL
          </button>
        ) : null}
      </div>
    </section>
  );
}

interface ConnectionStatusPanelProps {
  serverId: string;
  url: string;
  status: string;
  onEdit?: () => void;
}

export function ConnectionStatusPanel(props: ConnectionStatusPanelProps) {
  return (
    <section className="panel connection-status" aria-label="connection status">
      <div className="connection-status-main">
        <Wifi size={16} aria-hidden="true" />
        <strong>{connectionLabel(props.status)}</strong>
        {props.onEdit ? (
          <button type="button" className="icon-button" aria-label="Edit connection" onClick={props.onEdit}>
            <Settings size={15} aria-hidden="true" />
          </button>
        ) : null}
      </div>
      <div className="server-identity">{props.serverId}</div>
      <div className="server-url">{props.url}</div>
    </section>
  );
}

function connectionLabel(status: string): string {
  if (status === "idle" || status === "connecting" || status === "listing") {
    return "Checking connection";
  }
  return "Connected";
}
