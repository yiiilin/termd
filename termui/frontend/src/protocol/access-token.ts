import { authPayloadForChallenge, signAuthPayload } from "./auth";
import { ProtocolClientError } from "./errors";
import type { DeviceState, PairedServerState } from "./types";

interface TokenResponse {
  access_token: string;
  expires_at_ms: number;
  refresh_at_ms: number;
}

export function applicationHttpUrl(serverUrl: string, path: string): string {
  const parsed = new URL(serverUrl, globalThis.location?.href);
  parsed.protocol = parsed.protocol === "wss:" ? "https:" : "http:";
  parsed.search = "";
  parsed.hash = "";
  parsed.pathname = parsed.pathname.replace(/\/ws(?:\/(?:metadata|terminal))?\/?$/, "") + path;
  return parsed.toString();
}

export class AccessTokenManager {
  private current?: TokenResponse;
  private pending?: Promise<string>;
  private refreshTimer?: ReturnType<typeof globalThis.setTimeout>;
  private readonly refreshListeners = new Set<(accessToken: string) => void>();

  constructor(private readonly server: PairedServerState, private readonly device: DeviceState) {}

  invalidate(): void {
    this.current = undefined;
    this.clearRefreshTimer();
  }

  onRefresh(listener: (accessToken: string) => void): () => void {
    this.refreshListeners.add(listener);
    return () => this.refreshListeners.delete(listener);
  }

  dispose(): void {
    this.clearRefreshTimer();
    this.refreshListeners.clear();
  }

  async get(nowMs = Date.now()): Promise<string> {
    if (this.current && nowMs < this.current.refresh_at_ms) return this.current.access_token;
    this.pending ??= this.refresh().finally(() => { this.pending = undefined; });
    return this.pending;
  }

  private async refresh(): Promise<string> {
    const certificate = this.server.device_certificate;
    if (!certificate) {
      throw new ProtocolClientError("device_certificate_required", "pair this device again to upgrade its credential");
    }
    const headers = {
      authorization: `TermdDevice ${certificate}`,
      "content-type": "application/json",
      "x-termd-server-id": this.server.server_id,
    };
    const challengeResponse = await fetch(applicationHttpUrl(this.server.url, "/api/auth/challenge"), {
      method: "POST", headers, body: JSON.stringify({ device_id: this.device.device_id }),
    });
    const challengeBody = await challengeResponse.json() as { challenge?: string; error?: { code?: string; message?: string } };
    if (!challengeResponse.ok || !challengeBody.challenge) {
      throw new ProtocolClientError(challengeBody.error?.code ?? "device_challenge_failed", challengeBody.error?.message ?? "device challenge failed");
    }
    const proof = await signAuthPayload(
      authPayloadForChallenge(this.device.device_id, challengeBody.challenge),
      this.server,
      this.device.device_signing_key_secret,
    );
    const response = await fetch(applicationHttpUrl(this.server.url, "/api/auth/access-token"), {
      method: "POST", headers, body: JSON.stringify(proof),
    });
    const body = await response.json() as Partial<TokenResponse> & { error?: { code?: string; message?: string } };
    if (!response.ok || !body.access_token || !body.expires_at_ms) {
      throw new ProtocolClientError(body.error?.code ?? "access_token_failed", body.error?.message ?? "access token exchange failed");
    }
    this.current = {
      access_token: body.access_token,
      expires_at_ms: body.expires_at_ms,
      refresh_at_ms: body.refresh_at_ms ?? body.expires_at_ms - 60_000,
    };
    this.scheduleRefresh(this.current.refresh_at_ms);
    return this.current.access_token;
  }

  private scheduleRefresh(refreshAtMs: number): void {
    this.clearRefreshTimer();
    const delayMs = refreshAtMs - Date.now();
    if (delayMs <= 0) return;
    this.refreshTimer = globalThis.setTimeout(() => {
      this.refreshTimer = undefined;
      this.current = undefined;
      void this.get().then((accessToken) => {
        for (const listener of this.refreshListeners) listener(accessToken);
      }).catch(() => undefined);
    }, delayMs);
  }

  private clearRefreshTimer(): void {
    if (this.refreshTimer !== undefined) {
      globalThis.clearTimeout(this.refreshTimer);
      this.refreshTimer = undefined;
    }
  }
}
