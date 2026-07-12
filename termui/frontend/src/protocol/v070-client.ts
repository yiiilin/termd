import { AccessTokenManager, applicationHttpUrl } from "./access-token";
import { ProtocolClientError } from "./errors";
import { buildAttachFramePayload, encodeSupervisorTerminalClientFrame } from "./supervisor-terminal";
import type {
  DaemonClientsResultPayload,
  DaemonStatusResultPayload,
  DeviceState,
  Envelope,
  PairedServerState,
  SessionAttachedPayload,
  SessionClosedPayload,
  SessionCreatedPayload,
  SessionListResultPayload,
  SessionResizedPayload,
  TerminalSize,
  UUID,
} from "./types";
import { WorkspaceTransport, type WorkspaceCommand } from "./workspace-transport";

interface TransportLike {
  onMetadata?: (data: unknown) => void;
  onTerminal?: (data: unknown) => void;
  connectMetadata(): Promise<unknown>;
  reconnectMetadata(): Promise<unknown>;
  openTerminal(command: WorkspaceCommand): Promise<unknown>;
  sendTerminal(data: string | ArrayBufferLike | Blob | ArrayBufferView): void;
  closeTerminal(): void;
  close(): void;
}

type JsonRequest = (path: string, payload: unknown) => Promise<any>;
type HttpRequest = (path: string, init?: RequestInit) => Promise<Response>;
const FILE_CHUNK_BYTES = 2 * 1024 * 1024;

export class V070Client {
  readonly serverId: UUID;
  readonly deviceId: UUID;
  isClosed = false;
  private metadataState?: any;
  private metadataRevision?: number;
  private metadataConnected = false;
  private metadataResync?: Promise<void>;
  private metadataWaiters: Array<() => void> = [];
  private metadataListeners = new Set<(revision: number, state: any) => void>();
  private terminalSessionId?: UUID;
  private terminalOpen?: { resolve: (payload: any) => void; reject: (error: unknown) => void };
  private terminalBlobDecode = Promise.resolve();
  private receiveQueue: Envelope[] = [];
  private receiveWaiters: Array<{ resolve: (value: Envelope) => void; reject: (error: unknown) => void }> = [];
  private readonly tokens: AccessTokenManager;
  private readonly transport: TransportLike;
  private readonly jsonRequest: JsonRequest;
  private readonly httpRequest: HttpRequest;

  constructor(
    readonly server: PairedServerState,
    readonly device: DeviceState,
    transport?: TransportLike,
    request?: JsonRequest,
    httpRequest?: HttpRequest,
  ) {
    this.serverId = server.server_id;
    this.deviceId = device.device_id;
    this.tokens = new AccessTokenManager(server, device);
    this.transport = transport ?? new WorkspaceTransport(server.url, this.tokens);
    this.jsonRequest = request ?? ((path, payload) => this.requestJson(path, payload));
    this.httpRequest = httpRequest ?? ((path, init) => this.requestAuthorized(path, init));
    this.transport.onMetadata = (data) => this.handleMetadata(data);
    this.transport.onTerminal = (data) => this.handleTerminal(data);
    this.tokens.onRefresh(() => this.reconnectWithRefreshedToken());
  }

  static async connect(server: PairedServerState, device: DeviceState): Promise<V070Client> {
    return new V070Client(server, device);
  }

  async authenticate(): Promise<void> {}

  async subscribeMetadata(): Promise<void> { await this.ensureMetadata(); }

  watchMetadata(listener: (revision: number, state: any) => void): () => void {
    this.metadataListeners.add(listener);
    if (this.metadataRevision !== undefined && this.metadataState !== undefined) {
      listener(this.metadataRevision, this.metadataState);
    }
    return () => this.metadataListeners.delete(listener);
  }

  async listSessions(): Promise<SessionListResultPayload> {
    await this.ensureMetadata();
    return { sessions: this.metadataState?.sessions ?? [] };
  }

  async listDaemonClients(): Promise<DaemonClientsResultPayload> {
    await this.ensureMetadata();
    return { clients: this.metadataState?.clients ?? [] };
  }

  async getDaemonStatus(): Promise<DaemonStatusResultPayload> {
    await this.ensureMetadata();
    return this.metadataState?.daemon ?? {};
  }

  async measureLatency(): Promise<number> { await this.ensureMetadata(); return this.metadataState?.rtt_ms ?? 0; }

  async createSession(command: string[], size: TerminalSize): Promise<SessionCreatedPayload> {
    return this.openTerminal({ type: "terminal.create", payload: { command, size } });
  }

  async attachSession(sessionId: UUID): Promise<SessionAttachedPayload> {
    return this.openTerminal({ type: "terminal.attach", payload: { session_id: sessionId } });
  }

  async sendSessionData(sessionId: UUID, bytes: Uint8Array): Promise<void> {
    this.requireTerminal(sessionId);
    this.transport.sendTerminal(encodeSupervisorTerminalClientFrame({ type: "input", data_bytes: bytes }));
  }

  async requestSessionResize(sessionId: UUID, size: TerminalSize): Promise<void> {
    this.requireTerminal(sessionId);
    this.transport.sendTerminal(encodeSupervisorTerminalClientFrame({ type: "resize", size }));
  }

  async resizeSession(sessionId: UUID, size: TerminalSize): Promise<SessionResizedPayload> {
    await this.requestSessionResize(sessionId, size);
    return { session_id: sessionId, size } as SessionResizedPayload;
  }

  sendSupervisorTerminalHeartbeatPong(sessionId: UUID, nonce: string): void {
    this.requireTerminal(sessionId);
    this.transport.sendTerminal(encodeSupervisorTerminalClientFrame({ type: "heartbeat_pong", nonce }));
  }

  detachSession(sessionId: UUID): void {
    if (this.terminalSessionId === sessionId) {
      this.transport.closeTerminal();
      this.terminalSessionId = undefined;
    }
  }

  async closeSession(sessionId: UUID): Promise<SessionClosedPayload> {
    this.transport.closeTerminal();
    if (this.terminalSessionId === sessionId) this.terminalSessionId = undefined;
    return this.jsonRequest(`/api/control/session/${sessionId}/close`, {});
  }

  async renameSession(sessionId: UUID, name: string): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/rename`, { name });
  }

  async reorderSessions(sessionIds: UUID[]): Promise<any> {
    return this.jsonRequest("/api/control/session/reorder", { session_ids: sessionIds });
  }

  async forgetDaemonClient(deviceId: UUID): Promise<any> {
    return this.jsonRequest("/api/control/daemon/client_forget", { device_id: deviceId });
  }

  async listSessionFiles(sessionId: UUID, path?: string): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/files`, path ? { path } : {});
  }

  async getSessionGit(sessionId: UUID): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/git`, {});
  }

  async applySessionGitAction(sessionId: UUID, worktreePath: string, filePath: string, action: string): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/git_action`, { worktree_path: worktreePath, file_path: filePath, action });
  }

  async searchSessionOutput(sessionId: UUID, query: string, options: { caseSensitive?: boolean; maxResults?: number } = {}): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/search`, { query, case_sensitive: Boolean(options.caseSensitive), max_results: options.maxResults ?? 80 });
  }

  async getSessionGitDiff(sessionId: UUID, worktreePath: string, filePath?: string | null, staged = false): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/git_diff`, { worktree_path: worktreePath, file_path: filePath, staged });
  }

  async writeSessionFile(sessionId: UUID, path: string, data: Uint8Array): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/file_write`, { path, data: Array.from(data) });
  }

  async deleteSessionFile(sessionId: UUID, path: string): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/file_delete`, { path });
  }

  async readSessionFile(sessionId: UUID, path: string, options: { maxBytes?: number } = {}): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/file_read`, {
      path,
      max_bytes: options.maxBytes ?? 1024 * 1024,
    });
  }

  async uploadSessionFile(
    sessionId: UUID,
    path: string,
    file: Blob,
    options: {
      onProgress?: (progress: any) => void;
      onSentProgress?: (sentBytes: number, sizeBytes: number) => void;
    } = {},
  ): Promise<any> {
    const ready = await this.fileJson("/api/files/uploads", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_id: sessionId, path, size_bytes: file.size }),
    });
    try {
      if (file.size === 0) {
        const progress = await this.fileJson(`/api/files/uploads/${ready.upload_id}/chunks`, {
          method: "PUT",
          headers: { "content-range": "bytes */0" },
          body: new Uint8Array(),
        });
        options.onProgress?.(progress);
      } else {
        for (let offset = 0; offset < file.size; offset += FILE_CHUNK_BYTES) {
          const end = Math.min(file.size, offset + FILE_CHUNK_BYTES);
          const bytes = await new Response(file.slice(offset, end)).arrayBuffer();
          options.onSentProgress?.(end, file.size);
          const progress = await this.fileJson(`/api/files/uploads/${ready.upload_id}/chunks`, {
            method: "PUT",
            headers: { "content-range": `bytes ${offset}-${end - 1}/${file.size}` },
            body: bytes,
          });
          options.onProgress?.(progress);
        }
      }
      return await this.fileJson(`/api/files/uploads/${ready.upload_id}/commit`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{}",
      });
    } catch (error) {
      await this.fileJson(`/api/files/uploads/${ready.upload_id}/abort`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{}",
      }).catch(() => undefined);
      throw error;
    }
  }

  async downloadSessionFile(
    sessionId: UUID,
    path: string,
    options: {
      onProgress?: (receivedBytes: number, sizeBytes: number) => void;
      onChunk?: (bytes: Uint8Array, receivedBytes: number, sizeBytes: number) => void | Promise<void>;
      collectBytes?: boolean;
    } = {},
  ): Promise<any> {
    const ready = await this.fileJson("/api/files/downloads", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_id: sessionId, path }),
    });
    const response = await this.httpRequest(`/api/files/downloads/${ready.download_id}`, { method: "GET" });
    if (!response.ok) throw await this.responseError(response);
    const chunks: Uint8Array[] = [];
    let received = 0;
    const reader = response.body?.getReader();
    if (reader) {
      while (true) {
        const next = await reader.read();
        if (next.done) break;
        const bytes = next.value;
        received += bytes.byteLength;
        if (options.collectBytes ?? true) chunks.push(bytes);
        await options.onChunk?.(bytes, received, ready.size_bytes);
        options.onProgress?.(received, ready.size_bytes);
      }
    }
    if (received !== ready.size_bytes) {
      throw new ProtocolClientError("invalid_file_transfer", "file download ended before all bytes arrived");
    }
    const combined = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.byteLength, 0));
    let offset = 0;
    for (const chunk of chunks) {
      combined.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return {
      path: ready.path,
      name: ready.path?.split("/").filter(Boolean).at(-1) ?? "download",
      bytes: combined,
      size_bytes: ready.size_bytes,
      modified_at_ms: ready.modified_at_ms,
    };
  }

  async receiveInner(): Promise<Envelope> {
    const queued = this.receiveQueue.shift();
    if (queued) return queued;
    return new Promise((resolve, reject) => this.receiveWaiters.push({ resolve, reject }));
  }

  interruptReceiveWaiters(): void {
    const error = new ProtocolClientError("connection_closed", "connection closed");
    for (const waiter of this.receiveWaiters.splice(0)) waiter.reject(error);
  }

  close(): void {
    if (this.isClosed) return;
    this.isClosed = true;
    this.tokens.dispose();
    this.transport.close();
    this.interruptReceiveWaiters();
  }

  private async ensureMetadata(): Promise<void> {
    if (this.isClosed) throw new ProtocolClientError("connection_closed", "connection closed");
    await this.transport.connectMetadata();
    this.metadataConnected = true;
    if (this.metadataState) return;
    await new Promise<void>((resolve) => this.metadataWaiters.push(resolve));
  }

  private async openTerminal(command: WorkspaceCommand): Promise<any> {
    const result = new Promise<any>((resolve, reject) => { this.terminalOpen = { resolve, reject }; });
    try {
      await this.transport.openTerminal(command);
    } catch (caught) {
      this.terminalOpen?.reject(caught);
      this.terminalOpen = undefined;
      throw caught;
    }
    return result;
  }

  private reconnectWithRefreshedToken(): void {
    if (this.isClosed) return;
    const reconnects: Promise<unknown>[] = [];
    if (this.metadataConnected) {
      this.metadataState = undefined;
      this.metadataRevision = undefined;
      reconnects.push(this.transport.reconnectMetadata());
    }
    if (this.terminalSessionId) {
      reconnects.push(this.openTerminal({
        type: "terminal.attach",
        payload: { session_id: this.terminalSessionId },
      }));
    }
    void Promise.all(reconnects).catch(() => this.interruptReceiveWaiters());
  }

  private handleMetadata(data: unknown): void {
    if (typeof data !== "string") return;
    let message: any;
    try {
      message = JSON.parse(data);
    } catch {
      return;
    }
    const revision = message.payload?.revision;
    if (!Number.isSafeInteger(revision) || revision < 0) return;
    if (message.type === "metadata.update") {
      if (this.metadataRevision !== undefined && revision <= this.metadataRevision) return;
      if (this.metadataRevision === undefined || revision !== this.metadataRevision + 1) {
        this.metadataState = undefined;
        this.metadataRevision = undefined;
        void this.resyncMetadata();
        return;
      }
    } else if (message.type !== "metadata.snapshot") {
      return;
    }
    this.metadataRevision = revision;
    this.metadataState = message.payload?.state ?? {};
    for (const resolve of this.metadataWaiters.splice(0)) resolve();
    for (const listener of this.metadataListeners) listener(revision, this.metadataState);
  }

  private async resyncMetadata(): Promise<void> {
    if (!this.metadataResync) {
      this.metadataResync = this.transport.reconnectMetadata()
        .then(() => undefined)
        .finally(() => { this.metadataResync = undefined; });
    }
    await this.metadataResync;
  }

  private handleTerminal(data: unknown): void {
    if (typeof data === "string") {
      const message = JSON.parse(data) as any;
      if (message.type === "terminal.created" || message.type === "terminal.attached") {
        this.terminalSessionId = message.payload.session_id;
        this.terminalOpen?.resolve(message.payload);
        this.terminalOpen = undefined;
      } else if (message.type === "error") {
        this.terminalOpen?.reject(new ProtocolClientError(message.payload?.code ?? "terminal_error", message.payload?.message ?? "terminal error"));
        this.terminalOpen = undefined;
      }
      return;
    }
    if (data instanceof Blob) {
      this.terminalBlobDecode = this.terminalBlobDecode
        .then(() => new Response(data).arrayBuffer())
        .then((buffer) => {
          if (!this.isClosed) this.handleTerminal(buffer);
        });
      return;
    }
    const bytes = data instanceof ArrayBuffer || Object.prototype.toString.call(data) === "[object ArrayBuffer]"
      ? new Uint8Array(data as ArrayBuffer)
      : ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : undefined;
    if (!bytes || !this.terminalSessionId) return;
    this.enqueue({ type: "attach_frame", payload: buildAttachFramePayload(this.terminalSessionId, bytes) });
  }

  private enqueue(envelope: Envelope): void {
    const waiter = this.receiveWaiters.shift();
    if (waiter) waiter.resolve(envelope); else this.receiveQueue.push(envelope);
  }

  private requireTerminal(sessionId: UUID): void {
    if (this.terminalSessionId !== sessionId) throw new ProtocolClientError("not_attached", "session is not attached");
  }

  private async requestJson(path: string, payload: unknown): Promise<any> {
    const token = await this.tokens.get();
    const response = await fetch(applicationHttpUrl(this.server.url, path), {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json", "x-termd-server-id": this.server.server_id },
      body: JSON.stringify(payload),
    });
    const body = await response.json();
    if (!response.ok) throw new ProtocolClientError(body?.error?.code ?? "http_error", body?.error?.message ?? "request failed");
    return body;
  }

  private async requestAuthorized(path: string, init: RequestInit = {}): Promise<Response> {
    const token = await this.tokens.get();
    const headers = new Headers(init.headers);
    headers.set("authorization", `Bearer ${token}`);
    headers.set("x-termd-server-id", this.server.server_id);
    return fetch(applicationHttpUrl(this.server.url, path), { ...init, headers });
  }

  private async fileJson(path: string, init: RequestInit): Promise<any> {
    const response = await this.httpRequest(path, init);
    if (!response.ok) throw await this.responseError(response);
    return response.json();
  }

  private async responseError(response: Response): Promise<ProtocolClientError> {
    const body = await response.json().catch(() => undefined) as any;
    return new ProtocolClientError(
      body?.error?.code ?? "http_error",
      body?.error?.message ?? "request failed",
    );
  }
}
