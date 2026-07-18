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
  WorkspaceMetadataState,
} from "./types";
import { WorkspaceTransport, type WorkspaceCommand } from "./workspace-transport";
import { bytesToBase64 } from "./wire";

interface TransportLike {
  onMetadata?: (data: unknown) => void;
  onTerminal?: (data: unknown) => void;
  onMetadataClose?: () => void;
  onTerminalClose?: () => void;
  connectMetadata(): Promise<unknown>;
  reconnectMetadata(): Promise<unknown>;
  sendMetadata?: (data: string) => void;
  openTerminal(command: WorkspaceCommand): Promise<unknown>;
  sendTerminal(data: string | ArrayBufferLike | Blob | ArrayBufferView): void;
  closeTerminal(): void;
  close(): void;
}

type JsonRequest = (path: string, payload: unknown) => Promise<any>;
type HttpRequest = (path: string, init?: RequestInit) => Promise<Response>;
export type PushApiPath = "/api/push/config" | "/api/push/subscription";

interface PendingTerminalOpen {
  sessionId?: UUID;
  promise: Promise<any>;
  resolve: (payload: any) => void;
  reject: (error: unknown) => void;
}
interface PendingMetadataPing {
  promise: Promise<number>;
  timeout: ReturnType<typeof globalThis.setTimeout>;
  resolve: (rttMs: number) => void;
  reject: (error: unknown) => void;
}
interface PendingTerminalActivity {
  sessionId: UUID;
  promise: Promise<void>;
  timeout: ReturnType<typeof globalThis.setTimeout>;
  resolve: () => void;
  reject: (error: unknown) => void;
}
const FILE_CHUNK_BYTES = 2 * 1024 * 1024;
const DEFAULT_REQUEST_TIMEOUT_MS = 5_000;
const METADATA_RECONNECT_BASE_DELAY_MS = 100;
const METADATA_RECONNECT_MAX_DELAY_MS = 2_000;

function isRawFileTransferRequest(path: string, method?: string): boolean {
  const normalizedMethod = (method ?? "GET").toUpperCase();
  return (
    normalizedMethod === "PUT" && /^\/api\/files\/uploads\/[^/]+\/chunks$/.test(path)
  ) || (
    normalizedMethod === "GET" && /^\/api\/files\/downloads\/[^/]+$/.test(path)
  );
}

async function readBlobArrayBuffer(blob: Blob): Promise<ArrayBuffer> {
  if (typeof blob.arrayBuffer === "function") {
    return blob.arrayBuffer();
  }
  return new Promise<ArrayBuffer>((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("failed to read upload blob"));
    reader.onload = () => resolve(reader.result as ArrayBuffer);
    reader.readAsArrayBuffer(blob);
  });
}

export type MetadataDeliveryKind = "snapshot" | "update";

export class V070Client {
  readonly serverId: UUID;
  readonly deviceId: UUID;
  isClosed = false;
  private metadataState?: WorkspaceMetadataState;
  private metadataRevision?: number;
  private metadataConnected = false;
  private metadataFailure?: ProtocolClientError;
  private metadataResync?: Promise<void>;
  private metadataConnectionGeneration = 0;
  private metadataReconnectAttempt = 0;
  private metadataReconnectNeeded = false;
  private metadataReconnectTimer?: ReturnType<typeof globalThis.setTimeout>;
  private metadataWaiters: Array<{ resolve: () => void; reject: (error: unknown) => void }> = [];
  private metadataPingWaiters = new Map<number, PendingMetadataPing>();
  private metadataListeners = new Set<(
    revision: number,
    state: WorkspaceMetadataState,
    deliveryKind: MetadataDeliveryKind,
  ) => void>();
  private terminalSessionId?: UUID;
  private terminalOpen?: PendingTerminalOpen;
  private terminalGeneration = 0;
  private terminalActivityProbe?: PendingTerminalActivity;
  private pendingTerminalFrames: Uint8Array[] = [];
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
    private readonly requestTimeoutMs = DEFAULT_REQUEST_TIMEOUT_MS,
  ) {
    this.serverId = server.server_id;
    this.deviceId = device.device_id;
    this.tokens = new AccessTokenManager(server, device);
    this.transport = transport ?? new WorkspaceTransport(server.url, this.tokens);
    this.jsonRequest = request ?? ((path, payload) => this.requestJson(path, payload));
    this.httpRequest = httpRequest ?? ((path, init) => this.requestAuthorized(path, init));
    this.transport.onMetadata = (data) => this.handleMetadata(data);
    this.transport.onTerminal = (data) => this.handleTerminal(data);
    this.transport.onMetadataClose = () => this.handleMetadataClose();
    this.transport.onTerminalClose = () => this.handleTerminalClose();
    this.tokens.onRefresh(() => this.reconnectWithRefreshedToken());
  }

  static async connect(server: PairedServerState, device: DeviceState): Promise<V070Client> {
    return new V070Client(server, device);
  }

  async authenticate(): Promise<void> {}

  async subscribeMetadata(): Promise<void> { await this.ensureMetadata(); }

  watchMetadata(listener: (
    revision: number,
    state: WorkspaceMetadataState,
    deliveryKind: MetadataDeliveryKind,
  ) => void): () => void {
    this.metadataListeners.add(listener);
    if (this.metadataRevision !== undefined && this.metadataState !== undefined) {
      listener(this.metadataRevision, this.metadataState, "snapshot");
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
    return this.metadataState?.daemon ?? ({} as DaemonStatusResultPayload);
  }

  async measureLatency(): Promise<number> {
    await this.ensureMetadata();
    const timestampMs = Date.now();
    if (!Number.isSafeInteger(timestampMs) || timestampMs < 0) {
      throw new ProtocolClientError("invalid_state", "local timestamp is invalid");
    }
    const inFlight = this.metadataPingWaiters.get(timestampMs);
    if (inFlight) return inFlight.promise;
    let resolve!: (rttMs: number) => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<number>((resolvePending, rejectPending) => {
      resolve = resolvePending;
      reject = rejectPending;
    });
    let waiter!: PendingMetadataPing;
    const timeout = globalThis.setTimeout(() => {
      if (!this.removeMetadataPingWaiter(timestampMs, waiter)) return;
      reject(new ProtocolClientError("response_timeout", "metadata ping timed out"));
    }, Math.max(1, this.requestTimeoutMs));
    waiter = { promise, timeout, resolve, reject };
    this.metadataPingWaiters.set(timestampMs, waiter);
    try {
      if (!this.transport.sendMetadata) {
        throw new ProtocolClientError("connection_closed", "metadata websocket cannot send");
      }
      this.transport.sendMetadata(JSON.stringify({
        type: "metadata.ping",
        payload: { timestamp_ms: timestampMs },
      }));
    } catch (caught) {
      if (this.removeMetadataPingWaiter(timestampMs, waiter)) {
        globalThis.clearTimeout(timeout);
        reject(caught);
      }
    }
    return promise;
  }

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

  probeTerminalLiveness(sessionId: UUID, size: TerminalSize, timeoutMs: number): Promise<void> {
    this.requireTerminal(sessionId);
    const inFlight = this.terminalActivityProbe;
    if (inFlight?.sessionId === sessionId) return inFlight.promise;
    let resolve!: () => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<void>((resolvePending, rejectPending) => {
      resolve = resolvePending;
      reject = rejectPending;
    });
    let probe!: PendingTerminalActivity;
    const timeout = globalThis.setTimeout(() => {
      if (this.terminalActivityProbe !== probe) return;
      this.terminalActivityProbe = undefined;
      reject(new ProtocolClientError("response_timeout", "terminal liveness probe timed out"));
    }, Math.max(1, timeoutMs));
    probe = { sessionId, promise, timeout, resolve, reject };
    this.terminalActivityProbe = probe;
    void this.requestSessionResize(sessionId, size).catch((caught) => {
      this.rejectTerminalActivityProbe(caught, probe);
    });
    return promise;
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
    if (this.terminalSessionId !== sessionId && this.terminalOpen?.sessionId !== sessionId) return;
    this.resetTerminalState(new ProtocolClientError("connection_closed", "terminal connection closed"));
    this.transport.closeTerminal();
  }

  async closeSession(sessionId: UUID): Promise<SessionClosedPayload> {
    if (this.terminalSessionId === sessionId || this.terminalOpen?.sessionId === sessionId) {
      this.resetTerminalState(new ProtocolClientError("connection_closed", "terminal connection closed"));
      this.transport.closeTerminal();
    }
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

  async requestPush(path: PushApiPath, init: RequestInit = {}): Promise<Response> {
    return this.httpRequest(path, init);
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

  async getSessionGitDiff(sessionId: UUID, worktreePath: string, filePath?: string | null, staged = false): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/git_diff`, { worktree_path: worktreePath, file_path: filePath, staged });
  }

  async writeSessionFile(sessionId: UUID, path: string, data: Uint8Array): Promise<any> {
    return this.jsonRequest(`/api/control/session/${sessionId}/file_write`, {
      path,
      data_base64: bytesToBase64(data),
    });
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
      signal?: AbortSignal;
    } = {},
  ): Promise<any> {
    const ready = await this.fileJson("/api/files/uploads", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_id: sessionId, path, size_bytes: file.size }),
      signal: options.signal,
    });
    try {
      if (file.size === 0) {
        const progress = await this.fileJson(`/api/files/uploads/${ready.upload_id}/chunks`, {
          method: "PUT",
          headers: { "content-range": "bytes */0" },
          body: new Uint8Array(),
          signal: options.signal,
        });
        options.onProgress?.(progress);
      } else {
        for (let offset = 0; offset < file.size; offset += FILE_CHUNK_BYTES) {
          const end = Math.min(file.size, offset + FILE_CHUNK_BYTES);
          const bytes = await readBlobArrayBuffer(file.slice(offset, end));
          options.onSentProgress?.(end, file.size);
          const progress = await this.fileJson(`/api/files/uploads/${ready.upload_id}/chunks`, {
            method: "PUT",
            headers: { "content-range": `bytes ${offset}-${end - 1}/${file.size}` },
            body: bytes,
            signal: options.signal,
          });
          options.onProgress?.(progress);
        }
      }
      return await this.fileJson(`/api/files/uploads/${ready.upload_id}/commit`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{}",
        signal: options.signal,
      });
    } catch (error) {
      await this.fileJson(`/api/files/uploads/${ready.upload_id}/abort`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{}",
        signal: options.signal,
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
      signal?: AbortSignal;
    } = {},
  ): Promise<any> {
    const ready = await this.fileJson("/api/files/downloads", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_id: sessionId, path }),
      signal: options.signal,
    });
    const response = await this.httpRequest(`/api/files/downloads/${ready.download_id}`, {
      method: "GET",
      signal: options.signal,
    });
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
    this.metadataConnectionGeneration += 1;
    this.metadataConnected = false;
    this.metadataReconnectNeeded = false;
    this.clearMetadataReconnectTimer();
    const error = new ProtocolClientError("connection_closed", "connection closed");
    this.rejectMetadataWaiters(error);
    this.rejectMetadataPingWaiters(error);
    this.resetTerminalState(new ProtocolClientError("connection_closed", "connection closed"));
    this.transport.close();
    this.interruptReceiveWaiters();
  }

  private async ensureMetadata(): Promise<void> {
    if (this.isClosed) throw new ProtocolClientError("connection_closed", "connection closed");
    const generation = this.metadataConnectionGeneration;
    await this.transport.connectMetadata();
    if (this.isClosed) throw new ProtocolClientError("connection_closed", "connection closed");
    if (generation !== this.metadataConnectionGeneration) {
      throw new ProtocolClientError("stale_connection", "metadata connection was superseded");
    }
    if (this.metadataFailure) throw this.metadataFailure;
    this.metadataConnected = true;
    if (this.metadataState !== undefined) return;
    await new Promise<void>((resolve, reject) => this.metadataWaiters.push({ resolve, reject }));
  }

  private openTerminal(command: WorkspaceCommand): Promise<any> {
    if (this.isClosed) {
      return Promise.reject(new ProtocolClientError("connection_closed", "connection closed"));
    }
    this.resetTerminalState(new ProtocolClientError("stale_connection", "terminal connection was superseded"));
    let resolve!: (payload: any) => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<any>((resolvePending, rejectPending) => {
      resolve = resolvePending;
      reject = rejectPending;
    });
    const pending: PendingTerminalOpen = {
      sessionId: command.type === "terminal.attach"
        ? (command.payload as { session_id: UUID }).session_id
        : undefined,
      promise,
      resolve,
      reject,
    };
    this.terminalOpen = pending;
    try {
      void this.transport.openTerminal(command).catch((caught) => {
        if (this.terminalOpen === pending) {
          this.terminalOpen = undefined;
          this.pendingTerminalFrames = [];
        }
        pending.reject(caught);
      });
    } catch (caught) {
      this.terminalOpen = undefined;
      pending.reject(caught);
    }
    return promise;
  }

  private reconnectWithRefreshedToken(): void {
    if (this.isClosed) return;
    const reconnects: Promise<unknown>[] = [];
    if (this.metadataConnected) {
      this.metadataState = undefined;
      this.metadataRevision = undefined;
      this.metadataConnected = false;
      this.requestMetadataReconnect();
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
    if (message.type === "error") {
      const error = new ProtocolClientError(
        message.payload?.code ?? "metadata_error",
        message.payload?.message ?? "metadata error",
      );
      this.metadataFailure = error;
      this.rejectMetadataWaiters(error);
      this.rejectMetadataPingWaiters(error);
      return;
    }
    if (message.type === "metadata.pong") {
      const timestampMs = message.payload?.timestamp_ms;
      if (!Number.isSafeInteger(timestampMs) || timestampMs < 0) return;
      const waiter = this.takeMetadataPingWaiter(timestampMs);
      if (!waiter) return;
      globalThis.clearTimeout(waiter.timeout);
      waiter.resolve(Math.max(0, Date.now() - timestampMs));
      return;
    }
    const revision = message.payload?.revision;
    if (!Number.isSafeInteger(revision) || revision < 0) return;
    if (message.type === "metadata.update") {
      if (this.metadataRevision !== undefined && revision <= this.metadataRevision) return;
      if (this.metadataRevision === undefined || revision !== this.metadataRevision + 1) {
        this.metadataState = undefined;
        this.metadataRevision = undefined;
        this.metadataConnected = false;
        this.requestMetadataReconnect();
        return;
      }
    } else if (message.type !== "metadata.snapshot") {
      return;
    }
    const metadataState = (message.payload?.state ?? {}) as WorkspaceMetadataState;
    this.metadataRevision = revision;
    this.metadataState = metadataState;
    this.metadataFailure = undefined;
    this.metadataConnected = true;
    this.metadataReconnectAttempt = 0;
    this.metadataReconnectNeeded = false;
    this.clearMetadataReconnectTimer();
    for (const waiter of this.metadataWaiters.splice(0)) waiter.resolve();
    const deliveryKind = message.type === "metadata.snapshot" ? "snapshot" : "update";
    for (const listener of this.metadataListeners) listener(revision, metadataState, deliveryKind);
  }

  private handleTerminal(data: unknown): void {
    if (typeof data === "string") {
      const message = JSON.parse(data) as any;
      if (message.type === "terminal.created" || message.type === "terminal.attached") {
        this.resolveTerminalActivityProbe();
        const pending = this.terminalOpen;
        if (!pending || this.isClosed) return;
        this.terminalSessionId = message.payload.session_id;
        this.terminalOpen = undefined;
        pending.resolve(message.payload);
        for (const bytes of this.pendingTerminalFrames.splice(0)) {
          this.enqueue({ type: "attach_frame", payload: buildAttachFramePayload(this.terminalSessionId!, bytes) });
        }
      } else if (message.type === "error") {
        const error = new ProtocolClientError(
          message.payload?.code ?? "terminal_error",
          message.payload?.message ?? "terminal error",
        );
        this.rejectTerminalActivityProbe(error);
        if (this.terminalOpen) {
          const pending = this.terminalOpen;
          this.terminalOpen = undefined;
          this.pendingTerminalFrames = [];
          pending.reject(error);
        } else {
          this.enqueue({ type: "error", payload: { code: error.code, message: error.message } });
        }
      }
      return;
    }
    if (data instanceof Blob) {
      const generation = this.terminalGeneration;
      this.terminalBlobDecode = this.terminalBlobDecode
        .then(() => new Response(data).arrayBuffer())
        .then((buffer) => {
          if (!this.isClosed && generation === this.terminalGeneration) this.handleTerminal(buffer);
        });
      return;
    }
    const bytes = data instanceof ArrayBuffer || Object.prototype.toString.call(data) === "[object ArrayBuffer]"
      ? new Uint8Array(data as ArrayBuffer)
      : ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : undefined;
    if (!bytes) return;
    this.resolveTerminalActivityProbe();
    if (!this.terminalSessionId) {
      if (this.terminalOpen) this.pendingTerminalFrames.push(bytes.slice());
      return;
    }
    this.enqueue({ type: "attach_frame", payload: buildAttachFramePayload(this.terminalSessionId, bytes) });
  }

  private handleMetadataClose(): void {
    if (this.isClosed) return;
    this.metadataConnectionGeneration += 1;
    this.metadataConnected = false;
    this.metadataState = undefined;
    this.metadataRevision = undefined;
    const error = this.metadataFailure
      ?? new ProtocolClientError("connection_closed", "metadata connection closed");
    this.rejectMetadataWaiters(error);
    this.rejectMetadataPingWaiters(error);
    this.requestMetadataReconnect();
  }

  private requestMetadataReconnect(): void {
    if (this.isClosed) return;
    this.rejectMetadataPingWaiters(
      new ProtocolClientError("stale_connection", "metadata connection was superseded"),
    );
    this.metadataReconnectNeeded = true;
    if (this.metadataResync || this.metadataReconnectTimer !== undefined) return;
    this.metadataReconnectNeeded = false;
    const generation = ++this.metadataConnectionGeneration;
    let opening: Promise<unknown>;
    try {
      opening = this.transport.reconnectMetadata();
    } catch (caught) {
      opening = Promise.reject(caught);
    }
    const reconnect = opening.then(
      () => {
        if (this.isClosed || generation !== this.metadataConnectionGeneration) return;
        this.metadataConnected = true;
        if (this.metadataState === undefined) {
          this.metadataReconnectNeeded = true;
          this.scheduleMetadataReconnect();
        }
      },
      () => {
        if (this.isClosed || generation !== this.metadataConnectionGeneration) return;
        if (this.metadataState !== undefined) return;
        this.metadataConnected = false;
        this.metadataReconnectNeeded = true;
        this.scheduleMetadataReconnect();
      },
    ).then(() => undefined);
    this.metadataResync = reconnect;
    void reconnect.then(() => {
      if (this.metadataResync !== reconnect) return;
      this.metadataResync = undefined;
      if (this.metadataReconnectNeeded && this.metadataReconnectTimer === undefined) {
        this.requestMetadataReconnect();
      }
    });
  }

  private scheduleMetadataReconnect(): void {
    if (this.isClosed || this.metadataReconnectTimer !== undefined) return;
    const exponent = Math.min(this.metadataReconnectAttempt, 5);
    const delay = Math.min(
      METADATA_RECONNECT_BASE_DELAY_MS * (2 ** exponent),
      METADATA_RECONNECT_MAX_DELAY_MS,
    );
    this.metadataReconnectAttempt += 1;
    this.metadataReconnectTimer = globalThis.setTimeout(() => {
      this.metadataReconnectTimer = undefined;
      this.requestMetadataReconnect();
    }, delay);
  }

  private clearMetadataReconnectTimer(): void {
    if (this.metadataReconnectTimer === undefined) return;
    globalThis.clearTimeout(this.metadataReconnectTimer);
    this.metadataReconnectTimer = undefined;
  }

  private rejectMetadataWaiters(error: ProtocolClientError): void {
    for (const waiter of this.metadataWaiters.splice(0)) waiter.reject(error);
  }

  private takeMetadataPingWaiter(timestampMs: number): PendingMetadataPing | undefined {
    const waiter = this.metadataPingWaiters.get(timestampMs);
    if (waiter) this.metadataPingWaiters.delete(timestampMs);
    return waiter;
  }

  private removeMetadataPingWaiter(timestampMs: number, waiter: PendingMetadataPing): boolean {
    if (this.metadataPingWaiters.get(timestampMs) !== waiter) return false;
    this.metadataPingWaiters.delete(timestampMs);
    return true;
  }

  private rejectMetadataPingWaiters(error: ProtocolClientError): void {
    const waiters = [...this.metadataPingWaiters.values()];
    this.metadataPingWaiters.clear();
    for (const waiter of waiters) {
      globalThis.clearTimeout(waiter.timeout);
      waiter.reject(error);
    }
  }

  private handleTerminalClose(): void {
    if (this.isClosed) return;
    const hadTerminal = this.terminalSessionId !== undefined || this.terminalOpen !== undefined;
    this.resetTerminalState(new ProtocolClientError("connection_closed", "terminal connection closed"));
    if (!hadTerminal) return;
    this.interruptReceiveWaiters();
  }

  private resetTerminalState(error?: ProtocolClientError): void {
    this.terminalGeneration += 1;
    if (error) this.rejectTerminalActivityProbe(error);
    this.terminalSessionId = undefined;
    this.pendingTerminalFrames = [];
    if (!error || !this.terminalOpen) return;
    const pending = this.terminalOpen;
    this.terminalOpen = undefined;
    pending.reject(error);
  }

  private resolveTerminalActivityProbe(): void {
    const probe = this.terminalActivityProbe;
    if (!probe || probe.sessionId !== this.terminalSessionId) return;
    this.terminalActivityProbe = undefined;
    globalThis.clearTimeout(probe.timeout);
    probe.resolve();
  }

  private rejectTerminalActivityProbe(error: unknown, expected?: PendingTerminalActivity): void {
    const probe = this.terminalActivityProbe;
    if (!probe || (expected && probe !== expected)) return;
    this.terminalActivityProbe = undefined;
    globalThis.clearTimeout(probe.timeout);
    probe.reject(error);
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
    const response = await this.fetchWithTimeout(applicationHttpUrl(this.server.url, path), {
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
    const input = applicationHttpUrl(this.server.url, path);
    const request = { ...init, headers };
    if (isRawFileTransferRequest(path, init.method)) {
      return fetch(input, request);
    }
    return this.fetchWithTimeout(input, request);
  }

  private async fetchWithTimeout(input: string, init: RequestInit): Promise<Response> {
    const controller = new AbortController();
    const callerSignal = init.signal;
    let timedOut = false;
    const abortFromCaller = () => controller.abort(callerSignal?.reason);
    if (callerSignal?.aborted) {
      abortFromCaller();
    } else {
      callerSignal?.addEventListener("abort", abortFromCaller, { once: true });
    }
    const timeout = globalThis.setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, Math.max(1, this.requestTimeoutMs));
    try {
      return await fetch(input, { ...init, signal: controller.signal });
    } catch (caught) {
      if (timedOut) {
        throw new ProtocolClientError("response_timeout", "request timed out");
      }
      throw caught;
    } finally {
      globalThis.clearTimeout(timeout);
      callerSignal?.removeEventListener("abort", abortFromCaller);
    }
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
