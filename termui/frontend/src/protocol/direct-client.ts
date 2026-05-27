import {
  authPayloadForChallenge,
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  e2eeAuthTranscriptDigestWire,
  signAuthPayload,
  verifyEd25519Signature,
} from "./auth";
import {
  E2eeSession,
  decodeBinaryEncryptedFrame,
  encodeBinaryEncryptedFrame,
  generateE2eeKeyPair,
} from "./e2ee";
import {
  type BinaryProtocolPacket,
  decodeBinaryProtocolPacket,
  encodeBinaryProtocolPacket,
  terminalFrameBinaryToJson,
  terminalFrameJsonToBinary,
} from "./binary-packet";
import { ProtocolClientError, protocolError } from "./errors";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "./types";
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
  PairRequestPayload,
  PairedServerState,
  PacketErrorPayload,
  PacketStreamId,
  PongPayload,
  ProtocolPacket,
  PublicKeyWire,
  RouteReadyPayload,
  SessionAttachPayload,
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
  SessionGitDiffPayload,
  SessionGitDiffResultPayload,
  SessionGitPayload,
  SessionGitResultPayload,
  SessionListResultPayload,
  SessionRenamedPayload,
  SessionRenamePayload,
  SessionReorderedPayload,
  SessionReorderPayload,
  SessionResizePayload,
  SessionResizedPayload,
  SessionSearchPayload,
  SessionSearchResultPayload,
  SingleTerminalFramePayload,
  TerminalSize,
  UUID,
} from "./types";
import {
  base64ToBytes,
  bytesToBase64,
  decodeUtf8,
  envelope,
  encodeUtf8,
  messageDataToText,
  nonce,
  nowMs,
  parseEnvelope,
  randomUuid,
  sessionDataToBase64,
} from "./wire";

interface DirectClientOptions {
  timeoutMs?: number;
  requestTimeoutMs?: number;
  expectedDaemonPublicKey?: PublicKeyWire;
  webSocketFactory?: (url: string) => WebSocket;
  signal?: AbortSignal;
}

interface QueuedMessage {
  envelope?: Envelope;
  binary?: Uint8Array;
}

interface PendingRequest {
  method: string;
  resolve: (payload: unknown) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface QueuedInnerWaiter {
  resolve: (envelope: Envelope) => void;
  reject: (error: Error) => void;
}

interface TerminalStreamState {
  sessionId: UUID;
  streamId: PacketStreamId;
  nextInputSeq: number;
  lastOutputSeq: number;
}

const DEFAULT_TIMEOUT_MS = 30000;
const RECEIVE_PUMP_YIELD_MESSAGES = 64;
const RECEIVE_PUMP_YIELD_BYTES = 256 * 1024;

export { ProtocolClientError };

function queuedMessageBytes(message: QueuedMessage): number {
  if (message.binary) {
    return message.binary.byteLength;
  }
  return 0;
}

function yieldToEventLoop(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

export class DirectClient {
  private readonly timeoutMs: number;
  private readonly authTimeoutMs: number;
  private e2ee: E2eeSession;
  private closed = false;
  private receivePumpStarted = false;
  private e2eeTranscriptSha256?: string;
  private readonly pendingRequests = new Map<UUID, PendingRequest>();
  private readonly pendingInner: Envelope[] = [];
  private readonly innerWaiters: QueuedInnerWaiter[] = [];
  private readonly terminalStreamsBySession = new Map<UUID, TerminalStreamState>();
  private readonly terminalStreamsById = new Map<PacketStreamId, TerminalStreamState>();

  private constructor(
    private readonly socket: WebSocket,
    private readonly inbox: SocketInbox,
    private readonly serverIdValue: UUID,
    private readonly deviceId: UUID,
    e2ee: E2eeSession,
    options: Required<Pick<DirectClientOptions, "timeoutMs" | "requestTimeoutMs">>,
    private readonly binaryMode: boolean,
  ) {
    this.e2ee = e2ee;
    this.authTimeoutMs = options.timeoutMs;
    this.timeoutMs = options.requestTimeoutMs;
  }

  static async connect(
    url: string,
    routeServerId: UUID,
    deviceId: UUID,
    options: DirectClientOptions = {},
  ): Promise<DirectClient> {
    const socket = options.webSocketFactory?.(url) ?? new WebSocket(url);
    socket.binaryType = "arraybuffer";
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const requestTimeoutMs = options.requestTimeoutMs ?? timeoutMs;
    const inbox = new SocketInbox(socket);
    const abortSignal = options.signal;
    const closeSocketOnAbort = () => socket.close();
    abortSignal?.addEventListener("abort", closeSocketOnAbort, { once: true });

    try {
      throwIfAborted(abortSignal);
      await withAbort(waitForOpen(socket, timeoutMs), abortSignal);

      // route_hello 是统一 /ws 入口的第一帧；relay/daemon 先确认路由，再进入原有业务握手。
      sendOuterMessage(
        socket,
        envelope("route_hello", {
          server_id: routeServerId,
          role: "client",
          protocol_version: PROTOCOL_PACKET_VERSION,
          nonce: nonce(),
          timestamp_ms: nowMs(),
        }),
      );
      const routeReady = expectQueuedEnvelope(
        await withAbort(
          withTimeout(inbox.read(), timeoutMs, "route_prelude_timeout"),
          abortSignal,
        ),
      );
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
        await withAbort(
          withTimeout(Promise.all([inbox.read(), inbox.read()]), timeoutMs, "handshake_timeout"),
          abortSignal,
        )
      ).map(expectQueuedEnvelope);

      const expectedDaemonPublicKey = options.expectedDaemonPublicKey;
      if (!expectedDaemonPublicKey) {
        throw new ProtocolClientError("daemon_identity_required", "daemon public key is required");
      }

      let daemonKeyExchange: E2eeKeyExchangePayload | undefined;
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
          if (payload.packet_version !== PROTOCOL_PACKET_VERSION) {
            throw new ProtocolClientError("unsupported_protocol_version", "daemon key exchange does not support packet v3");
          }
          if (!payload.signature) {
            throw new ProtocolClientError("invalid_handshake", "daemon key exchange is unsigned");
          }
          const verified = await verifyEd25519Signature(
            decodeEd25519PublicKey(expectedDaemonPublicKey),
            daemonE2eeSigningInputBytes(payload, {
              server_id: routeServerId,
              daemon_public_key: expectedDaemonPublicKey,
            }),
            payload.signature,
          );
          if (!verified) {
            throw new ProtocolClientError("daemon_identity_mismatch", "daemon key exchange signature is invalid");
          }
          daemonKeyExchange = payload;
        } else if (message.type === "error") {
          throw protocolError(message.payload as ErrorPayload);
        } else {
          throw new ProtocolClientError("unexpected_message", "unexpected handshake message");
        }
      }

      if (!daemonKeyExchange) {
        throw new ProtocolClientError("invalid_handshake", "daemon handshake was incomplete");
      }

      const keypair = generateE2eeKeyPair();
      const e2ee = E2eeSession.device({
        serverId: routeServerId,
        deviceId,
        localKeypair: keypair,
        daemonPublicKeyWire: daemonKeyExchange.public_key,
      });
      const binaryMode = daemonKeyExchange.binary_version === BINARY_PROTOCOL_VERSION;
      const client = new DirectClient(socket, inbox, routeServerId, deviceId, e2ee, { timeoutMs, requestTimeoutMs }, binaryMode);
      const deviceKeyExchange: E2eeKeyExchangePayload = {
        server_id: routeServerId,
        device_id: deviceId,
        public_key: keypair.publicKeyWire,
        nonce: nonce(),
        timestamp_ms: nowMs(),
        packet_version: PROTOCOL_PACKET_VERSION,
        ...(binaryMode ? { binary_version: BINARY_PROTOCOL_VERSION } : {}),
      };
      client.sendOuter(
        envelope("e2ee_key_exchange", deviceKeyExchange),
      );
      client.e2eeTranscriptSha256 = e2eeAuthTranscriptDigestWire(
        daemonKeyExchange,
        deviceKeyExchange,
        {
          server_id: routeServerId,
          daemon_public_key: expectedDaemonPublicKey,
        },
      );
      client.startReceivePump();
      return client;
    } catch (error) {
      // 连接建立阶段一旦超时或握手失败，必须关闭半开 socket，避免 relay 侧残留旧 client。
      socket.close();
      inbox.rejectPending(new ProtocolClientError("connection_closed", "connection closed"));
      throw error;
    } finally {
      abortSignal?.removeEventListener("abort", closeSocketOnAbort);
    }
  }

  get serverId(): UUID {
    return this.serverIdValue;
  }

  get isClosed(): boolean {
    return this.closed || this.socket.readyState === WebSocket.CLOSING || this.socket.readyState === WebSocket.CLOSED;
  }

  async pair(token: string, devicePublicKey: PublicKeyWire): Promise<PairAcceptPayload> {
    return this.request<PairAcceptPayload>("pair.request", {
      device_id: this.deviceId,
      device_public_key: devicePublicKey,
      token,
      nonce: nonce(),
      timestamp_ms: nowMs(),
    } satisfies PairRequestPayload);
  }

  async authenticate(device: DeviceState, server: PairedServerState): Promise<void> {
    const challenge = await this.expectQueuedPayload<AuthChallengePayload>("auth_challenge", this.authTimeoutMs);
    const auth = await signAuthPayload(
      authPayloadForChallenge(device.device_id, challenge.challenge),
      server,
      device.device_signing_key_secret,
      this.e2eeTranscriptSha256,
    );
    await this.request("auth.verify", auth, this.authTimeoutMs);
    await this.request("client.hello", { name: device.name?.trim() || "Web client" } satisfies ClientHelloPayload, this.authTimeoutMs);
  }

  async listSessions(): Promise<SessionListResultPayload> {
    return this.request<SessionListResultPayload>("session.list", {});
  }

  async listDaemonClients(): Promise<DaemonClientsResultPayload> {
    return this.request<DaemonClientsResultPayload>("daemon.clients", {});
  }

  async forgetDaemonClient(deviceId: UUID): Promise<DaemonClientForgotPayload> {
    return this.request<DaemonClientForgotPayload>("daemon.client_forget", { device_id: deviceId } satisfies DaemonClientForgetPayload);
  }

  async getDaemonStatus(): Promise<DaemonStatusResultPayload> {
    return this.request<DaemonStatusResultPayload>("daemon.status", {} satisfies DaemonStatusPayload);
  }

  async listSessionFiles(sessionId: UUID, path?: string): Promise<SessionFilesResultPayload> {
    return this.request<SessionFilesResultPayload>(
      "session.files",
      {
        session_id: sessionId,
        ...(path ? { path } : {}),
      } satisfies SessionFilesPayload,
    );
  }

  async requestSessionFiles(sessionId: UUID, path?: string): Promise<void> {
    const payload = await this.request<SessionFilesResultPayload>(
      "session.files",
      {
        session_id: sessionId,
        ...(path ? { path } : {}),
      } satisfies SessionFilesPayload,
    );
    this.enqueueInner(envelope("session_files_result", payload));
  }

  async getSessionGit(sessionId: UUID): Promise<SessionGitResultPayload> {
    return this.request<SessionGitResultPayload>("session.git", { session_id: sessionId } satisfies SessionGitPayload);
  }

  async requestSessionGit(sessionId: UUID): Promise<void> {
    const payload = await this.request<SessionGitResultPayload>("session.git", { session_id: sessionId } satisfies SessionGitPayload);
    this.enqueueInner(envelope("session_git_result", payload));
  }

  async applySessionGitAction(
    sessionId: UUID,
    worktreePath: string,
    filePath: string,
    action: SessionGitActionKind,
  ): Promise<SessionGitActionResultPayload> {
    return this.request<SessionGitActionResultPayload>(
      "session.git_action",
      {
        session_id: sessionId,
        worktree_path: worktreePath,
        file_path: filePath,
        action,
      } satisfies SessionGitActionPayload,
    );
  }

  async searchSessionOutput(
    sessionId: UUID,
    query: string,
    options: { caseSensitive?: boolean; maxResults?: number } = {},
  ): Promise<SessionSearchResultPayload> {
    return this.request<SessionSearchResultPayload>(
      "session.search",
      {
        session_id: sessionId,
        query,
        case_sensitive: Boolean(options.caseSensitive),
        max_results: options.maxResults ?? 80,
      } satisfies SessionSearchPayload,
    );
  }

  async getSessionGitDiff(
    sessionId: UUID,
    worktreePath: string,
    filePath?: string | null,
    staged = false,
  ): Promise<SessionGitDiffResultPayload> {
    return this.request<SessionGitDiffResultPayload>(
      "session.git_diff",
      {
        session_id: sessionId,
        worktree_path: worktreePath,
        file_path: filePath,
        staged,
      } satisfies SessionGitDiffPayload,
    );
  }

  async readSessionFile(sessionId: UUID, path: string): Promise<SessionFileReadResultPayload> {
    return this.request<SessionFileReadResultPayload>("session.file_read", { session_id: sessionId, path } satisfies SessionFileReadPayload);
  }

  async writeSessionFile(sessionId: UUID, path: string, bytes: Uint8Array): Promise<SessionFileWrittenPayload> {
    return this.request<SessionFileWrittenPayload>(
      "session.file_write",
      {
        session_id: sessionId,
        path,
        data_base64: sessionDataToBase64(bytes),
      } satisfies SessionFileWritePayload,
    );
  }

  async deleteSessionFile(sessionId: UUID, path: string): Promise<SessionFileDeletedPayload> {
    return this.request<SessionFileDeletedPayload>("session.file_delete", { session_id: sessionId, path } satisfies SessionFileDeletePayload);
  }

  async prepareSessionFileDownload(sessionId: UUID, path: string): Promise<SessionFileDownloadReadyPayload> {
    return this.request<SessionFileDownloadReadyPayload>(
      "session.file_download_prepare",
      {
        session_id: sessionId,
        path,
      } satisfies SessionFileDownloadPreparePayload,
    );
  }

  async readSessionFileDownloadChunk(
    sessionId: UUID,
    path: string,
    offsetBytes: number,
    maxBytes: number,
  ): Promise<SessionFileDownloadChunkResultPayload> {
    return this.request<SessionFileDownloadChunkResultPayload>(
      "session.file_download_chunk",
      {
        session_id: sessionId,
        path,
        offset_bytes: offsetBytes,
        max_bytes: maxBytes,
      } satisfies SessionFileDownloadChunkPayload,
    );
  }

  async createSession(
    command: string[],
    size: TerminalSize,
    options: { timeoutMs?: number } = {},
  ): Promise<SessionCreatedPayload> {
    return this.openTerminalStream<SessionCreatedPayload>(
      "terminal.create",
      {
        command,
        size,
      } satisfies SessionCreatePayload,
      undefined,
      options.timeoutMs,
    );
  }

  async attachSession(
    sessionId: UUID,
    options: { watchUpdates?: boolean; lastTerminalSeq?: number; timeoutMs?: number } = {},
  ): Promise<SessionAttachedPayload> {
    return this.openTerminalStream<SessionAttachedPayload>(
      "terminal.attach",
      {
        session_id: sessionId,
        watch_updates: options.watchUpdates ?? true,
        ...(options.lastTerminalSeq !== undefined ? { last_terminal_seq: options.lastTerminalSeq } : {}),
      } satisfies SessionAttachPayload,
      sessionId,
      options.timeoutMs,
    );
  }

  async attachSessionPermission(sessionId: UUID): Promise<SessionAttachedPayload> {
    return this.request<SessionAttachedPayload>(
      "session.attach",
      {
        session_id: sessionId,
        watch_updates: false,
      } satisfies SessionAttachPayload,
    );
  }

  async sendSessionData(sessionId: UUID, bytes: Uint8Array): Promise<void> {
    const stream = this.terminalStreamsBySession.get(sessionId);
    if (!stream) {
      throw new ProtocolClientError("invalid_state", "terminal stream is not attached");
    }
    const seq = stream.nextInputSeq;
    stream.nextInputSeq += 1;
    this.sendPacket({
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: stream.streamId,
      seq,
      payload: {
        session_id: sessionId,
        data_base64: sessionDataToBase64(bytes),
      } satisfies SessionDataPayload,
    });
  }

  async sendSessionCursor(sessionId: UUID, presence: SessionCursorPresence): Promise<void> {
    await this.request(
      "session.cursor",
      {
        session_id: sessionId,
        row: presence.row,
        col: presence.col,
        focused: presence.focused,
      } satisfies SessionCursorPayload,
    );
  }

  async resizeSession(sessionId: UUID, size: TerminalSize): Promise<SessionResizedPayload> {
    return this.request<SessionResizedPayload>("session.resize", { session_id: sessionId, size } satisfies SessionResizePayload);
  }

  async requestSessionResize(sessionId: UUID, size: TerminalSize): Promise<void> {
    const payload = await this.request<SessionResizedPayload>(
      "session.resize",
      { session_id: sessionId, size } satisfies SessionResizePayload,
    );
    this.enqueueInner(envelope("session_resized", payload));
  }

  async renameSession(sessionId: UUID, name: string): Promise<SessionRenamedPayload> {
    return this.request<SessionRenamedPayload>(
      "session.rename",
      {
        session_id: sessionId,
        name,
      } satisfies SessionRenamePayload,
    );
  }

  async reorderSessions(sessionIds: UUID[]): Promise<SessionReorderedPayload> {
    return this.request<SessionReorderedPayload>("session.reorder", { session_ids: sessionIds } satisfies SessionReorderPayload);
  }

  async closeSession(sessionId: UUID): Promise<SessionClosedPayload> {
    return this.request<SessionClosedPayload>("session.close", { session_id: sessionId } satisfies SessionClosePayload);
  }

  async requestControl(sessionId: UUID): Promise<ControlGrantPayload> {
    return this.request<ControlGrantPayload>("control.request", { session_id: sessionId, device_id: this.deviceId });
  }

  async sendControlRequest(sessionId: UUID): Promise<void> {
    await this.requestControl(sessionId);
  }

  async sendPing(): Promise<void> {
    await this.request<PongPayload>("ping", { nonce: nonce(), timestamp_ms: nowMs() });
  }

  async measureLatency(): Promise<number> {
    const pingNonce = nonce();
    const startedAt = performance.now();
    await this.request<PongPayload>("ping", { nonce: pingNonce, timestamp_ms: nowMs() });
    return Math.max(0, performance.now() - startedAt);
  }

  async request<T = unknown>(method: string, payload: unknown, timeoutMs = this.timeoutMs): Promise<T> {
    const id = randomUuid();
    return this.sendTrackedPacket<T>(
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "request",
        id,
        method,
        payload,
      },
      id,
      method,
      timeoutMs,
    );
  }

  async receiveInner(): Promise<Envelope> {
    const pending = this.pendingInner.shift();
    if (pending) {
      if (pending.type === "error") {
        throw protocolError(pending.payload as ErrorPayload);
      }
      return pending;
    }

    return new Promise((resolve, reject) => {
      this.innerWaiters.push({
        resolve: (inner) => {
          if (inner.type === "error") {
            reject(protocolError(inner.payload as ErrorPayload));
            return;
          }
          resolve(inner);
        },
        reject,
      });
    });
  }

  detachSession(sessionId: UUID, reason = "client_detached"): void {
    const stream = this.terminalStreamsBySession.get(sessionId);
    if (!stream) {
      return;
    }
    // 中文注释：切换会话只取消当前 terminal stream，不能关闭当前 session 的 WebSocket。
    // 同一条连接还要继续承载 RPC / 事件 / 其他非终端 segment。
    this.sendPacketBestEffort({
      version: PROTOCOL_PACKET_VERSION,
      kind: "cancel",
      stream_id: stream.streamId,
      payload: { reason },
    });
    this.discardQueuedTerminalOutput(stream.streamId, stream.sessionId);
    this.removeStream(stream.streamId);
  }

  interruptReceiveWaiters(): void {
    // 中文注释：App 暂停终端输出消费时不能关闭 socket；这里只唤醒正在 await
    // receiveInner 的 UI 循环，后续入站 segment 继续留在 DirectClient 队列里。
    this.rejectInnerWaiters(new ProtocolClientError("receive_interrupted", "receive interrupted"));
  }

  close(): void {
    const error = new ProtocolClientError("connection_closed", "connection closed");
    for (const stream of this.terminalStreamsById.values()) {
      this.sendPacketBestEffort({
        version: PROTOCOL_PACKET_VERSION,
        kind: "cancel",
        stream_id: stream.streamId,
        payload: { reason: "client_closed" },
      });
    }
    this.terminalStreamsById.clear();
    this.terminalStreamsBySession.clear();
    this.closed = true;
    this.rejectPendingRequests(error);
    this.rejectInnerWaiters(error);
    this.socket.close();
    this.inbox.rejectPending(error);
  }

  private startReceivePump(): void {
    if (this.receivePumpStarted) {
      return;
    }
    this.receivePumpStarted = true;
    void this.runReceivePump();
  }

  private async runReceivePump(): Promise<void> {
    let processedMessages = 0;
    let processedBytes = 0;
    while (!this.closed) {
      try {
        const message = await this.inbox.read();
        processedMessages += 1;
        processedBytes += queuedMessageBytes(message);
        if (message.binary) {
          if (!this.binaryMode) {
            throw new ProtocolClientError("unexpected_message", "unexpected binary outer message");
          }
          this.dispatchBinaryWire(message.binary);
        } else {
          const outer = expectQueuedEnvelope(message);
          if (outer.type === "encrypted_frame") {
            const inner = this.e2ee.decryptJson(outer.payload as EncryptedFramePayload);
            this.dispatchInner(inner);
          } else if (outer.type === "error") {
            throw protocolError(outer.payload as ErrorPayload);
          } else {
            throw new ProtocolClientError("unexpected_message", "unexpected outer message");
          }
        }
        if (
          processedMessages >= RECEIVE_PUMP_YIELD_MESSAGES ||
          processedBytes >= RECEIVE_PUMP_YIELD_BYTES
        ) {
          processedMessages = 0;
          processedBytes = 0;
          await yieldToEventLoop();
        }
      } catch (caught) {
        if (!this.closed) {
          const error = caught instanceof Error ? caught : new ProtocolClientError("protocol_error", "protocol operation failed");
          // receive pump 已经证明这条 WebSocket 不再可信；标记关闭并主动 close，
          // 避免上层继续复用一个不会再消费入站消息的 DirectClient。
          this.closed = true;
          this.rejectPendingRequests(error);
          this.rejectInnerWaiters(error);
          this.socket.close();
          this.inbox.rejectPending(error);
        }
        return;
      }
    }
  }

  private dispatchInner(inner: Envelope): void {
    if (inner.type === "packet") {
      this.dispatchPacket(inner.payload as ProtocolPacket);
      return;
    }
    if (inner.type === "error") {
      throw protocolError(inner.payload as ErrorPayload);
    }
    throw new ProtocolClientError("unexpected_message", "expected protocol packet");
  }

  private dispatchPacket(packet: ProtocolPacket): void {
    if (packet.version !== PROTOCOL_PACKET_VERSION) {
      throw new ProtocolClientError("unsupported_protocol_version", "unsupported protocol packet version");
    }

    switch (packet.kind) {
      case "response":
        this.resolvePacketResponse(packet);
        return;
      case "error":
        this.dispatchPacketError(packet as ProtocolPacket<PacketErrorPayload>);
        return;
      case "event":
        this.enqueuePacketEvent(packet);
        return;
      case "stream_chunk":
        this.handleStreamChunk(packet);
        return;
      case "stream_end":
      case "cancel":
        this.removeStream(packet.stream_id);
        return;
      case "flow":
        return;
      case "request":
      case "stream_open":
        throw new ProtocolClientError("unexpected_message", "unexpected protocol packet");
      default:
        throw new ProtocolClientError("unexpected_message", "unexpected protocol packet");
    }
  }

  private resolvePacketResponse(packet: ProtocolPacket): void {
    if (!packet.id) {
      throw new ProtocolClientError("invalid_packet", "packet response is missing request id");
    }
    const pending = this.pendingRequests.get(packet.id);
    if (!pending) {
      return;
    }
    if (packet.method && packet.method !== pending.method) {
      this.rejectTrackedRequest(packet.id, new ProtocolClientError("unexpected_message", "unexpected protocol response"));
      return;
    }
    clearTimeout(pending.timer);
    this.pendingRequests.delete(packet.id);
    pending.resolve(packet.payload);
  }

  private dispatchPacketError(packet: ProtocolPacket<PacketErrorPayload>): void {
    const error = new ProtocolClientError(packet.payload.code, packet.payload.message);
    if (packet.id && this.pendingRequests.has(packet.id)) {
      this.rejectTrackedRequest(packet.id, error);
      return;
    }
    if (packet.stream_id) {
      // stream 级错误只进入事件队列，不能误伤其他 pending unary request。
      this.enqueueInner(envelope("error", { code: error.code, message: error.message } satisfies ErrorPayload));
    }
  }

  private enqueuePacketEvent(packet: ProtocolPacket): void {
    switch (packet.method) {
      case "auth.challenge":
        this.enqueueInner(envelope("auth_challenge", packet.payload as AuthChallengePayload));
        return;
      case "session.activity":
        this.enqueueInner(envelope("session_activity", packet.payload));
        return;
      case "session.files":
        this.enqueueInner(envelope("session_files_result", packet.payload));
        return;
      case "session.git":
        this.enqueueInner(envelope("session_git_result", packet.payload));
        return;
      case "session.resized":
        this.enqueueInner(envelope("session_resized", packet.payload));
        return;
      default:
        return;
    }
  }

  private handleStreamChunk(packet: ProtocolPacket): void {
    if (!packet.stream_id) {
      throw new ProtocolClientError("invalid_packet", "stream chunk is missing stream id");
    }
    const seq = packet.seq ?? 0;
    const stream = this.terminalStreamsById.get(packet.stream_id);
    if (!stream) {
      // 中文注释：用户快速切换 session 后，旧 stream 的少量输出可能已经在
      // WebSocket/TCP 队列里。stream 已取消时这些 chunk 必须在协议层丢弃，
      // 否则会继续堆进 pendingInner，把新 session 的 snapshot/tail 挡在后面。
      return;
    }
    stream.lastOutputSeq = seq;
    const payload = packet.payload as { kind?: unknown; session_id?: unknown; frames?: unknown };
    if (payload.kind === "batch" && Array.isArray(payload.frames)) {
      for (const frame of payload.frames) {
        if (this.isTerminalFramePayload(frame)) {
          this.enqueueTerminalFrame(frame, packet.stream_id, seq);
        }
      }
      return;
    }
    if (this.isTerminalFramePayload(payload)) {
      this.enqueueTerminalFrame(payload, packet.stream_id, seq);
      return;
    }

    this.enqueueSessionData(packet.payload as SessionDataPayload, packet.stream_id, seq);
  }

  private enqueueSessionData(payload: SessionDataPayload, streamId: PacketStreamId, transportSeq: number): void {
    this.enqueueInner(envelope("session_data", {
      ...payload,
      // 这两个字段只供前端定位 stream 归属；daemon/relay 仍只理解原始 session_data。
      stream_id: streamId,
      transport_seq: transportSeq,
    } satisfies SessionDataPayload));
  }

  private enqueueTerminalFrame(
    payload: SingleTerminalFramePayload,
    streamId: PacketStreamId,
    transportSeq: number,
  ): void {
    this.enqueueInner(envelope("terminal_frame", {
      ...(payload as object),
      transport_seq: transportSeq,
      stream_id: streamId,
    }));
  }

  private isTerminalFramePayload(payload: unknown): payload is SingleTerminalFramePayload {
    if (!payload || typeof payload !== "object") {
      return false;
    }
    const kind = (payload as { kind?: unknown }).kind;
    return kind === "snapshot" || kind === "output" || kind === "resize" || kind === "exit";
  }

  private async expectQueuedPayload<T>(expectedType: Envelope["type"], timeoutMs = this.timeoutMs): Promise<T> {
    const buffered: Envelope[] = [];
    try {
      while (true) {
        const inner = await withTimeout(this.receiveInner(), timeoutMs, "response_timeout");
        if (inner.type === expectedType) {
          return inner.payload as T;
        }
        buffered.push(inner);
      }
    } finally {
      for (const inner of buffered) {
        this.enqueueInner(inner);
      }
    }
  }

  private openTerminalStream<T extends { session_id: UUID }>(
    method: string,
    payload: unknown,
    sessionId?: UUID,
    timeoutMs = this.timeoutMs,
  ): Promise<T> {
    const id = randomUuid();
    const streamId = randomUuid();
    let provisionalStream: TerminalStreamState | undefined;
    if (sessionId) {
      provisionalStream = { sessionId, streamId, nextInputSeq: 1, lastOutputSeq: 0 };
      this.terminalStreamsBySession.set(sessionId, provisionalStream);
      this.terminalStreamsById.set(streamId, provisionalStream);
    }

    return this.sendTrackedPacket<T>(
      {
        version: PROTOCOL_PACKET_VERSION,
        kind: "stream_open",
        id,
        stream_id: streamId,
        method,
        payload,
      },
      id,
      method,
      timeoutMs,
    ).then(
      (response) => {
        const resolvedSessionId = response.session_id;
        if (provisionalStream && provisionalStream.sessionId !== resolvedSessionId) {
          this.terminalStreamsBySession.delete(provisionalStream.sessionId);
        }
        const stream = provisionalStream ?? { sessionId: resolvedSessionId, streamId, nextInputSeq: 1, lastOutputSeq: 0 };
        stream.sessionId = resolvedSessionId;
        this.terminalStreamsBySession.set(resolvedSessionId, stream);
        this.terminalStreamsById.set(streamId, stream);
        return response;
      },
      (error) => {
        this.removeStream(streamId);
        throw error;
      },
    );
  }

  private sendTrackedPacket<T>(packet: ProtocolPacket, id: UUID, method: string, timeoutMs = this.timeoutMs): Promise<T> {
    if (this.closed) {
      return Promise.reject(new ProtocolClientError("connection_closed", "connection closed"));
    }
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingRequests.delete(id);
        reject(new ProtocolClientError("response_timeout", "operation timed out"));
      }, timeoutMs);
      this.pendingRequests.set(id, {
        method,
        resolve: (payload) => resolve(payload as T),
        reject,
        timer,
      });
      try {
        this.sendPacket(packet);
      } catch (caught) {
        this.rejectTrackedRequest(id, caught instanceof Error ? caught : new Error("send_failed"));
      }
    });
  }

  private rejectTrackedRequest(id: UUID, error: Error): void {
    const pending = this.pendingRequests.get(id);
    if (!pending) {
      return;
    }
    clearTimeout(pending.timer);
    this.pendingRequests.delete(id);
    pending.reject(error);
  }

  private rejectPendingRequests(error: Error): void {
    for (const [id] of this.pendingRequests) {
      this.rejectTrackedRequest(id, error);
    }
  }

  private enqueueInner(inner: Envelope): void {
    const waiter = this.innerWaiters.shift();
    if (waiter) {
      waiter.resolve(inner);
      return;
    }
    this.pendingInner.push(inner);
  }

  private discardQueuedTerminalOutput(streamId: PacketStreamId, sessionId: UUID): void {
    if (this.pendingInner.length === 0) {
      return;
    }
    const retained = this.pendingInner.filter((inner) => !this.isTerminalOutputForStream(inner, streamId, sessionId));
    if (retained.length === this.pendingInner.length) {
      return;
    }
    this.pendingInner.splice(0, this.pendingInner.length, ...retained);
  }

  private isTerminalOutputForStream(inner: Envelope, streamId: PacketStreamId, sessionId: UUID): boolean {
    if (inner.type !== "session_data" && inner.type !== "terminal_frame") {
      return false;
    }
    const payload = inner.payload as { stream_id?: unknown; session_id?: unknown };
    return payload.stream_id === streamId || payload.session_id === sessionId;
  }

  private rejectInnerWaiters(error: Error): void {
    let waiter = this.innerWaiters.shift();
    while (waiter) {
      waiter.reject(error);
      waiter = this.innerWaiters.shift();
    }
  }

  private removeStream(streamId?: PacketStreamId): void {
    if (!streamId) {
      return;
    }
    const stream = this.terminalStreamsById.get(streamId);
    if (!stream) {
      return;
    }
    this.terminalStreamsById.delete(streamId);
    this.terminalStreamsBySession.delete(stream.sessionId);
  }

  private sendPacket(packet: ProtocolPacket): void {
    if (this.binaryMode) {
      this.sendBinaryPacket(packet);
      return;
    }
    this.sendInner(envelope("packet", packet));
  }

  private sendPacketBestEffort(packet: ProtocolPacket): void {
    try {
      this.sendPacket(packet);
    } catch {
      // cancel 是连接关闭提示；socket 已关闭时不能影响其它请求归属。
    }
  }

  private sendInner(inner: Envelope): void {
    const frame = this.e2ee.encryptJson(inner);
    this.sendOuter(envelope("encrypted_frame", frame));
  }

  private sendBinaryPacket(packet: ProtocolPacket): void {
    const frame = this.e2ee.encryptBinary(encodeBinaryProtocolPacket(protocolPacketToBinary(packet)));
    this.sendBinaryOuter(encodeBinaryEncryptedFrame(frame));
  }

  private sendOuter(message: Envelope): void {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolClientError("connection_closed", "connection closed");
    }
    sendOuterMessage(this.socket, message);
  }

  private sendBinaryOuter(bytes: Uint8Array): void {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolClientError("connection_closed", "connection closed");
    }
    this.socket.send(bytes);
  }

  private dispatchBinaryWire(bytes: Uint8Array): void {
    const frame = decodeBinaryEncryptedFrame(bytes);
    const packet = binaryPacketToProtocol(decodeBinaryProtocolPacket(this.e2ee.decryptBinary(frame)));
    this.dispatchPacket(packet);
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
      const message = typeof data === "string"
        ? { envelope: parseEnvelope(await messageDataToText(data)) }
        : { binary: await messageDataToBytes(data) };
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

function expectQueuedEnvelope(message: QueuedMessage): Envelope {
  if (!message.envelope) {
    throw new ProtocolClientError("unexpected_message", "expected JSON outer message");
  }
  return message.envelope;
}

function protocolPacketToBinary(packet: ProtocolPacket): BinaryProtocolPacket {
  const binary: BinaryProtocolPacket = {
    version: packet.version,
    kind: packet.kind,
    id: packet.id,
    stream_id: packet.stream_id,
    method: packet.method,
    seq: packet.seq,
    ack: packet.ack,
    credit: packet.credit,
  };
  if (packet.kind === "stream_chunk") {
    const payload = packet.payload as { session_id?: UUID; data_base64?: string; data_bytes?: Uint8Array; kind?: string };
    if (payload.kind) {
      return {
        ...binary,
        payload: { type: "terminal_frame", frame: terminalFrameJsonToBinary(payload) },
      };
    }
    if (payload.session_id && (typeof payload.data_base64 === "string" || payload.data_bytes instanceof Uint8Array)) {
      const data = payload.data_bytes ?? base64ToBytes(payload.data_base64 ?? "");
      return {
        ...binary,
        payload: { type: "session_data", session_id: payload.session_id, data },
      };
    }
  }
  if (packet.kind === "error") {
    const payload = packet.payload as { code?: string; message?: string; retryable?: boolean };
    if (payload.code && payload.message) {
      return {
        ...binary,
        payload: { type: "error", code: payload.code, message: payload.message, retryable: Boolean(payload.retryable) },
      };
    }
  }
  return {
    ...binary,
    payload: { type: "json", data: encodeUtf8(JSON.stringify(packet.payload ?? {})) },
  };
}

function binaryPacketToProtocol(packet: BinaryProtocolPacket): ProtocolPacket {
  let payload: unknown = {};
  if (packet.payload?.type === "json") {
    payload = JSON.parse(decodeUtf8(packet.payload.data));
  } else if (packet.payload?.type === "session_data") {
    payload = {
      session_id: packet.payload.session_id,
      data_base64: bytesToBase64(packet.payload.data),
      data_bytes: packet.payload.data,
    } satisfies SessionDataPayload;
  } else if (packet.payload?.type === "terminal_frame") {
    payload = terminalFrameBinaryToJson(packet.payload.frame);
  } else if (packet.payload?.type === "error") {
    payload = {
      code: packet.payload.code,
      message: packet.payload.message,
      retryable: packet.payload.retryable,
    } satisfies PacketErrorPayload;
  }
  return {
    version: packet.version,
    kind: packet.kind,
    ...(packet.id ? { id: packet.id } : {}),
    ...(packet.stream_id ? { stream_id: packet.stream_id } : {}),
    ...(packet.method ? { method: packet.method } : {}),
    ...(packet.seq ? { seq: packet.seq } : {}),
    ...(packet.ack ? { ack: packet.ack } : {}),
    ...(packet.credit ? { credit: packet.credit } : {}),
    payload,
  };
}

async function messageDataToBytes(data: unknown): Promise<Uint8Array> {
  if (data instanceof Blob) {
    return new Uint8Array(await data.arrayBuffer());
  }
  if (data instanceof ArrayBuffer || Object.prototype.toString.call(data) === "[object ArrayBuffer]") {
    return new Uint8Array(data as ArrayBuffer);
  }
  if (ArrayBuffer.isView(data)) {
    const view = data as ArrayBufferView;
    return new Uint8Array(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
  }
  return encodeUtf8(String(data));
}

function sendOuterMessage(socket: WebSocket, message: Envelope): void {
  socket.send(JSON.stringify(message));
}

function waitForOpen(socket: WebSocket, timeoutMs: number): Promise<void> {
  if (socket.readyState === WebSocket.OPEN) {
    return Promise.resolve();
  }
  if (socket.readyState === WebSocket.CLOSING || socket.readyState === WebSocket.CLOSED) {
    return Promise.reject(new ProtocolClientError("connection_closed", "connection closed"));
  }
  return withTimeout(
    new Promise((resolve, reject) => {
      socket.addEventListener("open", () => resolve(undefined), { once: true });
      socket.addEventListener("error", () => reject(new ProtocolClientError("connection_error", "connection error")), {
        once: true,
      });
      // 连接拒绝可能在 error 监听器注册前已经推进到 CLOSED；监听 close 并在注册后再检查一次，
      // 避免不可用 daemon 让前端一直等到完整握手超时。
      socket.addEventListener("close", () => reject(new ProtocolClientError("connection_closed", "connection closed")), {
        once: true,
      });
      if (socket.readyState === WebSocket.CLOSING || socket.readyState === WebSocket.CLOSED) {
        reject(new ProtocolClientError("connection_closed", "connection closed"));
      }
    }),
    timeoutMs,
    "connect_timeout",
  );
}

function abortedConnectionError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection closed");
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw abortedConnectionError();
  }
}

function withAbort<T>(promise: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) {
    return promise;
  }
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    const abort = () => reject(abortedConnectionError());
    signal.addEventListener("abort", abort, { once: true });
    promise.then(
      (value) => {
        signal.removeEventListener("abort", abort);
        resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", abort);
        reject(error);
      },
    );
  });
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
