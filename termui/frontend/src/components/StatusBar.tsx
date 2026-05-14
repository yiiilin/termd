import type { SafeError, UUID } from "../protocol/types";

interface StatusBarProps {
  status: string;
  sessionId?: UUID;
  error?: SafeError;
}

export function StatusBar({ status, sessionId, error }: StatusBarProps) {
  return (
    <footer className="status-bar">
      <span>{status}</span>
      <span>{sessionId ? "attached" : "detached"}</span>
      {error ? (
        <span className="status-error">
          {error.code}: {error.message}
        </span>
      ) : null}
    </footer>
  );
}
