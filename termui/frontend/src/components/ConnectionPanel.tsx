import { KeyRound } from "lucide-react";

interface ConnectionPanelProps {
  url: string;
  token: string;
  status: string;
  onUrlChange: (url: string) => void;
  onTokenChange: (token: string) => void;
  onPair: () => void;
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
      <button type="button" onClick={props.onPair} disabled={props.status === "pairing" || !props.token.trim()}>
        <KeyRound size={16} aria-hidden="true" />
        Pair
      </button>
    </section>
  );
}
