import { AccessTokenManager, applicationHttpUrl } from "./access-token";
import { ProtocolClientError } from "./errors";
import type { DeviceState, PairedServerState, UUID } from "./types";

export type BrowserSessionState = "created" | "running" | "closed";

export interface BrowserSession {
  browser_id: UUID;
  state: BrowserSessionState;
  display_url: string;
  width: number;
  height: number;
  created_at_ms: number;
}

export interface BrowserCreateRequest {
  url: string;
  width: number;
  height: number;
}

interface BrowserSessionsResponse {
  sessions: BrowserSession[];
}

interface ApiErrorResponse {
  error?: {
    code?: string;
    message?: string;
  };
}

export class BrowserWorkspaceClient {
  private readonly tokens: AccessTokenManager;

  constructor(
    readonly server: PairedServerState,
    device: DeviceState,
  ) {
    this.tokens = new AccessTokenManager(server, device);
  }

  dispose(): void {
    this.tokens.dispose();
  }

  accessToken(): Promise<string> {
    return this.tokens.get();
  }

  async list(): Promise<BrowserSession[]> {
    const response = await this.request("/api/browser/sessions", { method: "GET" });
    const body = await responseJson<BrowserSessionsResponse>(response);
    return body.sessions ?? [];
  }

  async create(request: BrowserCreateRequest): Promise<BrowserSession> {
    const response = await this.request("/api/browser/sessions", {
      method: "POST",
      body: JSON.stringify(request),
    });
    return responseJson<BrowserSession>(response);
  }

  async close(browserId: UUID): Promise<void> {
    await this.request(`/api/browser/sessions/${encodeURIComponent(browserId)}`, {
      method: "DELETE",
    });
  }

  private async request(path: string, init: RequestInit): Promise<Response> {
    const makeRequest = async () => {
      const accessToken = await this.tokens.get();
      return fetch(applicationHttpUrl(this.server.url, path), {
        ...init,
        headers: {
          authorization: `Bearer ${accessToken}`,
          "content-type": "application/json",
          "x-termd-server-id": this.server.server_id,
          ...init.headers,
        },
      });
    };

    let response = await makeRequest();
    if (response.status === 401) {
      this.tokens.invalidate();
      response = await makeRequest();
    }
    if (!response.ok) {
      throw await protocolError(response);
    }
    return response;
  }
}

export function browserWebSocketUrl(serverUrl: string, browserId: UUID): string {
  const parsed = new URL(serverUrl, globalThis.location?.href);
  parsed.protocol = parsed.protocol === "https:" ? "wss:" : parsed.protocol === "http:" ? "ws:" : parsed.protocol;
  parsed.search = "";
  parsed.hash = "";
  parsed.pathname = parsed.pathname.replace(/\/ws(?:\/(?:metadata|terminal))?\/?$/, "") +
    `/ws/browser/${encodeURIComponent(browserId)}`;
  return parsed.toString();
}

export function browserViewerPath(serverId: UUID, browserId: UUID): string {
  const query = new URLSearchParams({ server_id: serverId });
  return `/browser/${encodeURIComponent(browserId)}?${query.toString()}`;
}

async function responseJson<T>(response: Response): Promise<T> {
  try {
    return await response.json() as T;
  } catch {
    throw new ProtocolClientError("browser_response_invalid", "browser service returned an invalid response");
  }
}

async function protocolError(response: Response): Promise<ProtocolClientError> {
  let body: ApiErrorResponse = {};
  try {
    body = await response.json() as ApiErrorResponse;
  } catch {
    // The status still provides a stable fallback when an intermediary returned a non-JSON body.
  }
  return new ProtocolClientError(
    body.error?.code ?? "browser_request_failed",
    body.error?.message ?? `browser request failed (${response.status})`,
  );
}
