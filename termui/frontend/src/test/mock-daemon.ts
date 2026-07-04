import { createServer, type IncomingMessage, type Server as HttpServer, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import type { Socket } from "node:net";
import { Buffer } from "node:buffer";
import { ed25519 } from "@noble/curves/ed25519";
import { WebSocketServer, type RawData, type WebSocket } from "ws";
import {
  authSigningInputBytes,
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  httpE2eeSigningInputBytes,
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
} from "../protocol/binary-packet";
import {
  legacyEnvelopeTypeForProtocolMethod,
  protocolEventMethodForLegacyEnvelopeType,
  protocolMethodNeedsEmptyAck,
} from "../protocol/methods";
import { binaryPacketToProtocol, protocolPacketToBinary } from "../protocol/packet-codec";
import {
  buildAttachFramePayload,
  decodeSupervisorTerminalClientFrame,
  decodeSupervisorTerminalServerFrame,
  encodeSupervisorTerminalServerFrame,
  type SupervisorTerminalServerFrame,
} from "../protocol/supervisor-terminal";
import { fallbackSessionDisplayName } from "../session-names";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "../protocol/types";
import type {
  AttachFramePayload,
  DaemonClientSummaryPayload,
  DaemonStatusResultPayload,
  HelloPayload,
  HttpE2eeAuthPayload,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  MetadataSubscribePayload,
  PacketErrorPayload,
  PacketStreamId,
  PairRequestPayload,
  ProtocolPacket,
  RouteHelloPayload,
  SessionCreatePayload,
  SessionCreatedPayload,
  SessionCursorPayload,
  SessionFileReadResultPayload,
  SessionFileTransferChunkPayload,
  SessionScopeGrantPayload,
  SessionFileUploadProgressPayload,
  SessionFileUploadReadyPayload,
  SessionFileWrittenPayload,
  SessionGitActionPayload,
  SessionGitDiffPayload,
  SessionGitDiffResultPayload,
  SessionFilesResultPayload,
  SessionGitResultPayload,
  SessionSearchMatchPayload,
  SessionSearchPayload,
  SessionSearchResultPayload,
  SingleTerminalFramePayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "../protocol/types";
import {
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
  createOutputBeforeResponse?: string;
  attachDelayMs?: number;
  sessionCreateDelayMs?: number;
  routePreludeError?: ErrorPayload;
  routeReadyDelayMs?: number;
  routeReadyDelayOnceMs?: number;
  daemonPacketVersion?: number;
  daemonBinaryVersion?: number | null;
  pairFailure?: ErrorPayload;
  sessionDataError?: ErrorPayload;
  resizeAckDelayMs?: number;
  cursorAckDelayMs?: number;
  daemonClients?: DaemonClientSummaryPayload[];
  daemonClientsDelayMs?: number;
  dropDaemonClients?: boolean;
  daemonStatus?: DaemonStatusResultPayload;
  daemonStatusResponses?: DaemonStatusResultPayload[];
  daemonStatusDelayMs?: number;
  dropAuthChallenge?: boolean;
  sessionFilesDelayMs?: number;
  sessionFilesDelayMsByPath?: Record<string, number>;
  sessionFiles?: Record<UUID, SessionFilesResultPayload>;
  sessionGitDelayMs?: number;
  sessionGitDelayMsBySession?: Record<UUID, number>;
  sessionGit?: Record<UUID, SessionGitResultPayload>;
  sessionFileReads?: Record<string, SessionFileReadResultPayload>;
  sessionFileReadDelayMsByPath?: Record<string, number>;
  sessionFileWriteDelayMsByPath?: Record<string, number>;
  sessionGitDiffDelayMsByPath?: Record<string, number>;
  fileUploadProgressOverrides?: Record<string, Partial<SessionFileUploadProgressPayload>>;
  fileUploadProgressDelayMs?: number;
  relayClientPathOnly?: boolean;
  closeSessionUnownedError?: ErrorPayload;
  pingDelayMs?: number;
  sessionTokenExpiresAtMs?: number;
  sessionScopeExpiresAtMs?: number;
}

interface QueuedSessionListResponse {
  sessions: SessionSummaryPayload[];
  delayMs: number;
}

interface QueuedSessionFilesResponse {
  sessionId: UUID;
  path?: string | null;
  files: SessionFilesResultPayload;
  delayMs: number;
}

interface TrustedDevice {
  deviceId: UUID;
  devicePublicKey: string;
}

interface SessionScopeRecord {
  deviceId: UUID;
  sessionId: UUID;
  token: string;
  expiresAtMs: number;
}

interface SessionTokenRecord {
  deviceId: UUID;
  expiresAtMs: number;
}

interface MockTerminalStream {
  sessionId: UUID;
  streamId: PacketStreamId;
  nextTransportSeq: number;
  watchUpdates: boolean;
}

interface MockFileUploadStream {
  sessionId: UUID;
  path: string;
  sizeBytes: number;
  offsetBytes: number;
  chunks: Uint8Array[];
  nextOutputSeq: number;
}

interface MockFileDownloadStream {
  sessionId: UUID;
  path: string;
  bytes: Uint8Array;
  offsetBytes: number;
  nextOutputSeq: number;
}

interface MockConnection {
  id: number;
  socket: WebSocket;
  routed: boolean;
  httpPackets?: ProtocolPacket[];
  deviceId?: UUID;
  e2ee?: E2eeSession;
  attachedSessionIds: Set<UUID>;
  watchedSessionIds: Set<UUID>;
  pendingCanceledTerminalStreamIds: Set<PacketStreamId>;
  terminalStreamsById: Map<PacketStreamId, MockTerminalStream>;
  terminalStreamsBySession: Map<UUID, MockTerminalStream>;
  fileUploadsById: Map<PacketStreamId, MockFileUploadStream>;
  fileDownloadsById: Map<PacketStreamId, MockFileDownloadStream>;
  daemonE2eeExchange?: E2eeKeyExchangePayload;
  e2eeAuthTranscriptSha256?: string;
  activeRequest?: ProtocolPacket;
  activeStreamId?: PacketStreamId;
  respondedToActiveRequest?: boolean;
  requestChain?: Promise<void>;
  binaryMode?: boolean;
  aborted?: boolean;
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
  public readonly receivedPacketLog: Array<{ connection_id: number; packet: ProtocolPacket }> = [];
  public readonly receivedHttpRequests: Array<{ path: string; method: string; payload: unknown }> = [];
  public readonly sentPacketLog: Array<{ connection_id: number; packet: ProtocolPacket }> = [];
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
  public readonly sessionFileBinaryWrites: Array<{ session_id: UUID; path: string; bytes: Uint8Array }> = [];
  public readonly sessionFileDeletes: Array<{ session_id: UUID; path: string }> = [];
  public readonly sessionGitRequests: Array<{ session_id: UUID }> = [];
  public readonly sessionGitActions: SessionGitActionPayload[] = [];
  public readonly sessionGitDiffRequests: SessionGitDiffPayload[] = [];
  public readonly sessionSearchRequests: SessionSearchPayload[] = [];
  public daemonStatusRequests = 0;
  public pingMessages = 0;
  public acceptedConnections = 0;
  private nextConnectionId = 1;
  public failedTerminalAttachRequests = 0;
  public readonly decryptedInputs: string[] = [];
  public nextAttachRole = "operator" as const;
  private createdSessionCounter = 0;
  private failTerminalAttachRequests = 0;
  private failWatchedTerminalAttachRequests = 0;
  private closeDaemonStatusRequests = 0;
  private closeDaemonClientsRequests = 0;
  private closeSessionListRequests = 0;
  private closeSessionResizeRequests = 0;
  private closeSessionCursorRequests = 0;
  private nextRouteReadyGate: Promise<void> | undefined;
  private readonly queuedSessionListResponses: QueuedSessionListResponse[] = [];
  private readonly queuedSessionFilesResponses: QueuedSessionFilesResponse[] = [];
  private readonly e2eeKeypair: E2eeKeyPair;
  private readonly trustedDevices = new Map<UUID, TrustedDevice>();
  private readonly sessionTokens = new Map<string, SessionTokenRecord>();
  private readonly sessionScopes = new Map<string, SessionScopeRecord>();
  private readonly connections = new Set<MockConnection>();
  private readonly serverSockets = new Set<Socket>();
  private readonly sessionFilePositions = new Map<UUID, string>();
  private readonly sessionOutputSnapshots = new Map<UUID, string>();
  private readonly sessionTerminalNextSeq = new Map<UUID, number>();
  private readonly fileStore = new Map<string, Uint8Array>();

  private constructor(
    private readonly httpServer: HttpServer,
    private readonly wsServer: WebSocketServer,
    private readonly urlValue: string,
    private readonly options: MockDaemonOptions,
  ) {
    this.serverId = randomUuid();
    this.e2eeKeypair = generateE2eeKeyPair();
  }

  static async start(options: MockDaemonOptions): Promise<MockDaemon> {
    let daemon!: MockDaemon;
    const httpServer = createServer((request, response) => {
      void daemon.handleNodeHttpRequest(request, response);
    });
    httpServer.on("connection", (socket) => {
      daemon.serverSockets.add(socket);
      socket.on("close", () => daemon.serverSockets.delete(socket));
    });
    const wsServer = new WebSocketServer({ noServer: true });
    httpServer.on("upgrade", (request, socket, head) => {
      const requestUrl = new URL(request.url ?? "/", "http://127.0.0.1");
      if (!/\/ws$/u.test(requestUrl.pathname)) {
        socket.destroy();
        return;
      }
      wsServer.handleUpgrade(request, socket, head, (websocket) => {
        daemon.accept(websocket, request.url ?? "");
      });
    });
    await new Promise<void>((resolve) => httpServer.listen(0, "127.0.0.1", resolve));
    const address = httpServer.address() as AddressInfo;
    daemon = new MockDaemon(httpServer, wsServer, `ws://127.0.0.1:${address.port}/ws`, options);
    const registry = globalThis as typeof globalThis & {
      __TERMD_TEST_HTTP_DAEMONS__?: Map<string, MockDaemon>;
    };
    registry.__TERMD_TEST_HTTP_DAEMONS__ ??= new Map();
    registry.__TERMD_TEST_HTTP_DAEMONS__.set(`http://127.0.0.1:${address.port}`, daemon);
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

  closeNextDaemonStatusRequests(count = 1): void {
    // 用 transport close 模拟真实浏览器/relay 中“旁路状态请求发现连接已关闭”的路径。
    this.closeDaemonStatusRequests = Math.max(0, Math.floor(count));
  }

  closeNextDaemonClientsRequests(count = 1): void {
    // daemon.clients 是后台元数据请求；关闭它不能被前端升级成终端永久断线。
    this.closeDaemonClientsRequests = Math.max(0, Math.floor(count));
  }

  closeNextSessionListRequests(count = 1): void {
    // 中文注释：真实 relay/浏览器里 workspace 的手动/后台 session.list 也可能瞬时断开。
    // 这里用 transport close 覆盖“已有 workspace 时不能被升级成全局断线”的回归。
    this.closeSessionListRequests = Math.max(0, Math.floor(count));
  }

  closeNextSessionResizeRequests(count = 1): void {
    // 中文注释：真实 relay/浏览器里 session.resize 的 HTTP control fetch 可能瞬断失败。
    // 这里显式只打断这一笔辅助 RPC，覆盖“不能升级成全局 Connection error”的回归。
    this.closeSessionResizeRequests = Math.max(0, Math.floor(count));
  }

  closeNextSessionCursorRequests(count = 1): void {
    // 中文注释：cursor 上报同样是 terminal sidecar HTTP control；网络瞬断只影响这一笔元数据。
    this.closeSessionCursorRequests = Math.max(0, Math.floor(count));
  }

  expireSessionToken(token: string, expiresAtMs = 0): void {
    const record = this.sessionTokens.get(token);
    if (!record) {
      return;
    }
    // 中文注释：测试 daemon 侧直接判定 bearer 过期的路径，覆盖浏览器本地缓存仍自认为有效的场景。
    this.sessionTokens.set(token, {
      ...record,
      expiresAtMs,
    });
  }

  expireSessionScope(token: string, expiresAtMs = 0): void {
    const record = this.sessionScopes.get(token);
    if (!record) {
      return;
    }
    // 中文注释：模拟 daemon 直接判定现有 scope 失效，而浏览器本地缓存仍未过期的场景。
    this.sessionScopes.set(token, {
      ...record,
      expiresAtMs,
    });
  }

  queueSessionListResponse(sessions: SessionSummaryPayload[], delayMs = 0): void {
    // 用一次性响应模拟“旧请求稍后返回”的真实浏览器竞态。
    this.queuedSessionListResponses.push({ sessions, delayMs });
  }

  queueSessionFilesResponse(
    sessionId: UUID,
    files: SessionFilesResultPayload,
    options: { path?: string | null; delayMs?: number } = {},
  ): void {
    // 中文注释：按单次请求排队的 file-tree 响应用来构造“旧 follow 晚到覆盖 manual”
    // 之类的竞态；不能只靠全局固定延迟，否则 follow/manual 的响应顺序仍然不可控。
    this.queuedSessionFilesResponses.push({
      sessionId,
      path: options.path,
      files,
      delayMs: options.delayMs ?? 0,
    });
  }

  pushSessionFiles(files: SessionFilesResultPayload): void {
    this.sessionFilePositions.set(files.session_id, files.path);
    this.options.sessionFiles = {
      ...(this.options.sessionFiles ?? {}),
      [files.path]: files,
      [files.session_id]: files,
    };
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.has(files.session_id)) {
        this.sendInner(connection, envelope("session_files_result", files));
      }
    }
  }

  pushSessionCwdChanged(sessionId: UUID, cwd: string): void {
    this.sessionFilePositions.set(sessionId, cwd);
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.has(sessionId)) {
        this.sendInner(connection, envelope("session_cwd_changed", { session_id: sessionId, cwd }));
      }
    }
  }

  sendUnownedPacketError(code: string, message: string): void {
    const connection = [...this.connections].find((candidate) => candidate.routed);
    if (!connection) {
      throw new Error("no routed connection is available");
    }
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "error",
      payload: { code, message, retryable: false },
    } satisfies ProtocolPacket<PacketErrorPayload>);
  }

  sendSessionClosed(sessionId: UUID): void {
    for (const connection of this.connections) {
      if (connection.routed) {
        this.sendInner(connection, envelope("session_closed", { session_id: sessionId }));
      }
    }
    this.cleanupClosedSession(sessionId);
  }

  setSessionFilePosition(sessionId: UUID, path: string): void {
    // 测试轮询时只改变 daemon 端共享目录，不主动 push，才能确认前端真的发起了下一次 session_files。
    this.sessionFilePositions.set(sessionId, path);
  }

  pushSessionData(sessionId: UUID, text: string): void {
    this.appendSessionOutput(sessionId, text);
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.has(sessionId)) {
        const terminalSeq = this.claimNextMockTerminalSeq(sessionId);
        this.sendSupervisorTerminalFrame(
          connection,
          sessionId,
          {
            type: "terminal_frame",
            session_id: sessionId,
            frame: {
              kind: "output",
              session_id: sessionId,
              terminal_seq: terminalSeq,
              data_bytes: new TextEncoder().encode(text),
            },
          },
        );
      }
    }
  }

  pushTerminalFrameBatch(sessionId: UUID, frames: unknown[]): void {
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.has(sessionId)) {
        this.sendTerminalStreamBatch(connection, sessionId, frames);
      }
    }
  }

  pushTerminalFrame(sessionId: UUID, frame: unknown): void {
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.has(sessionId)) {
        this.sendTerminalStreamFrame(connection, sessionId, frame);
      }
    }
  }

  pushSessionDataToAll(sessionId: UUID, text: string): void {
    // 后台 session 只发 activity 标记，不把未打开 session 的输出内容灌进当前 xterm。
    for (const connection of this.connections) {
      if (connection.routed && connection.watchedSessionIds.size > 0) {
        void text;
        this.sendInner(connection, envelope("session_activity", { session_id: sessionId, timestamp_ms: nowMs() }));
      }
    }
  }

  async stop(): Promise<void> {
    const registry = globalThis as typeof globalThis & {
      __TERMD_TEST_HTTP_DAEMONS__?: Map<string, MockDaemon>;
    };
    registry.__TERMD_TEST_HTTP_DAEMONS__?.delete(this.httpBaseUrl());
    this.wsServer.clients.forEach((client) => client.terminate());
    // 中文注释：metadata sidecar 接入后，同一测试里更容易残留 HTTP keep-alive 或 upgrade socket。
    // 只调用 httpServer.close() 可能一直等到 Vitest hook timeout；这里显式强拆全部底层连接。
    this.httpServer.closeIdleConnections?.();
    this.httpServer.closeAllConnections?.();
    for (const socket of this.serverSockets) {
      socket.destroy();
    }
    await new Promise<void>((resolve, reject) => {
      this.httpServer.close((error) => (error ? reject(error) : resolve()));
    });
  }

  httpBaseUrl(): string {
    return this.urlValue.replace(/^ws:/u, "http:").replace(/\/ws$/u, "");
  }

  dropConnections(): void {
    // 移动端 PWA 切后台时系统可能只杀掉 WebSocket，而 daemon 本身仍然在线。
    this.wsServer.clients.forEach((client) => client.close());
  }

  private async handleNodeHttpRequest(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", this.httpBaseUrl());
    const setCorsHeaders = () => {
      response.setHeader("access-control-allow-origin", "*");
      response.setHeader("access-control-allow-methods", "POST, OPTIONS");
      response.setHeader(
        "access-control-allow-headers",
        "authorization, content-type, x-termd-server-id, x-termd-device-id, x-termd-relay-admission, x-termd-e2ee-public-key, x-termd-e2ee-nonce, x-termd-e2ee-timestamp-ms, x-termd-e2ee-signature, x-termd-session-scope",
      );
      response.setHeader("access-control-max-age", "600");
    };
    setCorsHeaders();
    if (request.method === "OPTIONS") {
      // 中文注释：浏览器 direct 模式访问不同端口的 daemon 时，HTTP control 会先走
      // CORS preflight；测试桩必须显式放行，才能覆盖真实跨源直连路径。
      response.statusCode = 204;
      response.end();
      return;
    }
    if (!/^\/api\/control\//u.test(url.pathname)) {
      response.statusCode = 404;
      response.end();
      return;
    }
    const abortController = new AbortController();
    request.on("aborted", () => abortController.abort());
    response.on("close", () => {
      if (!response.writableEnded) {
        abortController.abort();
      }
    });
    const chunks: Buffer[] = [];
    for await (const chunk of request) {
      chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
    }
    try {
      const result = await this.handleHttpControlRequest(url.toString(), {
        method: request.method,
        headers: request.headers as HeadersInit,
        body: chunks.length > 0 ? Buffer.concat(chunks) : undefined,
        signal: abortController.signal,
      });
      response.statusCode = result.status;
      result.headers.forEach((value, key) => response.setHeader(key, value));
      setCorsHeaders();
      response.end(Buffer.from(await result.arrayBuffer()));
    } catch (error) {
      if (abortController.signal.aborted) {
        response.destroy();
        return;
      }
      response.statusCode = 500;
      response.end(String(error instanceof Error ? error.message : error));
    }
  }

  setDropAuthChallenge(drop: boolean): void {
    // 用来模拟 relay/daemon 卡在认证挑战前的半开连接，覆盖前端是否会主动收口 socket。
    this.options.dropAuthChallenge = drop;
  }

  delayNextRouteReady(delayMs: number): void {
    // 中文注释：恢复链路测试需要先完成初次 pairing/attach，再只把下一条新连接变慢。
    // 直接在启动参数里设置一次性延迟会误伤初次 pairing，无法覆盖真实的后台恢复路径。
    this.options.routeReadyDelayOnceMs = delayMs;
  }

  holdNextRouteReady(): () => void {
    let releaseGate: () => void = () => {};
    // 中文注释：快速切换测试需要精确卡住某一次 route_ready，而不是依赖计时器；
    // 这样可以稳定复现“旧 workspace connect 未完成就切到新 session”的半开连接。
    this.nextRouteReadyGate = new Promise<void>((resolve) => {
      releaseGate = resolve;
    });
    return () => {
      releaseGate();
    };
  }

  private accept(socket: WebSocket, requestPath: string): void {
    const pathname = requestPath.split("?")[0] || requestPath;
    if (this.options.relayClientPathOnly && pathname !== "/ws") {
      // 旧版 path-based client URL 已移除；mock 用这个开关确保前端只连接统一 /ws 入口。
      socket.close();
      return;
    }

    const connection: MockConnection = {
      id: this.nextConnectionId++,
      socket,
      routed: false,
      attachedSessionIds: new Set(),
      watchedSessionIds: new Set(),
      pendingCanceledTerminalStreamIds: new Set(),
      terminalStreamsById: new Map(),
      terminalStreamsBySession: new Map(),
      fileUploadsById: new Map(),
      fileDownloadsById: new Map(),
      requestChain: Promise.resolve(),
    };
    this.connections.add(connection);
    this.acceptedConnections += 1;
    socket.on("close", () => this.connections.delete(connection));

    socket.on("message", (raw, isBinary) => {
      const run = async () => {
        if (isBinary) {
          await this.handleOuterBinary(connection, bytesFromWsMessage(raw));
          return;
        }
        await this.handleOuter(connection, raw.toString());
      };
      connection.requestChain = (connection.requestChain ?? Promise.resolve())
        .catch(() => undefined)
        .then(run);
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
      binary_version: this.options.daemonBinaryVersion ?? BINARY_PROTOCOL_VERSION,
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

    if (outer.type === "hello") {
      const payload = outer.payload as HelloPayload;
      if (payload.protocol_version !== PROTOCOL_PACKET_VERSION || payload.server_id !== this.serverId || !payload.device_id) {
        this.sendError(connection, "unsupported_protocol_version", "unsupported protocol version");
        return;
      }
      connection.deviceId = payload.device_id;
      connection.binaryMode = payload.binary_version === BINARY_PROTOCOL_VERSION;

      if (this.trustedDevices.has(payload.device_id) && !this.options.dropAuthChallenge) {
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

    if (outer.type === "packet") {
      await this.handlePacket(connection, outer.payload as ProtocolPacket);
      return;
    }

    this.sendError(connection, "invalid_state", "invalid protocol state");
  }

  private async handleOuterBinary(connection: MockConnection, raw: Uint8Array): Promise<void> {
    this.binaryWireFrames.push({ direction: "in", byteLength: raw.byteLength });
    if (!connection.binaryMode) {
      this.sendError(connection, "invalid_state", "invalid protocol state");
      return;
    }
    const binaryPacket = decodeBinaryProtocolPacket(raw);
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
	          protocol_version: this.options.daemonPacketVersion ?? PROTOCOL_PACKET_VERSION,
	          nonce: nonce(),
	          timestamp_ms: nowMs(),
	          server_id: this.serverId,
	          daemon_public_key: this.daemonPublicKey,
	          binary_version: this.options.daemonBinaryVersion ?? BINARY_PROTOCOL_VERSION,
	          device_id: null,
	        }),
      );
    };
    const routeReadyDelayMs = this.options.routeReadyDelayOnceMs ?? this.options.routeReadyDelayMs;
    if (this.options.routeReadyDelayOnceMs !== undefined) {
      // 中文注释：一次性慢 route prelude 用来复现浏览器从后台恢复时，
      // 第一条 relay/client 连接被短超时误杀，后续 attach 仍应按长超时恢复。
      this.options.routeReadyDelayOnceMs = undefined;
    }
    const routeReadyGate = this.nextRouteReadyGate;
    if (routeReadyGate) {
      this.nextRouteReadyGate = undefined;
      void routeReadyGate.then(sendPrelude);
      return;
    }
    if (routeReadyDelayMs) {
      setTimeout(sendPrelude, routeReadyDelayMs);
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
    this.receivedPacketLog.push({ connection_id: connection.id, packet });
    if (packet.version !== PROTOCOL_PACKET_VERSION) {
      this.sendPacketError(connection, packet, "unsupported_protocol_version", "unsupported protocol packet version");
      return;
    }

    if (packet.kind === "flow") {
      if (packet.stream_id && connection.fileDownloadsById.has(packet.stream_id)) {
        this.handleFileDownloadFlow(connection, packet);
      }
      return;
    }
    if (packet.kind === "cancel") {
      if (packet.stream_id && !this.removeTerminalStream(connection, packet.stream_id)) {
        // 中文注释：terminal.attach 可能仍在延迟响应阶段，此时 stream 尚未注册。
        // 先记下取消意图，等迟到 ack 返回时不要再把旧 stream/watcher 挂回连接。
        connection.pendingCanceledTerminalStreamIds.add(packet.stream_id);
      }
      if (packet.stream_id) {
        connection.fileUploadsById.delete(packet.stream_id);
        connection.fileDownloadsById.delete(packet.stream_id);
      }
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
    if (packet.kind === "stream_open" && await this.handleFileStreamOpen(connection, packet)) {
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
    if (packet.stream_id && connection.fileUploadsById.has(packet.stream_id)) {
      await this.handleFileUploadChunk(connection, packet);
      return;
    }
    if (!packet.stream_id || !connection.terminalStreamsById.has(packet.stream_id)) {
      this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
      return;
    }
    connection.activeStreamId = packet.stream_id;
    try {
      const attachFramePayload = packet.payload as {
        data_base64?: string;
        data_bytes?: Uint8Array;
      };
      const maybeAttachBytes = attachFramePayload.data_bytes ?? (
        typeof attachFramePayload.data_base64 === "string"
          ? sessionDataFromBase64(attachFramePayload.data_base64)
          : undefined
      );
      if (!maybeAttachBytes) {
        this.sendPacketError(connection, packet, "invalid_packet", "terminal stream chunk payload is invalid");
        return;
      }
      try {
        decodeSupervisorTerminalClientFrame(maybeAttachBytes);
      } catch {
        this.sendPacketError(connection, packet, "invalid_packet", "terminal stream chunk payload is invalid");
        return;
      }
      await this.handleLegacyInner(connection, envelope("attach_frame", packet.payload));
    } finally {
      connection.activeStreamId = undefined;
    }
  }

  private async handleFileStreamOpen(connection: MockConnection, packet: ProtocolPacket): Promise<boolean> {
    if (!packet.stream_id) {
      return false;
    }
    if (packet.method === "session.file_upload") {
      const payload = packet.payload as { session_id: UUID; path: string; size_bytes: number };
      if (!connection.attachedSessionIds.has(payload.session_id)) {
        this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
        return true;
      }
      connection.fileUploadsById.set(packet.stream_id, {
        sessionId: payload.session_id,
        path: payload.path,
        sizeBytes: payload.size_bytes,
        offsetBytes: 0,
        chunks: [],
        nextOutputSeq: 1,
      });
      this.sendPacketResponse(connection, packet, {
        session_id: payload.session_id,
        path: payload.path,
        size_bytes: payload.size_bytes,
        offset_bytes: 0,
      } satisfies SessionFileUploadReadyPayload);
      return true;
    }
    if (packet.method === "session.file_download") {
      const payload = packet.payload as { session_id: UUID; path: string };
      if (!connection.attachedSessionIds.has(payload.session_id)) {
        this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
        return true;
      }
      const bytes = this.fileStore.get(payload.path) ?? sessionDataFromBase64(this.options.sessionFileReads?.[payload.path]?.data_base64 ?? "");
      connection.fileDownloadsById.set(packet.stream_id, {
        sessionId: payload.session_id,
        path: payload.path,
        bytes,
        offsetBytes: 0,
        nextOutputSeq: 1,
      });
      this.sendPacketResponse(connection, packet, {
        session_id: payload.session_id,
        path: payload.path,
        name: basenamePath(payload.path),
        size_bytes: bytes.byteLength,
        modified_at_ms: this.options.sessionFileReads?.[payload.path]?.modified_at_ms ?? null,
      });
      return true;
    }
    return false;
  }

  private async handleFileUploadChunk(connection: MockConnection, packet: ProtocolPacket): Promise<void> {
    const streamId = packet.stream_id!;
    const stream = connection.fileUploadsById.get(streamId);
    if (!stream) {
      this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
      return;
    }
    const payload = packet.payload as SessionFileTransferChunkPayload;
    const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
    if (payload.session_id !== stream.sessionId || payload.offset_bytes !== stream.offsetBytes || payload.size_bytes !== stream.sizeBytes) {
      this.sendPacketError(connection, packet, "invalid_packet", "invalid file upload chunk");
      return;
    }
    stream.chunks.push(bytes);
    stream.offsetBytes += bytes.byteLength;
    const complete = Boolean(payload.eof || stream.offsetBytes >= stream.sizeBytes);
    if (complete) {
      const uploaded = concatByteChunks(stream.chunks);
      this.fileStore.set(stream.path, uploaded);
      this.sessionFileBinaryWrites.push({ session_id: stream.sessionId, path: stream.path, bytes: uploaded });
      connection.fileUploadsById.delete(streamId);
    }
    const progress: SessionFileUploadProgressPayload = {
      session_id: stream.sessionId,
      path: stream.path,
      offset_bytes: stream.offsetBytes,
      size_bytes: stream.sizeBytes,
      eof: complete,
      modified_at_ms: complete ? nowMs() : null,
      ...(this.options.fileUploadProgressOverrides?.[stream.path] ?? {}),
    };
    if (this.options.fileUploadProgressDelayMs) {
      // 中文注释：测试用延迟让客户端稳定进入 file stream waiter，便于验证 close/error 会唤醒它。
      await new Promise((resolve) => setTimeout(resolve, this.options.fileUploadProgressDelayMs));
    }
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: streamId,
      seq: stream.nextOutputSeq,
      payload: progress,
    });
    stream.nextOutputSeq += 1;
    if (complete) {
      this.sendPacket(connection, {
        version: PROTOCOL_PACKET_VERSION,
        kind: "stream_end",
        stream_id: streamId,
        seq: stream.nextOutputSeq,
        payload: {},
      });
    }
  }

  private handleFileDownloadFlow(connection: MockConnection, packet: ProtocolPacket): void {
    const streamId = packet.stream_id!;
    const stream = connection.fileDownloadsById.get(streamId);
    if (!stream) {
      this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
      return;
    }
    const maxBytes = Math.max(1, Math.min(packet.credit ?? 256 * 1024, 256 * 1024));
    const start = stream.offsetBytes;
    const end = Math.min(stream.bytes.byteLength, start + maxBytes);
    const bytes = stream.bytes.slice(start, end);
    stream.offsetBytes = end;
    const eof = end >= stream.bytes.byteLength;
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: streamId,
      seq: stream.nextOutputSeq,
      payload: {
        session_id: stream.sessionId,
        offset_bytes: start,
        data_bytes: bytes,
        size_bytes: stream.bytes.byteLength,
        eof,
      } satisfies SessionFileTransferChunkPayload,
    });
    stream.nextOutputSeq += 1;
    if (eof) {
      connection.fileDownloadsById.delete(streamId);
      this.sendPacket(connection, {
        version: PROTOCOL_PACKET_VERSION,
        kind: "stream_end",
        stream_id: streamId,
        seq: stream.nextOutputSeq,
        payload: {},
      });
    }
  }

  private async handleDirectPacketRequest(connection: MockConnection, packet: ProtocolPacket): Promise<boolean> {
    switch (packet.method) {
      case "session.list": {
        if (this.closeSessionListRequests > 0) {
          this.closeSessionListRequests -= 1;
          this.throwIfHttpAborted(connection);
          throw new TypeError("Failed to fetch");
        }
        const queued = this.queuedSessionListResponses.shift();
        if (queued?.delayMs) {
          await new Promise((resolve) => setTimeout(resolve, queued.delayMs));
        }
        this.throwIfHttpAborted(connection);
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
        this.cleanupClosedSession(payload.session_id);
        if (this.options.closeSessionUnownedError) {
          // 中文注释：真实 daemon 在显式 close 当前 attach session 时，watch stream 的收尾错误
          // 可能先于 close RPC ack 到达浏览器。测试桩用未归属 request id 的 error 模拟该竞态。
          this.sendPacket(connection, {
            version: PROTOCOL_PACKET_VERSION,
            kind: "error",
            payload: {
              ...this.options.closeSessionUnownedError,
              retryable: false,
            },
          });
        }
        this.sendPacketResponse(connection, packet, payload);
        return true;
      }
      case "session.files": {
        const payload = packet.payload as { session_id: UUID; path?: string | null };
        if (!connection.attachedSessionIds.has(payload.session_id)) {
          this.sendPacketError(connection, packet, "auth_failed", "auth failed");
          return true;
        }
        // 中文注释：真实 daemon 一收到 HTTP files 请求就已经进入服务端处理队列；
        // 即使浏览器随后超时，测试也应当能观察到“请求已发出”。
        this.sessionFileRequests.push(payload);
        const queued = this.dequeueSessionFilesResponse(payload);
        const delayMs = queued?.delayMs ?? this.options.sessionFilesDelayMsByPath?.[payload.path ?? ""] ?? this.options.sessionFilesDelayMs;
        if (delayMs) {
          // 中文注释：packet response 按 id 返回；慢 files 请求不能抢占同连接上的其他请求。
          await new Promise((resolve) => setTimeout(resolve, delayMs));
        }
        this.throwIfHttpAborted(connection);
        const files = queued?.files ?? this.resolveSessionFilesResult(payload);
        this.sendPacketResponse(connection, packet, files);
        return true;
      }
      case "session.git": {
        const payload = packet.payload as { session_id: UUID };
        if (!connection.attachedSessionIds.has(payload.session_id)) {
          // 中文注释：HTTP session scope 缺失/过期时，git 面板也必须走统一的 auth_failed 语义，
          // 否则会掩盖前端对 scope 失效自愈路径的真实处理。
          this.sendPacketError(connection, packet, "auth_failed", "auth failed");
          return true;
        }
        this.sessionGitRequests.push(payload);
        const gitDelayMs = this.options.sessionGitDelayMsBySession?.[payload.session_id] ?? this.options.sessionGitDelayMs;
        if (gitDelayMs) {
          // 中文注释：Git packet response 必须保留原 request id，模拟真实 daemon 并发响应。
          await new Promise((resolve) => setTimeout(resolve, gitDelayMs));
        }
        this.sendPacketResponse(
          connection,
          packet,
          this.options.sessionGit?.[payload.session_id] ?? defaultSessionGit(payload.session_id),
        );
        return true;
      }
      case "auth.session_token": {
        if (!connection.deviceId) {
          this.sendPacketError(connection, packet, "invalid_state", "invalid protocol state");
          return true;
        }
        const token = `mock-session-token-${connection.deviceId}`;
        const expiresAtMs = this.options.sessionTokenExpiresAtMs ?? nowMs() + 60_000;
        this.sessionTokens.set(token, {
          deviceId: connection.deviceId,
          expiresAtMs,
        });
        this.sendPacketResponse(connection, packet, {
          server_id: this.serverId,
          device_id: connection.deviceId,
          token,
          expires_at_ms: expiresAtMs,
        });
        return true;
      }
      default:
        return false;
    }
  }

  async handleHttpControlRequest(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
    const url = new URL(input instanceof Request ? input.url : String(input));
    const requestInit = input instanceof Request && !init
      ? {
          method: input.method,
          headers: input.headers,
          body: input.body,
          signal: input.signal,
        }
      : init;
    const headers = new Headers(requestInit?.headers);
    if (requestInit?.signal?.aborted) {
      throw new DOMException("The operation was aborted", "AbortError");
    }
    const authorization = headers.get("authorization");
    const bearer = authorization?.startsWith("Bearer ") ? authorization.slice("Bearer ".length) : undefined;
    if (!bearer) {
      return this.jsonError(401, "auth_failed", "auth failed");
    }
    const tokenRecord = this.sessionTokens.get(bearer);
    if (!tokenRecord || tokenRecord.expiresAtMs < nowMs()) {
      return this.jsonError(401, "auth_failed", "auth failed");
    }
    const deviceId = tokenRecord.deviceId;
    let verifiedDeviceId: UUID;
    try {
      verifiedDeviceId = await this.verifyHttpAuthHeaders(headers, url.pathname, requestInit?.method ?? "POST");
    } catch {
      return this.jsonError(400, "invalid_envelope", "message envelope is invalid");
    }
    if (verifiedDeviceId !== deviceId) {
      return this.jsonError(401, "auth_failed", "auth failed");
    }
    const body = await this.requestBodyBytes(requestInit?.body);
    const frames = this.decodeHttpPlainFrames(body);
    const payload = frames[0] ? JSON.parse(decodeUtf8(frames[0])) : {};
    // 中文注释：HTTP 控制面要把“请求已送达 daemon”与“最终是否成功返回”分开记录，
    // 这样 timeout/断链测试才能断言真实到达过的请求。
    this.receivedHttpRequests.push({ path: url.pathname, method: requestInit?.method ?? "POST", payload });

    const sessionScopeToken = headers.get("x-termd-session-scope");
    const connection = this.httpConnection(deviceId, sessionScopeToken ?? undefined);
    if (requestInit?.signal) {
      requestInit.signal.addEventListener("abort", () => {
        connection.aborted = true;
      }, { once: true });
    }
    const response = await this.dispatchHttpControl(connection, url.pathname, payload);
    const encoded = this.encodeHttpPlainFrames(response.frames.map((frame) => encodeUtf8(JSON.stringify(frame))));
    const bodyBytes = encoded.slice();
    return new Response(bodyBytes, {
      status: response.status,
      headers: { "content-type": "application/octet-stream" },
    });
  }

  private packetToLegacyEnvelope(packet: ProtocolPacket): Envelope | undefined {
    const type = legacyEnvelopeTypeForProtocolMethod(packet.method);
    if (!type) {
      return undefined;
    }
    return envelope(type, packet.payload);
  }

  private packetMethodNeedsEmptyAck(method?: string): boolean {
    return protocolMethodNeedsEmptyAck(method);
  }

  private httpConnection(deviceId: UUID, sessionScopeToken?: string): MockConnection {
    const connection: MockConnection = {
      id: 0,
      socket: {} as WebSocket,
      routed: true,
      httpPackets: [],
      deviceId,
      attachedSessionIds: new Set(),
      watchedSessionIds: new Set(),
      pendingCanceledTerminalStreamIds: new Set(),
      terminalStreamsById: new Map(),
      terminalStreamsBySession: new Map(),
      fileUploadsById: new Map(),
      fileDownloadsById: new Map(),
    };
    if (sessionScopeToken) {
      const scope = this.sessionScopes.get(sessionScopeToken);
      if (
        scope &&
        scope.deviceId === deviceId &&
        scope.expiresAtMs >= nowMs() &&
        this.options.sessions.some((session) => session.session_id === scope.sessionId)
      ) {
        connection.attachedSessionIds.add(scope.sessionId);
      }
    }
    return connection;
  }

  private async dispatchHttpControl(
    connection: MockConnection,
    path: string,
    payload: unknown,
  ): Promise<{ status: number; frames: unknown[] }> {
    const method = this.httpControlMethod(path);
    switch (method.kind) {
      case "global":
        return this.dispatchHttpUnary(connection, method.method, payload);
      case "session": {
        if (!method.sessionId) {
          return { status: 400, frames: [{ code: "invalid_envelope", message: "message envelope is invalid" }] };
        }
        if (!connection.attachedSessionIds.has(method.sessionId)) {
          // 中文注释：真实 daemon 在 session scope token 缺失、过期或与 session 不匹配时
          // 直接返回 401 auth_failed，而不是把它伪装成业务层 invalid_state。
          return { status: 401, frames: [{ code: "auth_failed", message: "auth failed" }] };
        }
        return this.dispatchHttpUnary(connection, method.method, payload);
      }
    }
  }

  private async dispatchHttpUnary(
    connection: MockConnection,
    method: string,
    payload: unknown,
  ): Promise<{ status: number; frames: unknown[] }> {
    if (connection.aborted) {
      throw new DOMException("The operation was aborted", "AbortError");
    }
    const sentPacketStart = connection.httpPackets?.length ?? 0;
    const packet: ProtocolPacket = {
      version: PROTOCOL_PACKET_VERSION,
      kind: "request",
      id: randomUuid(),
      method,
      payload,
    };
    const previousRequest = connection.activeRequest;
    const previousResponded = connection.respondedToActiveRequest;
    connection.activeRequest = packet;
    connection.respondedToActiveRequest = false;
    try {
      if (await this.handleDirectPacketRequest(connection, packet)) {
        if (connection.aborted) {
          throw new DOMException("The operation was aborted", "AbortError");
        }
        return this.packetFramesForHttpRequest(connection, sentPacketStart);
      }
      const legacy = this.packetToLegacyEnvelope(packet);
      if (!legacy) {
        // 中文注释：HTTP control 的未知方法也要经过 packet error 路径，
        // 这样浏览器端看到的语义和 WebSocket request 一致。
        this.sendPacketError(connection, packet, "unknown_method", "unknown protocol method");
        return this.packetFramesForHttpRequest(connection, sentPacketStart);
      }
      await this.handleLegacyInner(connection, legacy);
      if (connection.aborted) {
        throw new DOMException("The operation was aborted", "AbortError");
      }
      if (!connection.respondedToActiveRequest && this.packetMethodNeedsEmptyAck(method)) {
        this.sendPacketResponse(connection, packet, {});
      }
      return this.packetFramesForHttpRequest(connection, sentPacketStart);
    } finally {
      connection.activeRequest = previousRequest;
      connection.respondedToActiveRequest = previousResponded;
    }
  }

  private httpControlMethod(pathname: string):
    | { kind: "global"; method: string }
    | { kind: "session"; method: string; sessionId?: UUID } {
    const controlPrefix = pathname.match(/\/api\/control\/(.+)$/u);
    const trimmed = controlPrefix?.[1] ?? pathname.replace(/^\/api\/control\//u, "");
    const segments = trimmed.split("/").filter(Boolean);
    if (segments[0] === "session" && segments.length >= 3) {
      return {
        kind: "session",
        sessionId: segments[1],
        method: `session.${segments.slice(2).join(".")}`,
      };
    }
    return { kind: "global", method: segments.join(".") };
  }

  private async verifyHttpAuthHeaders(headers: Headers, path: string, method: string): Promise<UUID> {
    const deviceId = headers.get("x-termd-device-id");
    const devicePublicKey = headers.get("x-termd-e2ee-public-key");
    const nonceValue = headers.get("x-termd-e2ee-nonce");
    const timestampValue = headers.get("x-termd-e2ee-timestamp-ms");
    const signature = headers.get("x-termd-e2ee-signature");
    if (!deviceId || !devicePublicKey || !nonceValue || !timestampValue || !signature) {
      throw new Error("missing HTTP E2EE headers");
    }
    const trusted = this.trustedDevices.get(deviceId);
    if (!trusted) {
      throw new Error("device not trusted");
    }
    const auth: HttpE2eeAuthPayload = {
      device_id: deviceId,
      e2ee_public_key: devicePublicKey,
      nonce: nonceValue,
      timestamp_ms: Number(timestampValue),
      method,
      path,
      signature,
    };
    const ok = await verifyEd25519Signature(
      decodeEd25519PublicKey(trusted.devicePublicKey),
      httpE2eeSigningInputBytes(auth, {
        server_id: this.serverId,
        daemon_public_key: this.daemonPublicKey,
      }),
      signature,
    );
    if (!ok) {
      throw new Error("invalid HTTP E2EE signature");
    }
    return deviceId;
  }

  private decodeHttpPlainFrames(wire: Uint8Array): Uint8Array[] {
    const frames: Uint8Array[] = [];
    let offset = 0;
    while (offset < wire.byteLength) {
      const len = new DataView(wire.buffer, wire.byteOffset + offset, 4).getUint32(0, false);
      offset += 4;
      frames.push(wire.slice(offset, offset + len));
      offset += len;
    }
    return frames;
  }

  private encodeHttpPlainFrames(frames: Uint8Array[]): Uint8Array {
    const parts = frames.map((frame) => {
      const wire = new Uint8Array(4 + frame.byteLength);
      new DataView(wire.buffer, wire.byteOffset, 4).setUint32(0, frame.byteLength, false);
      wire.set(frame, 4);
      return wire;
    });
    const total = parts.reduce((sum, part) => sum + part.byteLength, 0);
    const out = new Uint8Array(total);
    let offset = 0;
    for (const part of parts) {
      out.set(part, offset);
      offset += part.byteLength;
    }
    return out;
  }

  private async requestBodyBytes(body: BodyInit | null | undefined): Promise<Uint8Array> {
    if (!body) {
      return new Uint8Array();
    }
    if (body instanceof ArrayBuffer) {
      return new Uint8Array(body);
    }
    if (ArrayBuffer.isView(body)) {
      return new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength));
    }
    if (body instanceof Blob) {
      return new Uint8Array(await body.arrayBuffer());
    }
    if (typeof body === "string") {
      return encodeUtf8(body);
    }
    if (typeof Buffer !== "undefined" && body instanceof Buffer) {
      return new Uint8Array(body);
    }
    throw new Error("unsupported HTTP body");
  }

  private jsonError(status: number, code: string, message: string): Response {
    return new Response(JSON.stringify({ code, message }), {
      status,
      headers: { "content-type": "application/json" },
    });
  }

  private packetFramesForHttpRequest(connection: MockConnection, startIndex: number): { status: number; frames: unknown[] } {
    const packets = (connection.httpPackets ?? []).slice(startIndex);
    if (packets.length === 0) {
      return { status: 500, frames: [{ code: "invalid_state", message: "invalid protocol state" }] };
    }
    return {
      // 中文注释：真实 HTTP control 会把业务层 packet error 也包在 200 + E2EE packet frame 里，
      // 只有认证/包格式等前置失败才走非 200 HTTP status。
      status: 200,
      frames: packets,
    };
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
      case "metadata_subscribe":
        this.handleMetadataSubscribe(connection, inner.payload as MetadataSubscribePayload);
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
        if (this.closeDaemonClientsRequests > 0) {
          this.closeDaemonClientsRequests -= 1;
          connection.socket.close();
          return;
        }
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
        if (this.closeDaemonStatusRequests > 0) {
          this.closeDaemonStatusRequests -= 1;
          if (connection.httpPackets) {
            // 中文注释：HTTP 控制面下，status 失败只应打断这一笔 fetch；
            // 终端 stream 仍由独立 WebSocket 维持，不能跟着一起被 mock 关掉。
            throw new TypeError("mock daemon status closed");
          }
          connection.socket.close();
          return;
        }
        if (this.options.daemonStatusDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.daemonStatusDelayMs));
        }
        this.throwIfHttpAborted(connection);
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
        this.throwIfHttpAborted(connection);
        const payload = inner.payload as { session_id: UUID; last_terminal_seq?: number | null };
        const watchUpdates = connection.activeRequest?.method === "terminal.attach";
        if (!this.options.sessions.some((candidate) => candidate.session_id === payload.session_id)) {
          this.sendError(connection, "session_not_found", "session was not found");
          return;
        }
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
        this.attachRequests.push({
          session_id: payload.session_id,
          watch_updates: watchUpdates,
          ...(payload.last_terminal_seq !== undefined ? { last_terminal_seq: payload.last_terminal_seq } : {}),
        });
        if (this.options.attachDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.attachDelayMs));
        }
        this.throwIfHttpAborted(connection);
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        if (!session) {
          // 中文注释：attach ack 到达前 session 可能已被另一条连接关闭。
          // 这种情况下不能再返回成功，否则会制造“attach 成功但权限已被 cleanup 清空”的假阳性。
          this.sendError(connection, "session_not_found", "session was not found");
          return;
        }
        const requestStreamId = connection.activeRequest?.stream_id;
        const canceledBeforeAck = !!requestStreamId && connection.pendingCanceledTerminalStreamIds.has(requestStreamId);
        if (!canceledBeforeAck) {
          if (watchUpdates && !connection.watchedSessionIds.has(payload.session_id)) {
            this.attachedSessions.push(payload.session_id);
          }
          connection.attachedSessionIds.add(payload.session_id);
          if (watchUpdates) {
            connection.watchedSessionIds.add(payload.session_id);
          }
        }
        if (watchUpdates && this.options.attachOutput) {
          if (!this.sessionOutputSnapshots.has(payload.session_id)) {
            // 中文注释：attach_sync 是 terminal attach 的首个权威 bootstrap。
            // 必须先把 mock snapshot 写好，再发送 response/attach_sync，否则测试会收到空首屏。
            this.appendSessionOutput(payload.session_id, this.options.attachOutput);
          }
        }
        this.sendInner(
          connection,
          envelope("session_attached", {
            session_id: payload.session_id,
            role: this.nextAttachRole,
            state: session?.state ?? "running",
            size: session?.size ?? { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            resize_owner: watchUpdates,
          }),
        );
        const scope = this.issueSessionScope(connection, payload.session_id);
        if (scope) {
          this.sendInner(connection, envelope("session_scope_grant", scope));
        }
        return;
      }
      case "attach_frame": {
        const payload = inner.payload as AttachFramePayload & { data_bytes?: Uint8Array };
        const frame = decodeSupervisorTerminalClientFrame(
          payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? ""),
        );
        if (frame.type === "input") {
          const input = decodeUtf8(frame.data_bytes);
          this.sessionDataMessages.push(input);
          if (this.options.sessionDataError) {
            this.sendError(connection, this.options.sessionDataError.code, this.options.sessionDataError.message);
            return;
          }
          this.decryptedInputs.push(input);
        }
        // 中文注释：mock daemon 目前只需要验证 attach frame 输入链路。resize 和 heartbeat
        // 在这里接受即可，不额外模拟 supervisor 行为，避免把测试夹带成第二套实现。
        return;
      }
      case "session_cursor": {
        this.sessionCursorUpdates.push(inner.payload as SessionCursorPayload);
        if (this.closeSessionCursorRequests > 0) {
          this.closeSessionCursorRequests -= 1;
          if (connection.httpPackets) {
            throw new TypeError("Failed to fetch");
          }
          connection.socket.close();
          return;
        }
        if (this.options.cursorAckDelayMs) {
          // 中文注释：cursor 走普通 request ack；测试用延迟模拟它被持续 stdout 挤到超时。
          await new Promise((resolve) => setTimeout(resolve, this.options.cursorAckDelayMs));
        }
        return;
      }
      case "session_resize": {
        const payload = inner.payload as { session_id: UUID; size: TerminalSize };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionResizes.push(payload);
        if (this.closeSessionResizeRequests > 0) {
          this.closeSessionResizeRequests -= 1;
          if (connection.httpPackets) {
            throw new TypeError("Failed to fetch");
          }
          connection.socket.close();
          return;
        }
        if (this.options.resizeAckDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.resizeAckDelayMs));
        }
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        if (session) {
          session.size = payload.size;
        }
        // 中文注释：真实 termd 会先给当前请求连接返回 `session_resized` ack，
        // 再通过 watcher 通知其他已 attach 连接。HTTP control 依赖这条 ack 收口 request。
        this.sendInner(connection, envelope("session_resized", {
          session_id: payload.session_id,
          size: payload.size,
          resize_owner: true,
        }));
        this.broadcastSessionResized(payload.session_id, payload.size, connection);
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
        this.cleanupClosedSession(payload.session_id);
        if (this.options.closeSessionUnownedError) {
          this.sendInner(connection, envelope("error", this.options.closeSessionUnownedError));
        }
        this.sendInner(connection, envelope("session_closed", payload));
        return;
      }
      case "session_files": {
        const payload = inner.payload as { session_id: UUID; path?: string | null };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileRequests.push(payload);
        const queued = this.dequeueSessionFilesResponse(payload);
        const delayMs = queued?.delayMs ?? this.options.sessionFilesDelayMsByPath?.[payload.path ?? ""] ?? this.options.sessionFilesDelayMs;
        if (delayMs) {
          // 中文注释：文件树是终端旁路信息；测试用延迟模拟它被大输出或差网络排队。
          await new Promise((resolve) => setTimeout(resolve, delayMs));
        }
        this.sendInner(connection, envelope("session_files_result", queued?.files ?? this.resolveSessionFilesResult(payload)));
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
        const gitDelayMs = this.options.sessionGitDelayMsBySession?.[payload.session_id] ?? this.options.sessionGitDelayMs;
        if (gitDelayMs) {
          // 中文注释：Git 状态请求可能迟到；App 必须按当前 session 代际决定是否接受结果。
          await new Promise((resolve) => setTimeout(resolve, gitDelayMs));
        }
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
        const diffDelayMs = this.options.sessionGitDiffDelayMsByPath?.[payload.file_path ?? payload.worktree_path];
        if (diffDelayMs) {
          // 中文注释：测试慢 diff 返回，用来覆盖 UI requestId/path 防过期逻辑。
          await new Promise((resolve) => setTimeout(resolve, diffDelayMs));
        }
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
        const payload = inner.payload as { session_id: UUID; path: string; max_bytes?: number };
        if (!this.ensureAttached(connection, payload.session_id)) {
          return;
        }
        this.sessionFileReadRequests.push({ session_id: payload.session_id, path: payload.path });
        const readDelayMs = this.options.sessionFileReadDelayMsByPath?.[payload.path];
        if (readDelayMs) {
          // 中文注释：测试慢文件读取返回，用来确认旧响应不会复活或覆盖当前编辑器。
          await new Promise((resolve) => setTimeout(resolve, readDelayMs));
        }
        const result =
          this.options.sessionFileReads?.[payload.path] ??
          ({
            session_id: payload.session_id,
            path: payload.path,
            data_base64: sessionDataToBase64(new TextEncoder().encode("downloaded mock file\n")),
            size_bytes: 21,
            modified_at_ms: null,
          } satisfies SessionFileReadResultPayload);
        if (payload.max_bytes !== undefined && result.size_bytes > payload.max_bytes) {
          this.sendInner(connection, envelope("error", { code: "invalid_envelope", message: "message envelope is invalid" }));
          return;
        }
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
        const writeDelayMs = this.options.sessionFileWriteDelayMsByPath?.[payload.path];
        if (writeDelayMs) {
          // 中文注释：这里先记录“daemon 已收到写请求”，再延迟提交和响应，
          // 让测试可以先观察到 request，再切 session，最后才收到迟到 ack。
          await new Promise((resolve) => setTimeout(resolve, writeDelayMs));
        }
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
        if (this.options.pingDelayMs) {
          await new Promise((resolve) => setTimeout(resolve, this.options.pingDelayMs));
        }
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
      resize_owner: true,
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
    if (this.options.createOutputBeforeResponse && connection.activeRequest?.stream_id) {
      this.appendSessionOutput(created.session_id, this.options.createOutputBeforeResponse);
      // 中文注释：create response 前先到达的输出也属于同一条 terminal_seq 时间线。
      // 后续 attach_sync/live output 必须从它之后继续递增，不能再从 1 开始。
      const terminalSeq = this.claimNextMockTerminalSeq(created.session_id);
      const preResponseFrame = encodeSupervisorTerminalServerFrame({
        type: "terminal_frame",
        session_id: created.session_id,
        frame: {
          kind: "output",
          session_id: created.session_id,
          terminal_seq: terminalSeq,
          data_bytes: new TextEncoder().encode(this.options.createOutputBeforeResponse),
        },
      });
      this.sendPacket(connection, {
        version: PROTOCOL_PACKET_VERSION,
        kind: "stream_chunk",
        stream_id: connection.activeRequest.stream_id,
        seq: 1,
        payload: buildAttachFramePayload(created.session_id, preResponseFrame),
      });
    }
    if (this.options.attachOutput) {
      // 中文注释：`terminal.create` 的 response 返回后，前端会立刻消费 attach_sync 作为首屏。
      // attachOutput 必须先写进权威 snapshot，不能等 response 发完再补。
      this.appendSessionOutput(created.session_id, this.options.attachOutput);
    }
    this.sendInner(connection, envelope("session_created", created));
    const scope = this.issueSessionScope(connection, created.session_id);
    if (scope) {
      this.sendInner(connection, envelope("session_scope_grant", scope));
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

  private handleMetadataSubscribe(connection: MockConnection, payload: MetadataSubscribePayload): void {
    const activeRequest = connection.activeRequest;
    if (activeRequest && !connection.respondedToActiveRequest) {
      this.sendPacketResponse(connection, activeRequest, {});
    }
    if (payload.clients) {
      this.sendPacketEvent(connection, "daemon.clients_snapshot", {
        clients: this.options.daemonClients ?? [],
      });
    }
    if (payload.status_interval_ms !== undefined && payload.status_interval_ms !== null) {
      this.sendPacketEvent(
        connection,
        "daemon.status_snapshot",
        this.options.daemonStatus ?? mockDaemonStatus(),
      );
    }
  }

  private issueSessionScope(connection: MockConnection, sessionId: UUID): SessionScopeGrantPayload | undefined {
    if (!connection.deviceId) {
      return undefined;
    }
    const grant: SessionScopeGrantPayload = {
      session_id: sessionId,
      token: `mock-session-scope-${connection.deviceId}-${sessionId}`,
      expires_at_ms: this.options.sessionScopeExpiresAtMs ?? nowMs() + 60_000,
    };
    this.sessionScopes.set(grant.token, {
      deviceId: connection.deviceId,
      sessionId,
      token: grant.token,
      expiresAtMs: grant.expires_at_ms,
    });
    return grant;
  }

  private appendSessionOutput(sessionId: UUID, text: string): void {
    const current = this.sessionOutputSnapshots.get(sessionId) ?? "";
    this.sessionOutputSnapshots.set(sessionId, `${current}${text}`);
  }

  private resolveSessionFilesResult(payload: { session_id: UUID; path?: string | null }): SessionFilesResultPayload {
    // 指定 path 时必须按该目录返回，避免测试里把“任意切换目录”误回退成 session 根目录。
    const lookupPath =
      payload.path && payload.path.trim()
        ? payload.path
        : this.sessionFilePositions.get(payload.session_id) ?? payload.session_id;
    const files = this.options.sessionFiles?.[lookupPath];
    if (files) {
      this.sessionFilePositions.set(payload.session_id, files.path);
    }
    return files ?? {
      session_id: payload.session_id,
      path: payload.path ?? this.sessionFilePositions.get(payload.session_id) ?? "",
      entries: [],
    };
  }

  private dequeueSessionFilesResponse(
    payload: { session_id: UUID; path?: string | null },
  ): QueuedSessionFilesResponse | undefined {
    const index = this.queuedSessionFilesResponses.findIndex((entry) =>
      entry.sessionId === payload.session_id &&
      (entry.path ?? null) === (payload.path ?? null),
    );
    if (index < 0) {
      return undefined;
    }
    return this.queuedSessionFilesResponses.splice(index, 1)[0];
  }

  private cleanupClosedSession(sessionId: UUID): void {
    // 中文注释：真实 daemon 关闭 session 后，这个 session 对所有连接都会立刻失效。
    // mock 也必须做全局清理，避免多连接场景里另一条连接继续保留 attach/watch 权限。
    for (const connection of this.connections) {
      connection.attachedSessionIds.delete(sessionId);
      connection.watchedSessionIds.delete(sessionId);
      connection.terminalStreamsBySession.delete(sessionId);
      for (const [streamId, stream] of [...connection.terminalStreamsById.entries()]) {
        if (stream.sessionId === sessionId) {
          connection.terminalStreamsById.delete(streamId);
        }
      }
    }
    this.sessionFilePositions.delete(sessionId);
    this.sessionOutputSnapshots.delete(sessionId);
    this.sessionTerminalNextSeq.delete(sessionId);
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

  private throwIfHttpAborted(connection: MockConnection): void {
    if (connection.httpPackets && connection.aborted) {
      throw new DOMException("The operation was aborted", "AbortError");
    }
  }

  private broadcastSessionResized(
    sessionId: UUID,
    size: TerminalSize,
    sourceConnection?: MockConnection,
  ): void {
    for (const connection of this.connections) {
      if (sourceConnection && connection === sourceConnection) {
        continue;
      }
      if (connection.routed && connection.watchedSessionIds.has(sessionId)) {
        this.sendInner(
          connection,
          envelope("session_resized", {
            session_id: sessionId,
            size,
            resize_owner: true,
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
      }),
      String(payload.signature),
    );
    if (!ok) {
      this.sendError(connection, "auth_failed", "auth failed");
    }
  }

  private sendInner(connection: MockConnection, inner: Envelope): void {
    if (connection.httpPackets) {
      const activeRequest = connection.activeRequest;
      if (activeRequest && !connection.respondedToActiveRequest) {
        this.sendPacketResponse(connection, activeRequest, inner.payload);
        return;
      }
      this.sendPacketEvent(connection, this.legacyEventMethod(inner.type), inner.payload);
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
    if (connection.httpPackets) {
      this.sendPacketError(connection, connection.activeRequest, code, message);
      return;
    }
    if (connection.activeRequest) {
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
    this.sendInitialSupervisorAttachSync(connection, request, payload);
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
    if (connection.httpPackets) {
      connection.httpPackets.push(packet);
      return;
    }
    this.sentPackets.push(packet);
    this.sentPacketLog.push({ connection_id: connection.id, packet });
    if (connection.binaryMode) {
      const binaryPacket = protocolPacketToBinary(
        packet,
        this.binaryEncodingOptionsForPacket(connection, packet),
      );
      this.recordBinaryPacket("out", binaryPacket);
      const wire = encodeBinaryProtocolPacket(binaryPacket);
      this.binaryWireFrames.push({ direction: "out", byteLength: wire.byteLength });
      connection.socket.send(wire);
      return;
    }
    this.sendOuter(connection.socket, envelope("packet", packet));
  }

  private binaryEncodingOptionsForPacket(
    connection: MockConnection,
    packet: ProtocolPacket,
  ): import("../protocol/packet-codec").ProtocolPacketBinaryEncodingOptions | undefined {
    if (packet.kind !== "stream_chunk" || !packet.stream_id) {
      return undefined;
    }
    const payload = packet.payload as {
      session_id?: unknown;
      data_base64?: unknown;
      data_bytes?: unknown;
      offset_bytes?: unknown;
      kind?: unknown;
    };
    const isOpaqueAttachPayload =
      typeof payload.session_id === "string"
      && payload.kind === undefined
      && payload.offset_bytes === undefined
      && (typeof payload.data_base64 === "string" || payload.data_bytes instanceof Uint8Array);
    if (!isOpaqueAttachPayload) {
      return undefined;
    }
    if (connection.terminalStreamsById.has(packet.stream_id)) {
      return { streamChunkPayloadType: "attach_frame" };
    }
    if (
      connection.activeRequest?.stream_id === packet.stream_id
      && String(connection.activeRequest.method ?? "").startsWith("terminal.")
    ) {
      return { streamChunkPayloadType: "attach_frame" };
    }
    return undefined;
  }

  private recordBinaryPacket(direction: "in" | "out", packet: BinaryProtocolPacket): void {
    if (packet.payload?.type === "attach_frame") {
      const payload = direction === "out"
        ? decodeSupervisorTerminalServerFrame(packet.payload.data)
        : decodeSupervisorTerminalClientFrame(packet.payload.data);
      this.binaryPacketLog.push({
        direction,
        kind: packet.kind,
        payload_type: packet.payload.type,
        data_text: attachFrameDebugText(payload),
      });
      return;
    }
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
    if (packet.payload?.type === "file_chunk") {
      this.binaryPacketLog.push({
        direction,
        kind: packet.kind,
        payload_type: packet.payload.type,
        data_text: decodeUtf8(packet.payload.data),
      });
      return;
    }
    this.binaryPacketLog.push({
      direction,
      kind: packet.kind,
      payload_type: packet.payload?.type,
    });
  }

  private sendTerminalStreamFrame(connection: MockConnection, sessionId: UUID, payload: unknown): void {
    const stream = connection.terminalStreamsBySession.get(sessionId);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    const frame = this.normalizeSupervisorTerminalFrame(sessionId, payload);
    this.noteTerminalSequencePayload(sessionId, frame.frame);
    this.sendSupervisorTerminalFrame(connection, sessionId, frame);
  }

  private sendTerminalStreamBatch(connection: MockConnection, sessionId: UUID, frames: unknown[]): void {
    const stream = connection.terminalStreamsBySession.get(sessionId);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    this.noteTerminalSequencePayload(sessionId, { kind: "batch", frames });
    for (const frame of frames) {
      this.sendSupervisorTerminalFrame(connection, sessionId, this.normalizeSupervisorTerminalFrame(sessionId, frame));
    }
  }

  private sendSupervisorTerminalFrame(
    connection: MockConnection,
    sessionId: UUID,
    frame: SupervisorTerminalServerFrame,
  ): void {
    const stream = connection.terminalStreamsBySession.get(sessionId);
    if (!stream || !stream.watchUpdates) {
      return;
    }
    const seq = stream.nextTransportSeq;
    stream.nextTransportSeq += 1;
    const payload = buildAttachFramePayload(sessionId, encodeSupervisorTerminalServerFrame(frame));
    this.sendPacket(connection, {
      version: PROTOCOL_PACKET_VERSION,
      kind: "stream_chunk",
      stream_id: stream.streamId,
      seq,
      payload,
    });
  }

  private nextMockTerminalSeqBase(sessionId: UUID): number {
    return (this.sessionTerminalNextSeq.get(sessionId) ?? 1) - 1;
  }

  private claimNextMockTerminalSeq(sessionId: UUID): number {
    const nextSeq = this.sessionTerminalNextSeq.get(sessionId) ?? 1;
    this.sessionTerminalNextSeq.set(sessionId, nextSeq + 1);
    return nextSeq;
  }

  private noteTerminalSequencePayload(sessionId: UUID, payload: unknown): void {
    const nextSeq = this.sessionTerminalNextSeq.get(sessionId) ?? 1;
    const nextAfterPayload = (() => {
      if (!payload || typeof payload !== "object") {
        return nextSeq;
      }
      const packetPayload = payload as { kind?: unknown; frames?: unknown };
      if (packetPayload.kind === "batch" && Array.isArray(packetPayload.frames)) {
        let candidate = nextSeq;
        for (const frame of packetPayload.frames) {
          candidate = Math.max(candidate, this.terminalSequenceCeilingFromPayload(candidate, frame));
        }
        return candidate;
      }
      return this.terminalSequenceCeilingFromPayload(nextSeq, packetPayload);
    })();
    if (nextAfterPayload > nextSeq) {
      this.sessionTerminalNextSeq.set(sessionId, nextAfterPayload);
    }
  }

  private terminalSequenceCeilingFromPayload(current: number, payload: unknown): number {
    if (!payload || typeof payload !== "object") {
      return current;
    }
    const frame = payload as { terminal_seq?: unknown; base_seq?: unknown };
    const terminalSeq = typeof frame.terminal_seq === "number" ? frame.terminal_seq : undefined;
    if (terminalSeq !== undefined) {
      return Math.max(current, terminalSeq + 1);
    }
    const baseSeq = typeof frame.base_seq === "number" ? frame.base_seq : undefined;
    if (baseSeq !== undefined) {
      return Math.max(current, baseSeq + 1);
    }
    return current;
  }

  private sendInitialSupervisorAttachSync(connection: MockConnection, request: ProtocolPacket, payload: unknown): void {
    if (request.kind !== "stream_open" || !request.stream_id || !String(request.method ?? "").startsWith("terminal.")) {
      return;
    }
    const response = payload as { session_id?: UUID; size?: TerminalSize };
    if (!response.session_id) {
      return;
    }
    const stream = connection.terminalStreamsBySession.get(response.session_id);
    if (!stream?.watchUpdates) {
      return;
    }
    const snapshotText = this.sessionOutputSnapshots.get(response.session_id) ?? "";
    const snapshotBytes = new TextEncoder().encode(snapshotText);
    const requestPayload = request.payload as { last_terminal_seq?: number | null };
    const requestedLastTerminalSeq =
      typeof requestPayload.last_terminal_seq === "number" ? requestPayload.last_terminal_seq : undefined;
    const snapshotSize = response.size ?? { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    const currentBaseSeq = this.nextMockTerminalSeqBase(response.session_id);
    let baseSeq = currentBaseSeq;
    let retainedOutputBytes = new Uint8Array();
    let frames: SingleTerminalFramePayload[] = [];

    if (request.method === "terminal.create" && this.options.createOutputBeforeResponse) {
      // 中文注释：create response 前如果已经通过同一条 stream 推过首屏 output，
      // response 后的 attach_sync 只能作为“已追平”的确认，不能再回放同样的字节。
      baseSeq = currentBaseSeq;
    } else if (requestedLastTerminalSeq !== undefined) {
      if (requestedLastTerminalSeq >= currentBaseSeq) {
        // 中文注释：client 已经追平当前 terminal_seq 时，重连 bootstrap 只需要确认
        // base_seq，不应该再回放 retained_output，否则页面会把旧内容再渲染一遍。
        baseSeq = requestedLastTerminalSeq;
      } else if (snapshotBytes.byteLength > 0) {
        // 中文注释：mock 不维护完整 tail journal；当 last_terminal_seq 落后时，
        // 回退成权威 snapshot frame，模拟真实 supervisor 的“尾巴不可用则重建快照”语义。
        frames = [{
          kind: "snapshot",
          session_id: response.session_id,
          base_seq: currentBaseSeq,
          size: snapshotSize,
          data_bytes: snapshotBytes,
        }];
      }
    } else {
      // 中文注释：生产 supervisor 的 terminal attach 首屏只通过 frames.snapshot 传输。
      // MockDaemon 保持同样语义，避免测试继续固定旧的 retained_output 双播模型。
      frames = [{
        kind: "snapshot",
        session_id: response.session_id,
        base_seq: currentBaseSeq,
        size: snapshotSize,
        data_bytes: snapshotBytes,
      }];
    }
    this.sendSupervisorTerminalFrame(connection, response.session_id, {
      type: "attach_sync",
      session_id: response.session_id,
      // 中文注释：base_seq 表达 snapshot/tail 已覆盖到的 terminal_seq，
      // 不能再拿 packet transport seq 来推导，否则首条 live output 会被误判成缺口。
      base_seq: baseSeq,
      snapshot: {
        size: snapshotSize,
        process_id: 7,
        retained_output_bytes: retainedOutputBytes,
      },
      frames,
    });
  }

  private normalizeSupervisorTerminalFrame(sessionId: UUID, payload: unknown): SupervisorTerminalServerFrame & { type: "terminal_frame" } {
    if (
      payload &&
      typeof payload === "object" &&
      (payload as { type?: unknown }).type === "terminal_frame"
    ) {
      return payload as SupervisorTerminalServerFrame & { type: "terminal_frame" };
    }
    return {
      type: "terminal_frame",
      session_id: sessionId,
      frame: payload as SingleTerminalFramePayload,
    };
  }

  private registerTerminalStreamForResponse(connection: MockConnection, request: ProtocolPacket, payload: unknown): void {
    if (request.kind !== "stream_open" || !request.stream_id || !String(request.method ?? "").startsWith("terminal.")) {
      return;
    }
    if (connection.pendingCanceledTerminalStreamIds.delete(request.stream_id)) {
      return;
    }
    const response = payload as { session_id?: UUID };
    if (!response.session_id) {
      return;
    }
    const retainedOutputBytes = new TextEncoder().encode(this.sessionOutputSnapshots.get(response.session_id) ?? "");
    const stream: MockTerminalStream = {
      sessionId: response.session_id,
      streamId: request.stream_id,
      // 中文注释：transport seq 只给 packet stream 自身使用；create 若已在 response 前
      // 推过一个 chunk，后续 transport seq 要从 2 继续。
      nextTransportSeq: request.method === "terminal.create" && this.options.createOutputBeforeResponse ? 2 : 1,
      watchUpdates: true,
    };
    if (!this.sessionTerminalNextSeq.has(response.session_id)) {
      this.sessionTerminalNextSeq.set(response.session_id, retainedOutputBytes.byteLength > 0 ? 2 : 1);
    }
    connection.terminalStreamsById.set(stream.streamId, stream);
    connection.terminalStreamsBySession.set(stream.sessionId, stream);
  }

  private removeTerminalStream(connection: MockConnection, streamId?: PacketStreamId): boolean {
    if (!streamId) {
      return false;
    }
    const stream = connection.terminalStreamsById.get(streamId);
    if (!stream) {
      return false;
    }
    connection.terminalStreamsById.delete(streamId);
    connection.terminalStreamsBySession.delete(stream.sessionId);
    connection.watchedSessionIds.delete(stream.sessionId);
    return true;
  }

  private legacyEventMethod(type: Envelope["type"]): string {
    return protocolEventMethodForLegacyEnvelopeType(type);
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

function concatByteChunks(chunks: Uint8Array[]): Uint8Array {
  const length = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
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

function attachFrameDebugText(payload: ReturnType<typeof decodeSupervisorTerminalServerFrame> | ReturnType<typeof decodeSupervisorTerminalClientFrame>): string | undefined {
  if (payload.type === "input") {
    return decodeUtf8(payload.data_bytes);
  }
  if (payload.type === "terminal_frame" && payload.frame.kind === "output") {
    return decodeUtf8(payload.frame.data_bytes ?? new Uint8Array());
  }
  if (payload.type !== "attach_sync") {
    return undefined;
  }
  const chunks: string[] = [];
  if (payload.snapshot.retained_output_bytes.byteLength > 0) {
    chunks.push(decodeUtf8(payload.snapshot.retained_output_bytes));
  }
  for (const frame of payload.frames) {
    if (frame.kind === "snapshot" || frame.kind === "output") {
      chunks.push(decodeUtf8(frame.data_bytes ?? new Uint8Array()));
    }
  }
  return chunks.length > 0 ? chunks.join("") : undefined;
}
