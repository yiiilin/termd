import type { AddressInfo } from "node:net";
import { ed25519 } from "@noble/curves/ed25519";
import { WebSocketServer, type RawData, type WebSocket } from "ws";
import {
  authSigningInputBytes,
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  e2eeAuthTranscriptDigestWire,
  encodeEd25519Wire,
  verifyEd25519Signature,
} from "../protocol/auth";
import {
  E2eeSession,
  decodeBinaryEncryptedFrame,
  encodeBinaryEncryptedFrame,
  generateE2eeKeyPair,
  type E2eeKeyPair,
} from "../protocol/e2ee";
import {
  type BinaryProtocolPacket,
  decodeBinaryProtocolPacket,
  encodeBinaryProtocolPacket,
  terminalFrameBinaryToJson,
  terminalFrameJsonToBinary,
} from "../protocol/binary-packet";
import { fallbackSessionDisplayName } from "../session-names";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "../protocol/types";
import type {
  DaemonClientSummaryPayload,
  DaemonStatusResultPayload,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  PacketErrorPayload,
  PacketStreamId,
  PairRequestPayload,
  ProtocolPacket,
  RouteHelloPayload,
  SessionCreatePayload,
  SessionCreatedPayload,
  SessionCursorPayload,
  SessionDataPayload,
  SessionFileReadResultPayload,
  SessionFileWrittenPayload,
  SessionGitActionPayload,
  SessionGitDiffPayload,
  SessionGitDiffResultPayload,
  SessionFilesResultPayload,
  SessionGitResultPayload,
  SessionSearchMatchPayload,
  SessionSearchPayload,
  SessionSearchResultPayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "../protocol/types";
import {
  base64ToBytes,
  bytesToBase64,
  decodeUtf8,
  encodeUtf8,
  envelope,
  nonce,
  nowMs,
  parseEnvelope,
  randomUuid,
  sessionDataFromBase64,
  sessionDataToBase64,
} from "../protocol/wire";

interface MockDaemonOptions {
  token: string;
  sessions: Array<SessionSummaryPayload & { name?: string | null }>;
  attachOutput?: string;
  attachDelayMs?: number;
  sessionCreateDelayMs?: number;
  routePreludeError?: ErrorPayload;
  routeReadyDelayMs?: number;
  daemonPacketVersion?: number;
  pairFailure?: ErrorPayload;
  sessionDataError?: ErrorPayload;
  resizeAckDelayMs?: number;
  daemonClients?: DaemonClientSummaryPayload[];
  daemonClientsDelayMs?: number;
  dropDaemonClients?: boolean;
  daemonStatus?: DaemonStatusResultPayload;
  daemonStatusResponses?: DaemonStatusResultPayload[];
  daemonStatusDelayMs?: number;
  sessionFiles?: Record<UUID, SessionFilesResultPayload>;
  sessionGit?: Record<UUID, SessionGitResultPayload>;
  sessionFileReads?: Record<string, SessionFileReadResultPayload>;
  relayClientPathOnly?: boolean;
}

interface QueuedSessionListResponse {
  sessions: SessionSummaryPayload[];
  delayMs: number;
}

interface TrustedDevice {
  deviceId: UUID;
  devicePublicKey: string;
}

interface MockTerminalStream {
  sessionId: UUID;
  streamId: PacketStreamId;
  nextOutputSeq: number;
  watchUpdates: boolean;
}

interface MockConnection {
  socket: WebSocket;
  routed: boolean;
  deviceId?: UUID;
  e2ee?: E2eeSession;
  attachedSessionIds: Set<UUID>;
  watchedSessionIds: Set<UUID>;
  resizeOwnerSessionIds: Set<UUID>;
  terminalStreamsById: Map<PacketStreamId, MockTerminalStream>;
  terminalStreamsBySession: Map<UUID, MockTerminalStream>;
  daemonE2eeExchange?: E2eeKeyExchangePayload;
  e2eeAuthTranscriptSha256?: string;
  activeRequest?: ProtocolPacket;
  activeStreamId?: PacketStreamId;
  respondedToActiveRequest?: boolean;
  binaryMode?: boolean;
}

interface MockBinaryWireFrameLog {
  direction: "in" | "out";
  byteLength: number;
}

interface MockBinaryPacketLog {
  direction: "in" | "out";
  kind: string;
  payload_type?: string;
  data_text?: string;
}

export class MockDaemon {
  public readonly serverId: UUID;
  private readonly daemonSigningSecretKey = ed25519.utils.randomSecretKey();
  public readonly daemonPublicKey = encodeEd25519Wire(ed25519.getPublicKey(this.daemonSigningSecretKey));
  public readonly outerWireLog: string[] = [];
  public readonly binaryWireFrames: MockBinaryWireFrameLog[] = [];
  public readonly binaryPacketLog: MockBinaryPacketLog[] = [];
  public readonly receivedPackets: ProtocolPacket[] = [];
  public readonly sentPackets: ProtocolPacket[] = [];
  public readonly createdCommands: string[][] = [];
  public readonly sessionDataMessages: string[] = [];
  public readonly attachedSessions: UUID[] = [];
  public readonly attachRequests: Array<{ session_id: UUID; watch_updates?: boolean; last_terminal_seq?: number | null }> = [];
  public readonly sessionCursorUpdates: SessionCursorPayload[] = [];
  public readonly sessionResizes: Array<{ session_id: UUID; size: TerminalSize }> = [];
  public readonly sessionRenames: Array<{ session_id: UUID; name: string }> = [];
  public readonly sessionReorders: UUID[][] = [];
  public readonly closedSessions: UUID[] = [];
  public readonly sessionFileRequests: Array<{ session_id: UUID; path?: string | null }> = [];
  public readonly sessionFileReadRequests: Array<{ session_id: UUID; path: string }> = [];
  public readonly sessionFileDownloadPrepareRequests: Array<{ session_id: UUID; path: string }> = [];
  public readonly sessionFileDownloadChunkRequests: Array<{ session_id: UUID; path: string; offset_bytes: number; max_bytes: number }> = [];
  public readonly sessionFileWrites: Array<{ session_id: UUID; path: string; text: string }> = [];
  public readonly sessionFileDeletes: Array<{ session_id: UUID; path: string }> = [];
  public readonly sessionGitRequests: Array<{ session_id: UUID }> = [];
  public readonly sessionGitActions: SessionGitActionPayload[] = [];
  public readonly sessionGitDiffRequests: SessionGitDiffPayload[] = [];
  public readonly sessionSearchRequests: SessionSearchPayload[] = [];
  public daemonStatusRequests = 0;
  public pingMessages = 0;
  public acceptedConnections = 0;
  public failedTerminalAttachRequests = 0;
  public readonly decryptedInputs: string[] = [];
  public nextAttachRole = "operator" as const;
  private createdSessionCounter = 0;
  private failTerminalAttachRequests = 0;
  private failWatchedTerminalAttachRequests = 0;
  private readonly queuedSessionListResponses: QueuedSessionListResponse[] = [];
  private readonly e2eeKeypair: E2eeKeyPair;
  private readonly trustedDevices = new Map<UUID, TrustedDevice>();
  private readonly connections = new Set<MockConnection>();
  private readonly sessionFilePositions = new Map<UUID, string>();
  private readonly sessionOutputSnapshots = new Map<UUID, string>();

  private constructor(
    private readonly server: WebSocketServer,
    private readonly urlValue: string,
    private readonly options: MockDaemonOptions,
  ) {
    this.serverId = randomUuid();
    this.e2eeKeypair = generateE2eeKeyPair();
  }

  static async start(options: MockDaemonOptions): Promise<MockDaemon> {
    const server = new WebSocketServer({ port: 0, host: "127.0.0.1" });
    await new Promise<void>((resolve) => server.once("listening", resolve));
    const address = server.address() as AddressInfo;
    const daemon = new MockDaemon(server, `ws://127.0.0.1:${address.port}/ws`, options);
    server.on("connection", (socket, request) => daemon.accept(socket, request.url ?? ""));
    return daemon;
  }

  get url(): string {
    return this.urlValue;
  }

  activeConnectionCount(): number {
    return this.connections.size;
  }

  outerWireText(): string {
    return this.outerWireLog.join("\n");
  }

  forgetSession(sessionId: UUID): void {
    this.options.sessions = this.options.sessions.filter((session) => session.session_id !== sessionId);
  }

  setSessions(sessions: SessionSummaryPayload[]): void {
    // 测试另一个客户端已经改变 daemon 端权威列表时，当前浏览器下一次刷新必须服从 daemon 顺序。
    this.options.sessions = sessions;
  }

  failNextTerminalAttaches(count = 1): void {
    // 只让后续 terminal.attach 失败，用来稳定复现“重连尝试本身失败”的链路。
    // 失败发生在记录 attach 之前，测试可以清楚地区分失败尝试和真正成功 attach。
    this.failTerminalAttachRequests = Math.max(0, Math.floor(count));
  }

  failNextWatchedTerminalAttaches(count = 1): void {
    // 输出连接 watch_updates=true；只让这条 attach 失败，可以覆盖“控制连接已恢复但输出连接失败”
    // 之后还要继续排下一次重连的场景。
    this.failWatchedTerminalAttachRequests = Math.max(0, Math.floor(count));
  }

  queueSessionListResponse(sessions: SessionSummaryPayload[], delayMs = 0): void {
    // 用一次性响应模拟“旧请求稍后返回”的真实浏览器竞态。
    this.queuedSessionListResponses.push({ sessions, delayMs });
  }

  pushSessionFiles(files: SessionFilesResultPayload): void {
    this.sessionFilePositions.set(files.session_id, files.path);
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(files.session_id)) {
        this.sendInner(connection, envelope("session_files_result", files));
      }
    }
  }

  setSessionFilePosition(sessionId: UUID, path: string): void {
    // 测试轮询时只改变 daemon 端共享目录，不主动 push，才能确认前端真的发起了下一次 session_files。
    this.sessionFilePositions.set(sessionId, path);
  }

  pushSessionData(sessionId: UUID, text: string): void {
    this.appendSessionOutput(sessionId, text);
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(sessionId)) {
        this.sendInner(
          connection,
          envelope("session_data", {
            session_id: sessionId,
            data_base64: sessionDataToBase64(new TextEncoder().encode(text)),
          }),
        );
      }
    }
  }

  pushTerminalFrameBatch(sessionId: UUID, frames: unknown[]): void {
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(sessionId)) {
        this.sendTerminalStreamBatch(connection, sessionId, frames);
      }
    }
  }

  pushTerminalFrame(sessionId: UUID, frame: unknown): void {
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(sessionId)) {
        this.sendTerminalStreamFrame(connection, sessionId, frame);
      }
    }
  }

  pushSessionDataToAll(sessionId: UUID, text: string): void {
    // 后台 session 只发 activity 标记，不把未打开 session 的输出内容灌进当前 xterm。
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.size > 0) {
        void text;
        this.sendInner(connection, envelope("session_activity", { session_id: sessionId, timestamp_ms: nowMs() }));
      }
    }
  }

  async stop(): Promise<void> {
    this.server.clients.forEach((client) => client.close());
    await new Promise<void>((resolve, reject) => {
      this.server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  dropConnections(): void {
    // 移动端 PWA 切后台时系统可能只杀掉 WebSocket，而 daemon 本身仍然在线。
    this.server.clients.forEach((client) => client.close());
  }

  private accept(socket: WebSocket, requestPath: string): void {
    const pathname = requestPath.split("?")[0] || requestPath;
    if (this.options.relayClientPathOnly && pathname !== "/ws") {
      // 旧版 path-based client URL 已移除；mock 用这个开关确保前端只连接统一 /ws 入口。
      socket.close();
      return;
    }

    const connection: MockConnection = {
      socket,
      routed: false,
      attachedSessionIds: new Set(),
      watchedSessionIds: new Set(),
      resizeOwnerSessionIds: new Set(),
      terminalStreamsById: new Map(),
      terminalStreamsBySession: new Map(),
    };
    this.connections.add(connection);
    this.acceptedConnections += 1;
    socket.on("close", () => {
      this.connections.delete(connection);
      for (const sessionId of connection.resizeOwnerSessionIds) {
        this.promoteResizeOwner(sessionId);
      }
    });

    socket.on("message", (raw, isBinary) => {
      if (isBinary) {
        void this.handleOuterBinary(connection, bytesFromWsMessage(raw));
        return;
      }
      void this.handleOuter(connection, raw.toString());
    });
  }

  private signedDaemonE2eeExchange(): E2eeKeyExchangePayload {
    const payload: E2eeKeyExchangePayload = {
      server_id: this.serverId,
      device_id: "00000000-0000-0000-0000-000000000000",
      public_key: this.e2eeKeypair.publicKeyWire,
      nonce: nonce(),
      timestamp_ms: nowMs(),
      packet_version: this.options.daemonPacketVersion ?? PROTOCOL_PACKET_VERSION,
      binary_version: BINARY_PROTOCOL_VERSION,
    };
    const signature = ed25519.sign(
      daemonE2eeSigningInputBytes(payload, {
        server_id: this.serverId,
        daemon_public_key: this.daemonPublicKey,
      }),
      this.daemonSigningSecretKey,
    );
    return { ...payload, signature: encodeEd25519Wire(signature) };
  }

  private async handleOuter(connection: MockConnection, raw: string): Promise<void> {
    this.outerWireLog.push(raw);
    const outer = parseEnvelope(raw);

    if (!connection.routed) {
      this.handleRoutePrelude(connection, outer);
      return;
    }

    if (outer.type === "e2ee_key_exchange") {
      const payload = outer.payload as E2eeKeyExchangePayload;
      if (payload.packet_version !== PROTOCOL_PACKET_VERSION || !connection.daemonE2eeExchange) {
        this.sendError(connection, "unsupported_protocol_version", "unsupported protocol version");
        return;
      }
      connection.deviceId = payload.device_id;
      connection.e2ee = E2eeSession.daemon({
        serverId: this.serverId,
        deviceId: payload.device_id,
        localKeypair: this.e2eeKeypair,
        devicePublicKeyWire: payload.public_key,
      });
      connection.e2eeAuthTranscriptSha256 = e2eeAuthTranscriptDigestWire(
        connection.daemonE2eeExchange,
        payload,
        {
          server_id: this.serverId,
          daemon_public_key: this.daemonPublicKey,
        },
      );
      connection.binaryMode =
        connection.daemonE2eeExchange.binary_version === BINARY_PROTOCOL_VERSION &&
        payload.binary_version === BINARY_PROTOCOL_VERSION;

      if (this.trustedDevices.has(payload.device_id)) {
        this.sendPacket(
          connection,
          {
            version: PROTOCOL_PACKET_VERSION,
            kind: "event",
            method: "auth.challenge",
            payload: {
              device_id: payload.device_id,
              challenge: `challenge-${payload.device_id}`,
              expires_at_ms: nowMs() + 60_000,
            },
          },
        );
      }
      return;
    }

    if (outer.type !== "encrypted_frame" || !connection.e2ee) {
      this.sendError(connection, "invalid_state", "invalid protocol state");
      return;
    }

    const inner = connection.e2ee.decryptJson(outer.payload as EncryptedFramePayload);
    await this.handleInner(connection, inner);
  }

  private async handleOuterBinary(connection: MockConnection, raw: Uint8Array): Promise<void> {
    this.binaryWireFrames.push({ direction: "in", byteLength: raw.byteLength });
    if (!connection.e2ee || !connection.binaryMode) {
      this.sendError(connection, "invalid_state", "invalid protocol state");
      return;
    }
    const plaintext = connection.e2ee.decryptBinary(decodeBinaryEncryptedFrame(raw));
    const binaryPacket = decodeBinaryProtocolPacket(plaintext);
    this.recordBinaryPacket("in", binaryPacket);
    await this.handlePacket(connection, binaryPacketToProtocol(binaryPacket));
  }

  private handleRoutePrelude(connection: MockConnection, outer: Envelope): void {
    if (outer.type !== "route_hello") {
      this.sendError(connection, "invalid_route_prelude", "invalid route prelude");
      return;
    }

    const payload = outer.payload as RouteHelloPayload;
    if (payload.server_id !== this.serverId || payload.role !== "client" || payload.protocol_version !== PROTOCOL_PACKET_VERSION) {
      this.sendError(connection, "invalid_route_prelude", "invalid route prelude");
      return;
    }

    if (this.options.routePreludeError) {
      // 模拟 daemon/relay 在 E2EE 建立前直接返回外层 error envelope 的失败路径。
      this.sendError(connection, this.options.routePreludeError.code, this.options.routePreludeError.message);
      return;
    }

    connection.routed = true;
    const sendPrelude = () => {
      const daemonE2eeExchange = this.signedDaemonE2eeExchange();
      connection.daemonE2eeExchange = daemonE2eeExchange;
      this.sendOuter(
        connection.socket,
        envelope("route_ready", {
          server_id: this.serverId,
          role: "client",
        }),
      );
      this.sendOuter(
        connection.socket,
        envelope("hello", {
          protocol_version: PROTOCOL_PACKET_VERSION,
          nonce: nonce(),
          timestamp_ms: nowMs(),
          server_id: this.serverId,
          device_id: null,
        }),
      );
      this.sendOuter(
        connection.socket,
        envelope("e2ee_key_exchange", daemonE2eeExchange),
      );
    };
    if (this.options.routeReadyDelayMs) {
      setTimeout(sendPrelude, this.options.routeReadyDelayMs);
      return;
    }
    sendPrelude();
  }

  private async handleInner(connection: MockConnection, inner: Envelope): Promise<void> {
    if (inner.type !== "packet") {
      this.sendError(connection, "invalid_packet", "expected protocol packet");
      return;
    }
    await this.handlePacket(connection, inner.payload as ProtocolPacket);
  }

  private async handlePacket(connection: MockConnection, packet: ProtocolPacket): Promise<void> {
    this.receivedPackets.push(packet);
    if (packet.version !== PROTOCOL_PACKET_VERSION) {
      this.sendPacketError(connection, packet, "unsupported_protocol_version", "unsupported protocol packet version");
      return;
    }

    if (packet.kind === "flow") {
      return;
    }
    if (packet.kind === "cancel") {
      this.removeTerminalStream(connection, packet.stream_id);
      return;
    }
    if (packet.kind === "stream_chunk") {
      await this.handlePacketStreamChunk(connection, packet);
      return;
    }
    if (packet.kind !== "request" && packet.kind !== "stream_open") {
      this.sendPacketError(connection, packet, "invalid_packet", "invalid protocol packet");
      return;
    }
    if (packet.kind === "request" && await this.handleDirectPacketRequest(connection, packet)) {
      return;
    }

    const legacy = this.packetToLegacyEnvelope(packet);
    if (!legacy) {
      this.sendPacketError(connection, packet, "unknown_method", "unknown protocol method");
      return;
    }

    connection.activeRequest = packet;
    connection.respondedToActiveRequest = false;
    try {
      await this.handleLegacyInner(connection, legacy);
      if (!connection.respondedToActiveRequest && this.packetMethodNeedsEmptyAck(packet.method)) {
        this.sendPacketResponse(connection, packet, {});
      }
    } finally {
      connection.activeRequest = undefined;
      connection.respondedToActiveRequest = false;
    }
  }

  private async handlePacketStreamChunk(connection: MockConnection, packet: ProtocolPacket): Promise<void> {
    if (!packet.stream_id || !connection.terminalStreamsById.has(packet.stream_id)) {
      this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
      return;
    }
    connection.activeStreamId = packet.stream_id;
    try {
      await this.handleLegacyInner(connection, envelope("session_data", packet.payload));
    } finally {
      connection.activeStreamId = undefined;
    }
  }

  private async handleDirectPacketRequest(connection: MockConnection, packet: ProtocolPacket): Promise<boolean> {
    switch (packet.method) {
      case "session.list": {
        const queued = this.queuedSessionListResponses.shift();
        if (queued?.delayMs) {
          await new Promise((resolve) => setTimeout(resolve, queued.delayMs));
        }
        this.sendPacketResponse(connection, packet, { sessions: queued?.sessions ?? this.options.sessions });
        return true;
      }
      case "session.close": {
        const payload = packet.payload as { session_id: UUID };
        const sessionExists = this.options.sessions.some((session) => session.session_id === payload.session_id);
        if (!sessionExists) {
          this.sendPacketError(connection, packet, "session_not_found", "session was not found");
          return true;
        }
        this.closedSessions.push(payload.session_id);
        this.options.sessions = this.options.sessions.filter((session) => session.session_id !== payload.session_id);
        this.sendPacketResponse(connection, packet, payload);
        return true;
      }
      default:
        return false;
    }
  }

  private packetToLegacyEnvelope(packet: ProtocolPacket): Envelope | undefined {
    const payload = packet.payload;
    switch (packet.method) {
      case "pair.request":
        return envelope("pair_request", payload);
      case "auth":
      case "auth.verify":
        return envelope("auth", payload);
      case "client.hello":
        return envelope("client_hello", payload);
      case "session.list":
        return envelope("session_list", payload);
      case "daemon.clients":
        return envelope("daemon_clients", payload);
      case "daemon.client_forget":
        return envelope("daemon_client_forget", payload);
      case "daemon.status":
        return envelope("daemon_status", payload);
      case "terminal.create":
        return envelope("session_create", payload);
      case "terminal.attach":
        return envelope("session_attach", payload);
      case "session.cursor":
        return envelope("session_cursor", payload);
      case "session.resize":
        return envelope("session_resize", payload);
      case "session.rename":
        return envelope("session_rename", payload);
      case "session.reorder":
        return envelope("session_reorder", payload);
      case "session.close":
        return envelope("session_close", payload);
      case "session.files":
        return envelope("session_files", payload);
      case "session.search":
        return envelope("session_search", payload);
      case "session.git":
        return envelope("session_git", payload);
      case "session.git_diff":
        return envelope("session_git_diff", payload);
      case "session.git_action":
        return envelope("session_git_action", payload);
      case "session.file_read":
        return envelope("session_file_read", payload);
      case "session.file_download_prepare":
        return envelope("session_file_download_prepare", payload);
      case "session.file_download_chunk":
        return envelope("session_file_download_chunk", payload);
      case "session.file_write":
        return envelope("session_file_write", payload);
      case "session.file_delete":
        return envelope("session_file_delete", payload);
      case "control.request":
        return envelope("control_request", payload);
      case "ping":
        return envelope("ping", payload);
      default:
        return undefined;
    }
  }

  private packetMethodNeedsEmptyAck(method?: string): boolean {
    return method === "auth" || method === "auth.verify" || method === "client.hello" || method === "session.cursor";
  }

  private async handleLegacyInner(connection: MockConnection, inner: Envelope): Promise<void> {
    switch (inner.type) {
      case "pair_request":
        this.handlePairRequest(connection, inner.payload as PairRequestPayload);
        return;
      case "auth":
        await this.handleAuth(connection, inner.payload as Record<string, unknown>);
        return;
      case "client_hello":
        this.handleClientHello(connection, inner.payload as { name: string });
        return;
      case "session_list": {
        const queued = this.queuedSessionListResponses.shift();
        if (queued?.delayMs) {
          await new Promise((resolve) => setTimeout(resolve, queued.delayMs));
        }
        this.sendInner(
          connection,
          envelope("session_list_result", { sessions: queued?.sessions ?? this.options.sessions }),
        );
        return;
      }
      case "daemon_clients": {
        if (this.options.daemonClientsDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.daemonClientsDelayMs));
        }
        if (this.options.dropDaemonClients) {
          return;
        }
        this.sendInner(
          connection,
          envelope("daemon_clients_result", {
            clients: this.options.daemonClients ?? [],
          }),
        );
        return;
      }
      case "daemon_client_forget": {
        const payload = inner.payload as { device_id: UUID };
        const client = this.options.daemonClients?.find((candidate) => candidate.device_id === payload.device_id);
        if (client?.online) {
          this.sendError(connection, "invalid_state", "invalid protocol state");
          return;
        }
        // daemon 端删除离线客户端是幂等操作；测试桩也保持一致，覆盖连点删除的竞态。
        this.options.daemonClients = this.options.daemonClients?.filter(
          (candidate) => candidate.device_id !== payload.device_id,
        );
        this.sendInner(connection, envelope("daemon_client_forgot", payload));
        return;
      }
      case "daemon_status": {
        this.daemonStatusRequests += 1;
        if (this.options.daemonStatusDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.daemonStatusDelayMs));
        }
        const queuedStatus = this.options.daemonStatusResponses?.shift();
        this.sendInner(connection, envelope("daemon_status_result", queuedStatus ?? this.options.daemonStatus ?? mockDaemonStatus()));
        return;
      }
      case "session_create":
        if (this.options.sessionCreateDelayMs) {
          // 中文注释：真实 daemon 创建 shell 需要拉起 supervisor/PTY；测试用延迟覆盖
          // “创建 session 不是普通短 RPC”这个超时语义。
          await new Promise((resolve) => setTimeout(resolve, this.options.sessionCreateDelayMs));
        }
        this.handleSessionCreate(connection, inner.payload as SessionCreatePayload);
        return;
      case "session_attach": {
        const payload = inner.payload as { session_id: UUID; watch_updates?: boolean; last_terminal_seq?: number | null };
        const watchUpdates = payload.watch_updates ?? true;
        if (this.failTerminalAttachRequests > 0) {
          this.failTerminalAttachRequests -= 1;
          this.failedTerminalAttachRequests += 1;
          this.sendError(connection, "connection_closed", "mock terminal attach closed");
          return;
        }
        if (watchUpdates && this.failWatchedTerminalAttachRequests > 0) {
          this.failWatchedTerminalAttachRequests -= 1;
          this.failedTerminalAttachRequests += 1;
          this.sendError(connection, "connection_closed", "mock watched terminal attach closed");
          return;
        }
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        this.attachRequests.push(payload);
        this.attachedSessions.push(payload.session_id);
        connection.attachedSessionIds.add(payload.session_id);
        if (watchUpdates) {
          connection.watchedSessionIds.add(payload.session_id);
        }
        if (this.options.attachDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.attachDelayMs));
        }
        this.sendInner(
          connection,
          envelope("session_attached", {
            session_id: payload.session_id,
            role: this.nextAttachRole,
            state: session?.state ?? "running",
            size: session?.size ?? { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            resize_owner: watchUpdates ? this.assignResizeOwner(connection, payload.session_id) : false,
          }),
        );
        if (watchUpdates && this.options.attachOutput) {
          if (!this.sessionOutputSnapshots.has(payload.session_id)) {
            this.appendSessionOutput(payload.session_id, this.options.attachOutput);
          }
          this.sendInner(
            connection,
            envelope("session_data", {
              session_id: payload.session_id,
              data_base64: sessionDataToBase64(new TextEncoder().encode(this.options.attachOutput)),
            }),
          );
        }
        return;
      }
      case "session_data": {
        const payload = inner.payload as SessionDataPayload;
        const input = decodeUtf8(sessionDataFromBase64(payload.data_base64 ?? ""));
        this.sessionDataMessages.push(input);
        if (this.options.sessionDataError) {
          // 拒绝路径只记录收到的加密业务帧，不模拟写入 PTY。
          this.sendError(connection, this.options.sessionDataError.code, this.options.sessionDataError.message);
          return;
        }
        this.decryptedInputs.push(input);
        return;
      }
      case "session_cursor": {
        this.sessionCursorUpdates.push(inner.payload as SessionCursorPayload);
        return;
      }
      case "session_resize": {
        const payload = inner.payload as { session_id: UUID; size: TerminalSize };
        if (!connection.resizeOwnerSessionIds.has(payload.session_id)) {
          this.sendError(connection, "invalid_state", "invalid protocol state");
          return;
        }
        this.sessionResizes.push(payload);
        if (this.options.resizeAckDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.resizeAckDelayMs));
        }
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        if (session) {
          session.size = payload.size;
        }
        this.broadcastSessionResized(payload.session_id, payload.size);
        return;
      }
      case "session_rename": {
        const payload = inner.payload as { session_id: UUID; name: string };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionRenames.push(payload);
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        if (session) {
          session.name = payload.name;
        }
        this.sendInner(connection, envelope("session_renamed", payload));
        return;
      }
      case "session_reorder": {
        const payload = inner.payload as { session_ids: UUID[] };
        this.sessionReorders.push(payload.session_ids);
        const byId = new Map(this.options.sessions.map((session) => [session.session_id, session]));
        const ordered = payload.session_ids
          .map((sessionId) => byId.get(sessionId))
          .filter((session): session is SessionSummaryPayload => Boolean(session));
        const orderedIds = new Set(ordered.map((session) => session.session_id));
        this.options.sessions = [
          ...ordered,
          ...this.options.sessions.filter((session) => !orderedIds.has(session.session_id)),
        ];
        this.sendInner(connection, envelope("session_reordered", { session_ids: this.options.sessions.map((session) => session.session_id) }));
        return;
      }
      case "session_close": {
        const payload = inner.payload as { session_id: UUID };
        const sessionExists = this.options.sessions.some((session) => session.session_id === payload.session_id);
        if (!sessionExists) {
          this.sendError(connection, "session_not_found", "session was not found");
          return;
        }
        this.closedSessions.push(payload.session_id);
        this.options.sessions = this.options.sessions.filter((session) => session.session_id !== payload.session_id);
        this.sendInner(connection, envelope("session_closed", payload));
        return;
      }
      case "session_files": {
        const payload = inner.payload as { session_id: UUID; path?: string | null };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileRequests.push(payload);
        // 指定 path 时必须按该目录返回，避免测试里把“任意切换目录”误回退成 session 根目录。
        const lookupPath =
          payload.path && payload.path.trim()
            ? payload.path
            : this.sessionFilePositions.get(payload.session_id) ?? payload.session_id;
        const files = this.options.sessionFiles?.[lookupPath];
        if (files) {
          this.sessionFilePositions.set(payload.session_id, files.path);
        }
        this.sendInner(
          connection,
          envelope(
            "session_files_result",
            files ?? {
              session_id: payload.session_id,
              path: payload.path ?? this.sessionFilePositions.get(payload.session_id) ?? "",
              entries: [],
            },
          ),
        );
        return;
      }
      case "session_search": {
        const payload = inner.payload as SessionSearchPayload;
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionSearchRequests.push(payload);
        const result = mockSessionSearchResult(payload, this.sessionOutputSnapshots.get(payload.session_id) ?? "");
        this.sendInner(connection, envelope("session_search_result", result));
        return;
      }
      case "session_git": {
        const payload = inner.payload as { session_id: UUID };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionGitRequests.push(payload);
        this.sendInner(
          connection,
          envelope("session_git_result", this.options.sessionGit?.[payload.session_id] ?? defaultSessionGit(payload.session_id)),
        );
        return;
      }
      case "session_git_diff": {
        const payload = inner.payload as SessionGitDiffPayload;
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionGitDiffRequests.push(payload);
        this.sendInner(connection, envelope("session_git_diff_result", mockSessionGitDiffResult(payload)));
        return;
      }
      case "session_git_action": {
        const payload = inner.payload as SessionGitActionPayload;
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionGitActions.push(payload);
        this.sendInner(connection, envelope("session_git_action_result", payload));
        return;
      }
      case "session_file_read": {
        const payload = inner.payload as { session_id: UUID; path: string };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileReadRequests.push(payload);
        const result =
          this.options.sessionFileReads?.[payload.path] ??
          ({
            session_id: payload.session_id,
            path: payload.path,
            data_base64: sessionDataToBase64(new TextEncoder().encode("downloaded mock file\n")),
            size_bytes: 21,
            modified_at_ms: null,
          } satisfies SessionFileReadResultPayload);
        this.sendInner(connection, envelope("session_file_read_result", result));
        return;
      }
      case "session_file_download_prepare": {
        const payload = inner.payload as { session_id: UUID; path: string };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileDownloadPrepareRequests.push(payload);
        this.sendInner(
          connection,
          envelope("session_file_download_ready", {
            session_id: payload.session_id,
            path: payload.path,
            token: `mock-download-${this.sessionFileDownloadPrepareRequests.length}`,
            size_bytes: this.options.sessionFileReads?.[payload.path]?.size_bytes ?? 21,
            modified_at_ms: this.options.sessionFileReads?.[payload.path]?.modified_at_ms ?? null,
            expires_at_ms: nowMs() + 60_000,
          }),
        );
        return;
      }
      case "session_file_download_chunk": {
        const payload = inner.payload as { session_id: UUID; path: string; offset_bytes: number; max_bytes: number };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileDownloadChunkRequests.push(payload);
        const source =
          this.options.sessionFileReads?.[payload.path]?.data_base64 ??
          sessionDataToBase64(new TextEncoder().encode("downloaded mock file\n"));
        const allBytes = sessionDataFromBase64(source);
        const start = Math.max(0, payload.offset_bytes);
        const end = Math.min(allBytes.byteLength, start + Math.max(0, payload.max_bytes));
        const bytes = allBytes.slice(start, end);
        this.sendInner(
          connection,
          envelope("session_file_download_chunk_result", {
            session_id: payload.session_id,
            path: payload.path,
            offset_bytes: start,
            data_base64: sessionDataToBase64(bytes),
            next_offset_bytes: end,
            size_bytes: allBytes.byteLength,
            eof: end >= allBytes.byteLength,
            modified_at_ms: this.options.sessionFileReads?.[payload.path]?.modified_at_ms ?? null,
          }),
        );
        return;
      }
      case "session_file_write": {
        const payload = inner.payload as { session_id: UUID; path: string; data_base64: string };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        const bytes = sessionDataFromBase64(payload.data_base64);
        this.sessionFileWrites.push({
          session_id: payload.session_id,
          path: payload.path,
          text: decodeUtf8(bytes),
        });
        this.applyMockFileWrite(payload.session_id, payload.path);
        this.sendInner(
          connection,
          envelope("session_file_written", {
            session_id: payload.session_id,
            path: payload.path,
            size_bytes: bytes.byteLength,
            modified_at_ms: null,
          } satisfies SessionFileWrittenPayload),
        );
        return;
      }
      case "session_file_delete": {
        const payload = inner.payload as { session_id: UUID; path: string };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileDeletes.push(payload);
        this.applyMockFileDelete(payload.session_id, payload.path);
        this.sendInner(connection, envelope("session_file_deleted", payload));
        return;
      }
      case "control_request": {
        const payload = inner.payload as { session_id: UUID; device_id: UUID };
        this.sendInner(connection, envelope("control_grant", payload));
        return;
      }
      case "ping": {
        const payload = inner.payload as { nonce: string };
        this.pingMessages += 1;
        this.sendInner(connection, envelope("pong", { nonce: payload.nonce, timestamp_ms: nowMs() }));
        return;
      }
      default:
        this.sendError(connection, "invalid_state", "invalid protocol state");
    }
  }

  private handleSessionCreate(connection: MockConnection, payload: SessionCreatePayload): void {
    this.createdCommands.push(payload.command);
    this.createdSessionCounter += 1;
    const sessionId = `00000000-0000-0000-0000-${String(500 + this.createdSessionCounter).padStart(12, "0")}`;
    const name = fallbackSessionDisplayName(sessionId);
    const created = {
      session_id: sessionId,
      name,
      role: this.nextAttachRole,
      state: "running",
      size: payload.size,
      resize_owner: this.assignResizeOwner(connection, sessionId),
    } satisfies SessionCreatedPayload;

    // mock daemon 模拟真实 daemon：session_create 会立刻 attach 当前连接。
    this.options.sessions.unshift({
      session_id: created.session_id,
      name,
      state: created.state,
      size: created.size,
      created_at_ms: nowMs(),
    });
    connection.attachedSessionIds.add(created.session_id);
    connection.watchedSessionIds.add(created.session_id);
    this.sendInner(connection, envelope("session_created", created));
    if (this.options.attachOutput) {
      this.appendSessionOutput(created.session_id, this.options.attachOutput);
      this.sendInner(
        connection,
        envelope("session_data", {
          session_id: created.session_id,
          data_base64: sessionDataToBase64(new TextEncoder().encode(this.options.attachOutput)),
        }),
      );
    }
  }

  private handleClientHello(connection: MockConnection, payload: { name: string }): void {
    if (!connection.deviceId) {
      return;
    }
    const client = this.options.daemonClients?.find((candidate) => candidate.device_id === connection.deviceId);
    if (client) {
      client.name = payload.name;
    }
  }

  private appendSessionOutput(sessionId: UUID, text: string): void {
    const current = this.sessionOutputSnapshots.get(sessionId) ?? "";
    this.sessionOutputSnapshots.set(sessionId, `${current}${text}`);
  }

  private applyMockFileWrite(sessionId: UUID, path: string): void {
    const parent = parentDirectory(path);
    const record = this.findSessionFilesRecord(sessionId, parent);
    if (!record) {
      return;
    }

    const index = record.entries.findIndex((entry) => entry.path === path);
    const nextEntry = {
      name: basenamePath(path),
      path,
      kind: "file" as const,
      size_bytes: index >= 0 ? record.entries[index].size_bytes : 0,
      modified_at_ms: null,
    };

    if (index >= 0) {
      record.entries[index] = nextEntry;
      return;
    }
    record.entries.push(nextEntry);
  }

  private applyMockFileDelete(sessionId: UUID, path: string): void {
    const parent = parentDirectory(path);
    const record = this.findSessionFilesRecord(sessionId, parent);
    if (!record) {
      return;
    }

    record.entries = record.entries.filter((entry) => entry.path !== path);
  }

  private findSessionFilesRecord(sessionId: UUID, path: string): SessionFilesResultPayload | undefined {
    return Object.values(this.options.sessionFiles ?? {}).find(
      (record) => record.session_id === sessionId && record.path === path,
    );
  }

  private ensureAttached(connection: MockConnection, sessionId: UUID): boolean {
    if (connection.attachedSessionIds.has(sessionId)) {
      return true;
    }
    // 测试桩和真实 daemon 保持一致：session 级操作必须来自已 attach 的连接。
    this.sendError(connection, "invalid_state", "invalid protocol state");
    return false;
  }

  private assignResizeOwner(connection: MockConnection, sessionId: UUID): boolean {
    for (const candidate of this.connections) {
      if (candidate !== connection && candidate.e2ee && candidate.resizeOwnerSessionIds.has(sessionId)) {
        return false;
      }
    }
    connection.resizeOwnerSessionIds.add(sessionId);
    return true;
  }

  private promoteResizeOwner(sessionId: UUID): void {
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(sessionId)) {
        connection.resizeOwnerSessionIds.add(sessionId);
        const session = this.options.sessions.find((candidate) => candidate.session_id === sessionId);
        this.sendInner(
          connection,
          envelope("session_resized", {
            session_id: sessionId,
            size: session?.size ?? { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            resize_owner: true,
          }),
        );
        return;
      }
    }
  }

  private broadcastSessionResized(sessionId: UUID, size: TerminalSize): void {
    for (const connection of this.connections) {
      if (connection.e2ee && connection.watchedSessionIds.has(sessionId)) {
        this.sendInner(
          connection,
          envelope("session_resized", {
            session_id: sessionId,
            size,
            resize_owner: connection.resizeOwnerSessionIds.has(sessionId),
          }),
        );
      }
    }
  }

  private handlePairRequest(connection: MockConnection, payload: PairRequestPayload): void {
    if (payload.token !== this.options.token || payload.device_id !== connection.deviceId) {
      const failure = this.options.pairFailure ?? { code: "pairing_failed", message: "pairing failed" };
      this.sendError(connection, failure.code, failure.message);
      return;
    }
    this.trustedDevices.set(payload.device_id, {
      deviceId: payload.device_id,
      devicePublicKey: payload.device_public_key,
    });
    this.sendInner(
      connection,
      envelope("pair_accept", {
        server_id: this.serverId,
        daemon_public_key: this.daemonPublicKey,
        device_id: payload.device_id,
        expires_at_ms: nowMs() + 60_000,
      }),
    );
  }

  private async handleAuth(connection: MockConnection, payload: Record<string, unknown>): Promise<void> {
    const device = this.trustedDevices.get(String(payload.device_id));
    if (!device) {
      this.sendError(connection, "auth_failed", "auth failed");
      return;
    }
    const authPayload = payload as never as Parameters<typeof authSigningInputBytes>[0];
    const ok = await verifyEd25519Signature(
      decodeEd25519PublicKey(device.devicePublicKey),
      authSigningInputBytes(authPayload, {
        server_id: this.serverId,
        daemon_public_key: this.daemonPublicKey,
      }, connection.e2eeAuthTranscriptSha256),
      String(payload.signature),
    );
    if (!ok) {
      this.sendError(connection, "auth_failed", "auth failed");
    }
  }

  private sendInner(connection: MockConnection, inner: Envelope): void {
    if (!connection.e2ee) {
      this.sendError(connection, "invalid_state", "invalid protocol state");
      return;
    }
    if (inner.type === "session_data") {
      this.sendTerminalStreamChunk(connection, inner.payload as SessionDataPayload);
      return;
    }

    const activeRequest = connection.activeRequest;
    if (activeRequest && !connection.respondedToActiveRequest) {
      this.sendPacketResponse(connection, activeRequest, inner.payload);
      return;
    }

    this.sendPacketEvent(connection, this.legacyEventMethod(inner.type), inner.payload);
  }

  private sendError(connection: MockConnection, code: string, message: string): void {
    if (connection.e2ee) {
      this.sendPacketError(connection, connection.activeRequest, code, message);
      return;
    }
    const error = envelope("error", { code, message } satisfies ErrorPayload);
    this.sendOuter(connection.socket, error);
  }

  private sendPacketResponse(connection: MockConnection, request: ProtocolPacket, payload: unknown): void {
    if (!request.id || !request.method) {
      this.sendPacketError(connection, request, "invalid_packet", "invalid protocol packet");
      return;
    }
    this.registerTerminalStreamForResponse(connection, request, payload);
    connection.respondedToActiveRequest = true;
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "response",
      id: request.id,
      ...(request.stream_id ? { stream_id: request.stream_id } : {}),
      method: request.method,
      payload,
    });
  }

  private sendPacketEvent(connection: MockConnection, method: string, payload: unknown): void {
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "event",
      method,
      payload,
    });
  }

  private sendPacketError(
    connection: MockConnection,
    request: ProtocolPacket | undefined,
    code: string,
    message: string,
  ): void {
    const packet: ProtocolPacket<PacketErrorPayload> = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "error",
      ...(request?.id ? { id: request.id } : {}),
      ...(request?.stream_id ? { stream_id: request.stream_id } : connection.activeStreamId ? { stream_id: connection.activeStreamId } : {}),
      payload: { code, message, retryable: false },
    };
    connection.respondedToActiveRequest = true;
    this.sendPacket(connection, packet);
  }

  private sendPacket(connection: MockConnection, packet: ProtocolPacket): void {
    if (!connection.e2ee) {
      this.sendOuter(connection.socket, envelope("error", { code: "invalid_state", message: "invalid protocol state" } satisfies ErrorPayload));
      return;
    }
    this.sentPackets.push(packet);
    if (connection.binaryMode) {
      const binaryPacket = protocolPacketToBinary(packet);
      this.recordBinaryPacket("out", binaryPacket);
      const frame = connection.e2ee.encryptBinary(encodeBinaryProtocolPacket(binaryPacket));
      const wire = encodeBinaryEncryptedFrame(frame);
      this.binaryWireFrames.push({ direction: "out", byteLength: wire.byteLength });
      connection.socket.send(wire);
      return;
    }
    this.sendOuter(connection.socket, envelope("encrypted_frame", connection.e2ee.encryptJson(envelope("packet", packet))));
  }

  private recordBinaryPacket(direction: "in" | "out", packet: BinaryProtocolPacket): void {
    if (packet.payload?.type === "session_data") {
      this.binaryPacketLog.push({
        direction,
        kind: packet.kind,
        payload_type: packet.payload.type,
        data_text: decodeUtf8(packet.payload.data),
      });
      return;
    }
    if (packet.payload?.type === "terminal_frame") {
      const frame = packet.payload.frame;
      this.binaryPacketLog.push({
        direction,
        kind: packet.kind,
        payload_type: packet.payload.type,
        data_text: frame.kind === "snapshot" || frame.kind === "output" ? decodeUtf8(frame.data) : undefined,
      });
      return;
    }
    this.binaryPacketLog.push({
      direction,
      kind: packet.kind,
      payload_type: packet.payload?.type,
    });
  }

  private sendTerminalStreamChunk(connection: MockConnection, payload: SessionDataPayload): void {
    const stream = connection.terminalStreamsBySession.get(payload.session_id);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    const seq = stream.nextOutputSeq;
    stream.nextOutputSeq += 1;
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: stream.streamId,
      seq,
      payload,
    });
  }

  private sendTerminalStreamFrame(connection: MockConnection, sessionId: UUID, payload: unknown): void {
    const stream = connection.terminalStreamsBySession.get(sessionId);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    const seq = stream.nextOutputSeq;
    stream.nextOutputSeq += 1;
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: stream.streamId,
      seq,
      payload,
    });
  }

  private sendTerminalStreamBatch(connection: MockConnection, sessionId: UUID, frames: unknown[]): void {
    const stream = connection.terminalStreamsBySession.get(sessionId);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    const seq = stream.nextOutputSeq;
    stream.nextOutputSeq += 1;
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: stream.streamId,
      seq,
      payload: {
        kind: "batch",
        session_id: sessionId,
        frames,
      },
    });
  }

  private registerTerminalStreamForResponse(connection: MockConnection, request: ProtocolPacket, payload: unknown): void {
    if (request.kind !== "stream_open" || !request.stream_id || !String(request.method ?? "").startsWith("terminal.")) {
      return;
    }
    const response = payload as { session_id?: UUID };
    if (!response.session_id) {
      return;
    }
    const requestPayload = request.payload as { watch_updates?: boolean };
    const watchUpdates = request.method === "terminal.attach" ? requestPayload.watch_updates ?? true : true;
    const stream: MockTerminalStream = {
      sessionId: response.session_id,
      streamId: request.stream_id,
      nextOutputSeq: 1,
      watchUpdates,
    };
    connection.terminalStreamsById.set(stream.streamId, stream);
    connection.terminalStreamsBySession.set(stream.sessionId, stream);
  }

  private removeTerminalStream(connection: MockConnection, streamId?: PacketStreamId): void {
    if (!streamId) {
      return;
    }
    const stream = connection.terminalStreamsById.get(streamId);
    if (!stream) {
      return;
    }
    connection.terminalStreamsById.delete(streamId);
    connection.terminalStreamsBySession.delete(stream.sessionId);
    connection.watchedSessionIds.delete(stream.sessionId);
    connection.resizeOwnerSessionIds.delete(stream.sessionId);
  }

  private legacyEventMethod(type: Envelope["type"]): string {
    switch (type) {
      case "auth_challenge":
        return "auth.challenge";
      case "session_activity":
        return "session.activity";
      case "session_files_result":
        return "session.files";
      case "session_git_result":
        return "session.git";
      case "session_resized":
        return "session.resized";
      case "session_data":
        return "terminal.output";
      default:
        return type.replaceAll("_", ".");
    }
  }

  private sendOuter(socket: WebSocket, outer: Envelope): void {
    socket.send(JSON.stringify(outer));
  }
}

function mockDaemonStatus(): DaemonStatusResultPayload {
  // mock 只表达协议形状；真实采集由 daemon 端 /proc/statvfs 实现。
  return {
    host_name: "mock-daemon",
    load_avg: [0.12, 0.08, 0.03],
    uptime_seconds: 3600,
    cpu_percent: 7.5,
    memory_total_bytes: 8 * 1024 * 1024 * 1024,
    memory_available_bytes: 5 * 1024 * 1024 * 1024,
    disk_total_bytes: 128 * 1024 * 1024 * 1024,
    disk_available_bytes: 64 * 1024 * 1024 * 1024,
    network_rx_bytes: 24 * 1024 * 1024,
    network_tx_bytes: 6 * 1024 * 1024,
    process_count: 123,
    atop_available: false,
  };
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

function bytesFromWsMessage(raw: RawData): Uint8Array {
  if (raw instanceof ArrayBuffer) {
    return new Uint8Array(raw);
  }
  if (ArrayBuffer.isView(raw)) {
    return new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
  }
  if (Array.isArray(raw)) {
    const chunks = raw.map(bytesFromWsMessage);
    const totalLength = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
    const out = new Uint8Array(totalLength);
    let offset = 0;
    for (const chunk of chunks) {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return out;
  }
  return encodeUtf8(String(raw));
}

function defaultSessionGit(sessionId: UUID): SessionGitResultPayload {
  // 默认 Git 快照保持可读的最小样例，便于 UI 测试只关心 tab 渲染和消息流。
  return {
    session_id: sessionId,
    cwd: "/home/me/project",
    repository_root: "/home/me/project",
    worktrees: [
      {
        path: "/home/me/project",
        branch: "main",
        head: "a1b2c3d",
        is_current: true,
        staged: [{ path: "src/lib.rs", status: "M " }],
        unstaged: [{ path: "README.md", status: " M" }],
      },
    ],
    graph: ["* a1b2c3d main commit"],
    error: null,
  };
}

function mockSessionSearchResult(payload: SessionSearchPayload, source: string): SessionSearchResultPayload {
  const query = payload.query.trim();
  const caseSensitive = payload.case_sensitive;
  const maxResults = Math.max(1, Math.min(payload.max_results ?? 80, 500));
  const lines = source.split(/\r?\n/);
  const needle = caseSensitive ? query : query.toLowerCase();
  const matches: SessionSearchMatchPayload[] = [];

  for (let lineIndex = 0; lineIndex < lines.length; lineIndex += 1) {
    const line = lines[lineIndex] ?? "";
    const haystack = caseSensitive ? line : line.toLowerCase();
    let searchIndex = 0;
    while (query && searchIndex <= haystack.length) {
      const matchIndex = haystack.indexOf(needle, searchIndex);
      if (matchIndex < 0) {
        break;
      }
      if (matches.length >= maxResults) {
        return {
          session_id: payload.session_id,
          query,
          line_count: lines.length,
          matches,
          truncated: true,
        };
      }
      matches.push({
        line_index: lineIndex,
        column_index: matchIndex,
        line_text: line,
      });
      searchIndex = matchIndex + Math.max(1, needle.length);
    }
  }

  return {
    session_id: payload.session_id,
    query,
    line_count: lines.length,
    matches,
    truncated: false,
  };
}

function mockSessionGitDiffResult(payload: SessionGitDiffPayload): SessionGitDiffResultPayload {
  const fileLabel = payload.file_path?.trim() || payload.worktree_path;
  const staged = Boolean(payload.staged);
  const prefix = staged ? "staged" : "unstaged";
  return {
    session_id: payload.session_id,
    worktree_path: payload.worktree_path,
    file_path: payload.file_path?.trim() || undefined,
    staged,
    diff: [
      `diff --git a/${fileLabel} b/${fileLabel}`,
      `--- a/${fileLabel}`,
      `+++ b/${fileLabel}`,
      `@@ -1 +1 @@`,
      `${staged ? "+" : "-"} mock ${prefix} diff for ${fileLabel}`,
    ].join("\n") + "\n",
  };
}

function parentDirectory(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  if (index <= 0) {
    return "/";
  }
  return trimmed.slice(0, index);
}

function basenamePath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index >= 0 ? trimmed.slice(index + 1) || trimmed : trimmed;
}
