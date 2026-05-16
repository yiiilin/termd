import { authPayloadForChallenge, signAuthPayload } from "./auth";
import { E2eeSession, generateE2eeKeyPair } from "./e2ee";
import { ProtocolClientError, protocolError } from "./errors";
import type {
  AuthChallengePayload,
  ClientHelloPayload,
  ControlGrantPayload,
  DaemonClientForgetPayload,
  DaemonClientForgotPayload,
  DaemonStatusPayload,
  DaemonStatusResultPayload,
  DeviceState,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  HelloPayload,
  PairAcceptPayload,
  PairedServerState,
  PublicKeyWire,
  RouteReadyPayload,
  SessionClosePayload,
  SessionClosedPayload,
  SessionAttachedPayload,
  DaemonClientsResultPayload,
  SessionCreatePayload,
  SessionCreatedPayload,
  SessionCursorPayload,
  SessionCursorPresence,
  SessionDataPayload,
  SessionFileDeletePayload,
  SessionFileDeletedPayload,
  SessionFileDownloadChunkPayload,
  SessionFileDownloadChunkResultPayload,
  SessionFileDownloadPreparePayload,
  SessionFileDownloadReadyPayload,
  SessionFileReadPayload,
  SessionFileReadResultPayload,
  SessionFileWritePayload,
  SessionFileWrittenPayload,
  SessionFilesPayload,
  SessionFilesResultPayload,
  SessionGitActionKind,
  SessionGitActionPayload,
  SessionGitActionResultPayload,
  SessionGitPayload,
  SessionGitResultPayload,
  SessionListResultPayload,
  SessionRenamedPayload,
  SessionRenamePayload,
  SessionReorderedPayload,
  SessionReorderPayload,
  SessionResizePayload,
  SessionResizedPayload,
  TerminalSize,
  UUID,
} from "./types";
import {
  envelope,
  messageDataToText,
  nonce,
  nowMs,
  parseEnvelope,
  sessionDataToBase64,
} from "./wire";

interface DirectClientOptions {
  timeoutMs?: number;
  webSocketFactory?: (url: string) => WebSocket;
}

interface QueuedMessage {
  envelope: Envelope;
}

const DEFAULT_TIMEOUT_MS = 10000;

export { ProtocolClientError };

export class DirectClient {
  private readonly timeoutMs: number;
  private e2ee: E2eeSession;
  private closed = false;
  private readonly pendingInner: Envelope[] = [];

  private constructor(
    private readonly socket: WebSocket,
    private readonly inbox: SocketInbox,
    private readonly serverIdValue: UUID,
    private readonly deviceId: UUID,
    e2ee: E2eeSession,
    options: Required<Pick<DirectClientOptions, "timeoutMs">>,
  ) {
    this.e2ee = e2ee;
    this.timeoutMs = options.timeoutMs;
  }

  static async connect(
    url: string,
    routeServerId: UUID,
    deviceId: UUID,
    options: DirectClientOptions = {},
  ): Promise<DirectClient> {
    const socket = options.webSocketFactory?.(url) ?? new WebSocket(url);
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const inbox = new SocketInbox(socket);
    await waitForOpen(socket, timeoutMs);

    // route_hello 是统一 /ws 入口的第一帧；relay/daemon 先确认路由，再进入原有业务握手。
    sendOuterMessage(
      socket,
      envelope("route_hello", {
        server_id: routeServerId,
        role: "client",
        protocol_version: 2,
        nonce: nonce(),
        timestamp_ms: nowMs(),
      }),
    );
    const routeReady = (
      await withTimeout(inbox.read(), timeoutMs, "route_prelude_timeout")
    ).envelope;
    if (routeReady.type === "error") {
      throw protocolError(routeReady.payload as ErrorPayload);
    }
    if (routeReady.type !== "route_ready") {
      throw new ProtocolClientError("unexpected_message", "unexpected route prelude message");
    }
    const routeReadyPayload = routeReady.payload as RouteReadyPayload;
    if (routeReadyPayload.server_id !== routeServerId || routeReadyPayload.role !== "client") {
      throw new ProtocolClientError("route_server_mismatch", "route prelude does not match requested daemon");
    }

    const initial = (
      await withTimeout(Promise.all([inbox.read(), inbox.read()]), timeoutMs, "handshake_timeout")
    ).map((message) => message.envelope);

    let daemonPublicKeyWire: PublicKeyWire | undefined;
    for (const message of initial) {
      if (message.type === "hello") {
        const payload = message.payload as HelloPayload;
        if (payload.server_id && payload.server_id !== routeServerId) {
          throw new ProtocolClientError("route_server_mismatch", "daemon hello does not match requested route");
        }
      } else if (message.type === "e2ee_key_exchange") {
        const payload = message.payload as E2eeKeyExchangePayload;
        if (payload.server_id !== routeServerId) {
          throw new ProtocolClientError("route_server_mismatch", "daemon key exchange does not match requested route");
        }
        daemonPublicKeyWire = payload.public_key;
      } else if (message.type === "error") {
        throw protocolError(message.payload as ErrorPayload);
      } else {
        throw new ProtocolClientError("unexpected_message", "unexpected handshake message");
      }
    }

    if (!daemonPublicKeyWire) {
      throw new ProtocolClientError("invalid_handshake", "daemon handshake was incomplete");
    }

    const keypair = generateE2eeKeyPair();
    const e2ee = E2eeSession.device({
      serverId: routeServerId,
      deviceId,
      localKeypair: keypair,
      daemonPublicKeyWire,
    });
    const client = new DirectClient(socket, inbox, routeServerId, deviceId, e2ee, { timeoutMs });
    client.sendOuter(
      envelope("e2ee_key_exchange", {
        server_id: routeServerId,
        device_id: deviceId,
        public_key: keypair.publicKeyWire,
        nonce: nonce(),
        timestamp_ms: nowMs(),
      } satisfies E2eeKeyExchangePayload),
    );
    return client;
  }

  get serverId(): UUID {
    return this.serverIdValue;
  }

  async pair(token: string, devicePublicKey: PublicKeyWire): Promise<PairAcceptPayload> {
    await this.sendInner(
      envelope("pair_request", {
        device_id: this.deviceId,
        device_public_key: devicePublicKey,
        token,
        nonce: nonce(),
        timestamp_ms: nowMs(),
      }),
    );
    // 已配对过的浏览器 device 重新 Pair 时，daemon 会在 E2EE 握手后主动发 auth_challenge。
    // Pairing token 仍会被后续 pair_request 校验，这里只跳过这个与本次 Pair 无关的预发挑战。
    return this.expectPayload<PairAcceptPayload>("pair_accept", { ignoredTypes: ["auth_challenge"] });
  }

  async authenticate(device: DeviceState, server: PairedServerState): Promise<void> {
    const challenge = await this.expectPayload<AuthChallengePayload>("auth_challenge");
    const auth = await signAuthPayload(
      authPayloadForChallenge(device.device_id, challenge.challenge),
      server,
      device.device_signing_key_secret,
    );
    await this.sendInner(envelope("auth", auth));
    await this.sendInner(envelope("client_hello", { name: device.name?.trim() || "Web client" } satisfies ClientHelloPayload));
  }

  async listSessions(): Promise<SessionListResultPayload> {
    await this.sendInner(envelope("session_list", {}));
    return this.expectPayload<SessionListResultPayload>("session_list_result");
  }

  async listDaemonClients(): Promise<DaemonClientsResultPayload> {
    await this.sendInner(envelope("daemon_clients", {}));
    return this.expectPayload<DaemonClientsResultPayload>("daemon_clients_result");
  }

  async forgetDaemonClient(deviceId: UUID): Promise<DaemonClientForgotPayload> {
    await this.sendInner(envelope("daemon_client_forget", { device_id: deviceId } satisfies DaemonClientForgetPayload));
    return this.expectPayload<DaemonClientForgotPayload>("daemon_client_forgot");
  }

  async getDaemonStatus(): Promise<DaemonStatusResultPayload> {
    await this.sendInner(envelope("daemon_status", {} satisfies DaemonStatusPayload));
    return this.expectPayload<DaemonStatusResultPayload>("daemon_status_result", { bufferTerminalEvents: true });
  }

  async listSessionFiles(sessionId: UUID, path?: string): Promise<SessionFilesResultPayload> {
    await this.requestSessionFiles(sessionId, path);
    return this.expectPayload<SessionFilesResultPayload>("session_files_result", { bufferTerminalEvents: true });
  }

  async requestSessionFiles(sessionId: UUID, path?: string): Promise<void> {
    await this.sendInner(
      envelope("session_files", {
        session_id: sessionId,
        ...(path ? { path } : {}),
      } satisfies SessionFilesPayload),
    );
  }

  async getSessionGit(sessionId: UUID): Promise<SessionGitResultPayload> {
    await this.requestSessionGit(sessionId);
    return this.expectPayload<SessionGitResultPayload>("session_git_result", { bufferTerminalEvents: true });
  }

  async requestSessionGit(sessionId: UUID): Promise<void> {
    await this.sendInner(envelope("session_git", { session_id: sessionId } satisfies SessionGitPayload));
  }

  async applySessionGitAction(
    sessionId: UUID,
    worktreePath: string,
    filePath: string,
    action: SessionGitActionKind,
  ): Promise<SessionGitActionResultPayload> {
    await this.sendInner(
      envelope("session_git_action", {
        session_id: sessionId,
        worktree_path: worktreePath,
        file_path: filePath,
        action,
      } satisfies SessionGitActionPayload),
    );
    return this.expectPayload<SessionGitActionResultPayload>("session_git_action_result", { bufferTerminalEvents: true });
  }

  async readSessionFile(sessionId: UUID, path: string): Promise<SessionFileReadResultPayload> {
    await this.sendInner(envelope("session_file_read", { session_id: sessionId, path } satisfies SessionFileReadPayload));
    return this.expectPayload<SessionFileReadResultPayload>("session_file_read_result", { bufferTerminalEvents: true });
  }

  async writeSessionFile(sessionId: UUID, path: string, bytes: Uint8Array): Promise<SessionFileWrittenPayload> {
    await this.sendInner(
      envelope("session_file_write", {
        session_id: sessionId,
        path,
        data_base64: sessionDataToBase64(bytes),
      } satisfies SessionFileWritePayload),
    );
    return this.expectPayload<SessionFileWrittenPayload>("session_file_written", { bufferTerminalEvents: true });
  }

  async deleteSessionFile(sessionId: UUID, path: string): Promise<SessionFileDeletedPayload> {
    await this.sendInner(envelope("session_file_delete", { session_id: sessionId, path } satisfies SessionFileDeletePayload));
    return this.expectPayload<SessionFileDeletedPayload>("session_file_deleted", { bufferTerminalEvents: true });
  }

  async prepareSessionFileDownload(sessionId: UUID, path: string): Promise<SessionFileDownloadReadyPayload> {
    await this.sendInner(
      envelope("session_file_download_prepare", {
        session_id: sessionId,
        path,
      } satisfies SessionFileDownloadPreparePayload),
    );
    return this.expectPayload<SessionFileDownloadReadyPayload>("session_file_download_ready", { bufferTerminalEvents: true });
  }

  async readSessionFileDownloadChunk(
    sessionId: UUID,
    path: string,
    offsetBytes: number,
    maxBytes: number,
  ): Promise<SessionFileDownloadChunkResultPayload> {
    await this.sendInner(
      envelope("session_file_download_chunk", {
        session_id: sessionId,
        path,
        offset_bytes: offsetBytes,
        max_bytes: maxBytes,
      } satisfies SessionFileDownloadChunkPayload),
    );
    return this.expectPayload<SessionFileDownloadChunkResultPayload>("session_file_download_chunk_result", { bufferTerminalEvents: true });
  }

  async createSession(command: string[], size: TerminalSize): Promise<SessionCreatedPayload> {
    await this.sendInner(
      envelope("session_create", {
        command,
        size,
      } satisfies SessionCreatePayload),
    );
    return this.expectPayload<SessionCreatedPayload>("session_created");
  }

  async attachSession(sessionId: UUID): Promise<SessionAttachedPayload> {
    await this.sendInner(
      envelope("session_attach", {
        session_id: sessionId,
      }),
    );
    return this.expectPayload<SessionAttachedPayload>("session_attached");
  }

  async sendSessionData(sessionId: UUID, bytes: Uint8Array): Promise<void> {
    await this.sendInner(
      envelope("session_data", {
        session_id: sessionId,
        data_base64: sessionDataToBase64(bytes),
      } satisfies SessionDataPayload),
    );
  }

  async sendSessionCursor(sessionId: UUID, presence: SessionCursorPresence): Promise<void> {
    await this.sendInner(
      envelope("session_cursor", {
        session_id: sessionId,
        row: presence.row,
        col: presence.col,
        focused: presence.focused,
      } satisfies SessionCursorPayload),
    );
  }

  async resizeSession(sessionId: UUID, size: TerminalSize): Promise<SessionResizedPayload> {
    await this.requestSessionResize(sessionId, size);
    return this.expectPayload<SessionResizedPayload>("session_resized", { bufferTerminalEvents: true });
  }

  async requestSessionResize(sessionId: UUID, size: TerminalSize): Promise<void> {
    await this.sendInner(envelope("session_resize", { session_id: sessionId, size } satisfies SessionResizePayload));
  }

  async renameSession(sessionId: UUID, name: string): Promise<SessionRenamedPayload> {
    await this.sendInner(
      envelope("session_rename", {
        session_id: sessionId,
        name,
      } satisfies SessionRenamePayload),
    );
    return this.expectPayload<SessionRenamedPayload>("session_renamed", { bufferTerminalEvents: true });
  }

  async reorderSessions(sessionIds: UUID[]): Promise<SessionReorderedPayload> {
    await this.sendInner(envelope("session_reorder", { session_ids: sessionIds } satisfies SessionReorderPayload));
    return this.expectPayload<SessionReorderedPayload>("session_reordered", { bufferTerminalEvents: true });
  }

  async closeSession(sessionId: UUID): Promise<SessionClosedPayload> {
    await this.sendInner(envelope("session_close", { session_id: sessionId } satisfies SessionClosePayload));
    return this.expectPayload<SessionClosedPayload>("session_closed");
  }

  async requestControl(sessionId: UUID): Promise<ControlGrantPayload> {
    await this.sendControlRequest(sessionId);
    return this.expectPayload<ControlGrantPayload>("control_grant");
  }

  async sendControlRequest(sessionId: UUID): Promise<void> {
    await this.sendInner(envelope("control_request", { session_id: sessionId, device_id: this.deviceId }));
  }

  async sendPing(): Promise<void> {
    await this.sendInner(envelope("ping", { nonce: nonce(), timestamp_ms: nowMs() }));
  }

  async receiveInner(): Promise<Envelope> {
    const pending = this.pendingInner.shift();
    if (pending) {
      return pending;
    }

    return this.receiveInnerFromSocket();
  }

  private async receiveInnerFromSocket(): Promise<Envelope> {
    while (true) {
      const outer = await this.readOuter();
      if (outer.type === "encrypted_frame") {
        const inner = this.e2ee.decryptJson(outer.payload as EncryptedFramePayload);
        if (inner.type === "error") {
          throw protocolError(inner.payload as ErrorPayload);
        }
        return inner;
      }
      if (outer.type === "error") {
        throw protocolError(outer.payload as ErrorPayload);
      }
      throw new ProtocolClientError("unexpected_message", "unexpected outer message");
    }
  }

  close(): void {
    this.closed = true;
    this.socket.close();
    this.inbox.rejectPending(new ProtocolClientError("connection_closed", "connection closed"));
  }

  private async expectPayload<T>(
    expectedType: Envelope["type"],
    options: { bufferTerminalEvents?: boolean; ignoredTypes?: Envelope["type"][] } = {},
  ): Promise<T> {
    while (true) {
      const inner = await withTimeout(this.receiveInnerFromSocket(), this.timeoutMs, "response_timeout");
      if (inner.type === "pong") {
        continue;
      }
      if (options.ignoredTypes?.includes(inner.type)) {
        continue;
      }
      if (
        options.bufferTerminalEvents &&
        inner.type !== expectedType &&
        (inner.type === "session_data" ||
          inner.type === "session_activity" ||
          inner.type === "session_resized" ||
          inner.type === "control_grant" ||
          inner.type === "session_files_result" ||
          inner.type === "session_git_result" ||
          inner.type === "session_git_action_result")
      ) {
        // 文件操作复用已 attach 的终端连接；daemon 可能先推送 PTY 输出或文件树同步事件。
        // 这里把旁路事件放回队列，交给后续 receive loop 处理，避免文件 panel 吃掉回显。
        this.pendingInner.push(inner);
        continue;
      }
      if (inner.type !== expectedType) {
        throw new ProtocolClientError("unexpected_message", "unexpected protocol response");
      }
      return inner.payload as T;
    }
  }

  private async sendInner(inner: Envelope): Promise<void> {
    const frame = this.e2ee.encryptJson(inner);
    this.sendOuter(envelope("encrypted_frame", frame));
  }

  private sendOuter(message: Envelope): void {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolClientError("connection_closed", "connection closed");
    }
    sendOuterMessage(this.socket, message);
  }

  private readOuter(): Promise<Envelope> {
    return this.inbox.read().then((message) => message.envelope);
  }
}

class SocketInbox {
  private readonly queue: QueuedMessage[] = [];
  private readonly waiters: Array<(message: QueuedMessage) => void> = [];
  private readonly errors: Array<(error: Error) => void> = [];

  constructor(private readonly socket: WebSocket) {
    // 监听器必须在等待 open 前注册；route_ready 和后续 hello/E2EE 可能会连续到达。
    this.socket.addEventListener("message", (event) => {
      void this.enqueueMessage(event.data);
    });
    this.socket.addEventListener("close", () => this.rejectPending(new ProtocolClientError("connection_closed", "connection closed")));
    this.socket.addEventListener("error", () => this.rejectPending(new ProtocolClientError("connection_error", "connection error")));
  }

  read(): Promise<QueuedMessage> {
    if (this.queue.length > 0) {
      return Promise.resolve(this.queue.shift()!);
    }
    return new Promise((resolve, reject) => {
      this.waiters.push(resolve);
      this.errors.push(reject);
    });
  }

  rejectPending(error: Error): void {
    let reject = this.errors.shift();
    while (reject) {
      this.waiters.shift();
      reject(error);
      reject = this.errors.shift();
    }
  }

  private async enqueueMessage(data: unknown): Promise<void> {
    try {
      const message = { envelope: parseEnvelope(await messageDataToText(data)) };
      const waiter = this.waiters.shift();
      this.errors.shift();
      if (waiter) {
        waiter(message);
      } else {
        this.queue.push(message);
      }
    } catch (error) {
      this.rejectPending(error instanceof Error ? error : new Error("invalid_envelope"));
    }
  }
}

function sendOuterMessage(socket: WebSocket, message: Envelope): void {
  socket.send(JSON.stringify(message));
}

function waitForOpen(socket: WebSocket, timeoutMs: number): Promise<void> {
  if (socket.readyState === WebSocket.OPEN) {
    return Promise.resolve();
  }
  return withTimeout(
    new Promise((resolve, reject) => {
      socket.addEventListener("open", () => resolve(undefined), { once: true });
      socket.addEventListener("error", () => reject(new ProtocolClientError("connection_error", "connection error")), {
        once: true,
      });
    }),
    timeoutMs,
    "connect_timeout",
  );
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, code: string): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new ProtocolClientError(code, "operation timed out")), timeoutMs);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}
