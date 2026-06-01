import {
  authPayloadForChallenge,
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  e2eeAuthTranscriptDigestWire,
  signAuthPayload,
  signHttpE2eeAuthPayload,
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
  SessionFileDownloadStreamPayload,
  SessionFileDownloadStreamReadyPayload,
  SessionFileHttpDownloadPayload,
  SessionFileHttpUploadReadyPayload,
  SessionFileHttpUploadStreamPayload,
  SessionFileReadPayload,
  SessionFileReadResultPayload,
  SessionFileTransferChunkPayload,
  SessionFileUploadPayload,
  SessionFileUploadProgressPayload,
  SessionFileUploadReadyPayload,
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
  socketOpenTimeoutMs?: number;
  socketOpenHedgeDelayMs?: number;
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

interface FileStreamWaiter<T> {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
}

interface FileUploadStreamState {
  kind: "upload";
  sessionId: UUID;
  streamId: PacketStreamId;
  progress: SessionFileUploadProgressPayload[];
  waiters: Array<FileStreamWaiter<SessionFileUploadProgressPayload>>;
  closed: boolean;
}

interface FileDownloadChunk {
  session_id: UUID;
  offset_bytes: number;
  data_bytes: Uint8Array;
  size_bytes: number;
  eof: boolean;
}

interface FileDownloadStreamState {
  kind: "download";
  sessionId: UUID;
  streamId: PacketStreamId;
  chunks: FileDownloadChunk[];
  waiters: Array<FileStreamWaiter<FileDownloadChunk>>;
  closed: boolean;
}

type FileStreamState = FileUploadStreamState | FileDownloadStreamState;

const DEFAULT_TIMEOUT_MS = 30000;
const RECEIVE_PUMP_YIELD_MESSAGES = 64;
const RECEIVE_PUMP_YIELD_BYTES = 256 * 1024;
const FILE_TRANSFER_CHUNK_BYTES = 256 * 1024;
const HTTP_UPLOAD_CHUNK_BYTES = 10 * 1024 * 1024;
const HTTP_UPLOAD_MAX_PARALLEL_CHUNKS = 2;
// 中文注释：10MiB 是一次 HTTP POST 的业务分片大小；E2EE frame 仍必须小于
// daemon/浏览器共同的 2MiB frame cap，所以每个 POST 内部再拆成较小加密帧。
const HTTP_UPLOAD_FRAME_PLAINTEXT_BYTES = 1024 * 1024;
// 中文注释：正常上传仍依赖 HTTP 连接背压；这个宽限只处理连接半开但 fetch 永不返回的故障态。
// 10MiB/10min 约等于 17KiB/s，已经覆盖很差的实际网络。
const HTTP_UPLOAD_CHUNK_TIMEOUT_MS = 10 * 60 * 1000;
const HTTP_UPLOAD_ABORT_TIMEOUT_MS = 5000;
const FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES = 16 * 1024 * 1024;
const FILE_TRANSFER_WEBSOCKET_COMPAT_MAX_BYTES = 16 * 1024 * 1024;
// 中文注释：RPC file_write/file_read 只服务浏览器内置文本编辑器和很小的兼容上传；
// 大文件必须走 HTTP E2EE 或 binary stream，不能再整包 base64 进入 RPC。
const SESSION_FILE_RPC_MAX_BYTES = 1024 * 1024;
const HTTP_E2EE_MAX_FRAME_BYTES = 2 * 1024 * 1024;
const HTTP_E2EE_MAX_PENDING_BYTES = 4 + HTTP_E2EE_MAX_FRAME_BYTES;

interface HttpE2eeFetchOptions {
  timeoutMs?: number;
  firstFrameTimeoutMs?: number;
  onFrame?: (frame: Uint8Array) => void | Promise<void>;
  collectFrames?: boolean;
  signal?: AbortSignal;
}

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
  private readonly fileStreamsById = new Map<PacketStreamId, FileStreamState>();
  private authenticatedDevice?: DeviceState;
  private authenticatedServer?: PairedServerState;

  private constructor(
    private readonly socket: WebSocket,
    private readonly inbox: SocketInbox,
    private readonly socketUrl: string,
    private readonly serverIdValue: UUID,
    private readonly deviceId: UUID,
    private readonly daemonE2eePublicKeyWire: PublicKeyWire,
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
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const socketOpenTimeoutMs = options.socketOpenTimeoutMs ?? timeoutMs;
    const requestTimeoutMs = options.requestTimeoutMs ?? timeoutMs;
    const abortSignal = options.signal;
    let socket: WebSocket | undefined;
    let inbox: SocketInbox | undefined;
    const closeSocketOnAbort = () => socket?.close();
    abortSignal?.addEventListener("abort", closeSocketOnAbort, { once: true });

    try {
      throwIfAborted(abortSignal);
      socket = await openWebSocket(url, {
        timeoutMs: socketOpenTimeoutMs,
        hedgeDelayMs: options.socketOpenHedgeDelayMs,
        webSocketFactory: options.webSocketFactory,
        signal: abortSignal,
      });
      inbox = new SocketInbox(socket);

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
      const client = new DirectClient(
        socket,
        inbox,
        url,
        routeServerId,
        deviceId,
        daemonKeyExchange.public_key,
        e2ee,
        { timeoutMs, requestTimeoutMs },
        binaryMode,
      );
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
      socket?.close();
      inbox?.rejectPending(new ProtocolClientError("connection_closed", "connection closed"));
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
    this.authenticatedDevice = device;
    this.authenticatedServer = server;
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

  async readSessionFile(sessionId: UUID, path: string, options: { maxBytes?: number } = {}): Promise<SessionFileReadResultPayload> {
    return this.request<SessionFileReadResultPayload>(
      "session.file_read",
      {
        session_id: sessionId,
        path,
        ...(options.maxBytes !== undefined ? { max_bytes: options.maxBytes } : {}),
      } satisfies SessionFileReadPayload,
    );
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

  async uploadSessionFile(
    sessionId: UUID,
    path: string,
    file: Blob,
    options: {
      onProgress?: (progress: SessionFileUploadProgressPayload) => void;
      onSentProgress?: (sentBytes: number, sizeBytes: number) => void;
      timeoutMs?: number;
    } = {},
  ): Promise<SessionFileUploadProgressPayload> {
    if (this.authenticatedDevice && this.authenticatedServer) {
      try {
        return await this.uploadSessionFileHttp(sessionId, path, file, options);
      } catch (error) {
        if (!isHttpFileTransferUnsupported(error)) {
          throw error;
        }
        if (file.size > FILE_TRANSFER_WEBSOCKET_COMPAT_MAX_BYTES) {
          throw new ProtocolClientError(
            "file_too_large",
            "HTTP file upload is required for large files",
          );
        }
        // 中文注释：只有 404/426 这类“老 daemon/old relay 明确不支持 HTTP 文件端点”
        // 才允许小文件走 WebSocket 兼容路径；网络 TypeError 不会进入这里，避免大文件
        // 在 relay 下重新退回主 WebSocket 后逐块等待确认。
      }
    }
    if (!this.binaryMode) {
      return this.uploadSessionFileLegacy(sessionId, path, file, options);
    }
    this.ensureBinaryFileTransfer();
    const streamId = randomUuid();
    const state: FileUploadStreamState = {
      kind: "upload",
      sessionId,
      streamId,
      progress: [],
      waiters: [],
      closed: false,
    };
    this.fileStreamsById.set(streamId, state);
    try {
      const id = randomUuid();
      await this.sendTrackedPacket<SessionFileUploadReadyPayload>(
        {
          version: PROTOCOL_PACKET_VERSION,
          kind: "stream_open",
          id,
          stream_id: streamId,
          method: "session.file_upload",
          payload: {
            session_id: sessionId,
            path,
            size_bytes: file.size,
          } satisfies SessionFileUploadPayload,
        },
        id,
        "session.file_upload",
        options.timeoutMs ?? this.timeoutMs,
      );
    } catch (error) {
      this.removeFileStream(streamId, error instanceof Error ? error : new Error("file_upload_failed"));
      throw error;
    }

    let offset = 0;
    let seq = 1;
    let lastProgress: SessionFileUploadProgressPayload | undefined;
    try {
      while (offset < file.size || (file.size === 0 && seq === 1)) {
        const end = Math.min(file.size, offset + FILE_TRANSFER_CHUNK_BYTES);
        const bytes = await blobSliceBytes(file, offset, end);
        const eof = end >= file.size;
        this.sendPacket({
          version: PROTOCOL_PACKET_VERSION,
          kind: "stream_chunk",
          stream_id: streamId,
          seq,
          payload: {
            session_id: sessionId,
            offset_bytes: offset,
            data_bytes: bytes,
            size_bytes: file.size,
            eof,
          } satisfies SessionFileTransferChunkPayload,
        });
        seq += 1;
        const progress = await this.waitForFileUploadProgress(state, options.timeoutMs ?? this.timeoutMs);
        if (progress.session_id !== sessionId || progress.size_bytes !== file.size) {
          throw new ProtocolClientError("invalid_file_transfer", "file upload progress does not match request");
        }
        if (progress.offset_bytes < end || progress.offset_bytes > file.size) {
          throw new ProtocolClientError("invalid_file_transfer", "file upload progress is out of bounds");
        }
        if (progress.eof !== (progress.offset_bytes === file.size)) {
          throw new ProtocolClientError("invalid_file_transfer", "file upload completion was not confirmed");
        }
        lastProgress = progress;
        options.onProgress?.(progress);
        offset = progress.offset_bytes;
        if (progress.eof) {
          break;
        }
      }
      if (!lastProgress) {
        throw new ProtocolClientError("invalid_file_transfer", "file upload did not report progress");
      }
      this.removeFileStream(streamId);
      return lastProgress;
    } catch (error) {
      this.sendPacketBestEffort({
        version: PROTOCOL_PACKET_VERSION,
        kind: "cancel",
        stream_id: streamId,
        payload: { reason: "file_upload_cancelled" },
      });
      this.removeFileStream(streamId, error instanceof Error ? error : new Error("file_upload_failed"));
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
      timeoutMs?: number;
    } = {},
  ): Promise<{ path: string; name: string; bytes: Uint8Array; size_bytes: number; modified_at_ms?: number | null }> {
    if (this.authenticatedDevice && this.authenticatedServer) {
      return await this.downloadSessionFileHttp(sessionId, path, options);
    }
    if (!this.binaryMode) {
      return this.downloadSessionFileLegacy(sessionId, path, options);
    }
    this.ensureBinaryFileTransfer();
    const streamId = randomUuid();
    const state: FileDownloadStreamState = {
      kind: "download",
      sessionId,
      streamId,
      chunks: [],
      waiters: [],
      closed: false,
    };
    this.fileStreamsById.set(streamId, state);
    let ready: SessionFileDownloadStreamReadyPayload;
    try {
      const id = randomUuid();
      ready = await this.sendTrackedPacket<SessionFileDownloadStreamReadyPayload>(
        {
          version: PROTOCOL_PACKET_VERSION,
          kind: "stream_open",
          id,
          stream_id: streamId,
          method: "session.file_download",
          payload: {
            session_id: sessionId,
            path,
          } satisfies SessionFileDownloadStreamPayload,
        },
        id,
        "session.file_download",
        options.timeoutMs ?? this.timeoutMs,
      );
    } catch (error) {
      this.removeFileStream(streamId, error instanceof Error ? error : new Error("file_download_failed"));
      throw error;
    }

    const collectBytes = options.collectBytes ?? true;
    if (collectBytes && ready.size_bytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
      const error = new ProtocolClientError("file_too_large", "file is too large to buffer in browser memory");
      this.sendPacketBestEffort({
        version: PROTOCOL_PACKET_VERSION,
        kind: "cancel",
        stream_id: streamId,
        payload: { reason: "file_download_cancelled" },
      });
      this.removeFileStream(streamId, error);
      throw error;
    }
    const chunks: Uint8Array[] = [];
    let receivedBytes = 0;
    try {
      while (true) {
        this.sendPacket({
          version: PROTOCOL_PACKET_VERSION,
          kind: "flow",
          stream_id: streamId,
          ack: receivedBytes,
          credit: FILE_TRANSFER_CHUNK_BYTES,
          payload: {},
        });
        const chunk = await this.waitForFileDownloadChunk(state, options.timeoutMs ?? this.timeoutMs);
        if (chunk.session_id !== sessionId || chunk.offset_bytes !== receivedBytes) {
          throw new ProtocolClientError("invalid_file_transfer", "file download chunk is out of order");
        }
        const nextReceivedBytes = receivedBytes + chunk.data_bytes.byteLength;
        if (nextReceivedBytes > ready.size_bytes || nextReceivedBytes > chunk.size_bytes) {
          throw new ProtocolClientError("invalid_file_transfer", "file download returned more bytes than declared");
        }
        receivedBytes = nextReceivedBytes;
        if (collectBytes) {
          chunks.push(chunk.data_bytes);
        }
        // 下载走二进制 stream；支持 showSaveFilePicker 时这里直接写入磁盘，避免把大文件完整攒在内存里。
        await options.onChunk?.(chunk.data_bytes, receivedBytes, chunk.size_bytes);
        options.onProgress?.(receivedBytes, chunk.size_bytes);
        if (chunk.eof) {
          if (receivedBytes !== ready.size_bytes) {
            throw new ProtocolClientError("invalid_file_transfer", "file download ended before all bytes arrived");
          }
          this.removeFileStream(streamId);
          return {
            path: ready.path,
            name: ready.name,
            bytes: concatByteChunks(chunks),
            size_bytes: ready.size_bytes,
            modified_at_ms: ready.modified_at_ms,
          };
        }
      }
    } catch (error) {
      this.sendPacketBestEffort({
        version: PROTOCOL_PACKET_VERSION,
        kind: "cancel",
        stream_id: streamId,
        payload: { reason: "file_download_cancelled" },
      });
      this.removeFileStream(streamId, error instanceof Error ? error : new Error("file_download_failed"));
      throw error;
    }
  }

  private async uploadSessionFileLegacy(
    sessionId: UUID,
    path: string,
    file: Blob,
    options: {
      onProgress?: (progress: SessionFileUploadProgressPayload) => void;
      onSentProgress?: (sentBytes: number, sizeBytes: number) => void;
      timeoutMs?: number;
    } = {},
  ): Promise<SessionFileUploadProgressPayload> {
    if (file.size > SESSION_FILE_RPC_MAX_BYTES) {
      throw new ProtocolClientError("file_too_large", "legacy RPC file upload is limited to editor-sized files");
    }
    const bytes = await readBlobBytes(file);
    const written = await this.writeSessionFile(sessionId, path, bytes);
    const progress: SessionFileUploadProgressPayload = {
      session_id: sessionId,
      path: written.path,
      offset_bytes: written.size_bytes,
      size_bytes: written.size_bytes,
      eof: true,
      modified_at_ms: written.modified_at_ms,
    };
    options.onProgress?.(progress);
    return progress;
  }

  private async downloadSessionFileLegacy(
    sessionId: UUID,
    path: string,
    options: {
      onProgress?: (receivedBytes: number, sizeBytes: number) => void;
      onChunk?: (bytes: Uint8Array, receivedBytes: number, sizeBytes: number) => void | Promise<void>;
      collectBytes?: boolean;
      timeoutMs?: number;
    } = {},
  ): Promise<{ path: string; name: string; bytes: Uint8Array; size_bytes: number; modified_at_ms?: number | null }> {
    const ready = await this.prepareSessionFileDownload(sessionId, path);
    const chunks: Uint8Array[] = [];
    const collectBytes = options.collectBytes ?? true;
    if (collectBytes && ready.size_bytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
      throw new ProtocolClientError("file_too_large", "file is too large to buffer in browser memory");
    }
    let offset = 0;
    while (true) {
      const chunk = await this.readSessionFileDownloadChunk(sessionId, path, offset, FILE_TRANSFER_CHUNK_BYTES);
      const bytes = base64ToBytes(chunk.data_base64);
      const nextOffset = offset + bytes.byteLength;
      if (
        nextOffset !== chunk.next_offset_bytes ||
        nextOffset > ready.size_bytes ||
        nextOffset > chunk.size_bytes
      ) {
        throw new ProtocolClientError("invalid_file_transfer", "file download returned more bytes than declared");
      }
      offset = nextOffset;
      if (collectBytes) {
        chunks.push(bytes);
      }
      await options.onChunk?.(bytes, offset, chunk.size_bytes);
      options.onProgress?.(offset, chunk.size_bytes);
      if (chunk.eof) {
        if (offset !== ready.size_bytes) {
          throw new ProtocolClientError("invalid_file_transfer", "file download ended before all bytes arrived");
        }
        return {
          path: ready.path,
          name: fileNameFromPath(ready.path),
          bytes: concatByteChunks(chunks),
          size_bytes: ready.size_bytes,
          modified_at_ms: ready.modified_at_ms,
        };
      }
    }
  }

  private async uploadSessionFileHttp(
    sessionId: UUID,
    path: string,
    file: Blob,
    options: {
      onProgress?: (progress: SessionFileUploadProgressPayload) => void;
      onSentProgress?: (sentBytes: number, sizeBytes: number) => void;
      timeoutMs?: number;
    } = {},
  ): Promise<SessionFileUploadProgressPayload> {
    const readyFrames = await this.httpE2eeRequest("POST", "/api/files/upload/init", [
      encodeUtf8(JSON.stringify({
        session_id: sessionId,
        path,
        size_bytes: file.size,
      } satisfies SessionFileUploadPayload)),
    ], options.timeoutMs);
    const ready = parseHttpJsonFrame<SessionFileHttpUploadReadyPayload>(readyFrames[0]);
    const offsets = file.size === 0
      ? [0]
      : Array.from(
        { length: Math.ceil(file.size / HTTP_UPLOAD_CHUNK_BYTES) },
        (_value, index) => index * HTTP_UPLOAD_CHUNK_BYTES,
      );
    let nextOffsetIndex = 0;
    let lastProgress: SessionFileUploadProgressPayload | undefined;
    let completeProgress: SessionFileUploadProgressPayload | undefined;
    let sentProgressBytes = 0;
    const abortController = new AbortController();
    const reportCommittedProgress = (progress: SessionFileUploadProgressPayload) => {
      // 中文注释：2 并发下响应顺序不等于 offset 顺序；UI 和最终结果只接受单调前进。
      if (!lastProgress || progress.offset_bytes >= lastProgress.offset_bytes) {
        lastProgress = progress;
        options.onProgress?.(progress);
      }
      if (progress.eof) {
        completeProgress = progress;
      }
    };
    const reportSentProgress = (sentBytes: number) => {
      // 中文注释：浏览器 fetch 没有标准上传进度事件；这里表示分片已封包并交给 fetch。
      // 多 worker 并发时仍保证展示进度不回退。
      sentProgressBytes = Math.max(sentProgressBytes, sentBytes);
      options.onSentProgress?.(sentProgressBytes, file.size);
    };
    const uploadWorker = async () => {
      while (true) {
        const index = nextOffsetIndex;
        nextOffsetIndex += 1;
        if (index >= offsets.length) {
          return;
        }
        const progress = await this.uploadSessionFileHttpChunk(
          sessionId,
          path,
          file,
          ready,
          offsets[index],
          {
            timeoutMs: options.timeoutMs ?? HTTP_UPLOAD_CHUNK_TIMEOUT_MS,
            signal: abortController.signal,
            onSentProgress: reportSentProgress,
          },
        );
        if (progress.session_id !== sessionId || progress.size_bytes !== file.size) {
          throw new ProtocolClientError("invalid_file_transfer", "file upload progress does not match request");
        }
        if (progress.offset_bytes > file.size || progress.eof !== (progress.offset_bytes === file.size)) {
          throw new ProtocolClientError("invalid_file_transfer", "file upload progress is out of bounds");
        }
        reportCommittedProgress(progress);
      }
    };
    const workerCount = Math.min(HTTP_UPLOAD_MAX_PARALLEL_CHUNKS, offsets.length);
    const workers = Array.from({ length: workerCount }, () => uploadWorker());
    try {
      await Promise.all(workers);
      if (!completeProgress || completeProgress.offset_bytes !== completeProgress.size_bytes) {
        throw new ProtocolClientError("invalid_file_transfer", "file upload ended before all bytes were stored");
      }
      return completeProgress;
    } catch (error) {
      abortController.abort();
      await Promise.allSettled(workers);
      if (completeProgress && completeProgress.offset_bytes === completeProgress.size_bytes) {
        // 中文注释：2 并发下最后一个分片可能已经提交成功；另一个旧请求随后超时或被取消时，
        // 不能再把已完成上传回滚成失败。
        return completeProgress;
      }
      await this.abortSessionFileHttpUpload(sessionId, path, ready).catch(() => undefined);
      throw error;
    }
  }

  private async uploadSessionFileHttpChunk(
    sessionId: UUID,
    path: string,
    file: Blob,
    ready: SessionFileHttpUploadReadyPayload,
    offset: number,
    options: {
      onSentProgress?: (sentBytes: number, sizeBytes: number) => void;
      timeoutMs?: number;
      signal?: AbortSignal;
    },
  ): Promise<SessionFileUploadProgressPayload> {
    const end = Math.min(file.size, offset + HTTP_UPLOAD_CHUNK_BYTES);
    const chunk = file.size === 0 ? undefined : await blobSliceBytes(file, offset, end);
    const streamContext = await this.createHttpE2eeContext("POST", "/api/files/upload");
    const metaFrame = encodeUtf8(JSON.stringify({
      session_id: sessionId,
      path,
      upload_id: ready.upload_id,
      size_bytes: file.size,
      offset_bytes: offset,
    } satisfies SessionFileHttpUploadStreamPayload));
    const uploadBody = buildHttpUploadChunkBody(streamContext.e2ee, metaFrame, chunk);
    options.onSentProgress?.(end, file.size);
    const progressFrames = await this.httpE2eeFetch(
      "POST",
      "/api/files/upload",
      streamContext.headers,
      uploadBody,
      streamContext.e2ee,
      { timeoutMs: options.timeoutMs, signal: options.signal },
    );
    return parseHttpJsonFrame<SessionFileUploadProgressPayload>(progressFrames[0]);
  }

  private async abortSessionFileHttpUpload(
    sessionId: UUID,
    path: string,
    ready: SessionFileHttpUploadReadyPayload,
  ): Promise<void> {
    const streamContext = await this.createHttpE2eeContext("POST", "/api/files/upload/abort");
    const metaFrame = encodeUtf8(JSON.stringify({
      session_id: sessionId,
      path,
      upload_id: ready.upload_id,
      size_bytes: ready.size_bytes,
      offset_bytes: 0,
    } satisfies SessionFileHttpUploadStreamPayload));
    await this.httpE2eeFetch(
      "POST",
      "/api/files/upload/abort",
      streamContext.headers,
      bodyToArrayBuffer(encodeHttpE2eeFrames(streamContext.e2ee, [metaFrame])),
      streamContext.e2ee,
      { collectFrames: true, timeoutMs: HTTP_UPLOAD_ABORT_TIMEOUT_MS },
    );
  }

  private async downloadSessionFileHttp(
    sessionId: UUID,
    path: string,
    options: {
      onProgress?: (receivedBytes: number, sizeBytes: number) => void;
      onChunk?: (bytes: Uint8Array, receivedBytes: number, sizeBytes: number) => void | Promise<void>;
      collectBytes?: boolean;
      timeoutMs?: number;
    } = {},
  ): Promise<{ path: string; name: string; bytes: Uint8Array; size_bytes: number; modified_at_ms?: number | null }> {
    const collectBytes = options.collectBytes ?? true;
    const chunks: Uint8Array[] = [];
    let receivedBytes = 0;
    let ready: SessionFileDownloadStreamReadyPayload | undefined;
    const context = await this.createHttpE2eeContext("POST", "/api/files/download");
    await this.httpE2eeFetch(
      "POST",
      "/api/files/download",
      context.headers,
      bodyToArrayBuffer(encodeHttpE2eeFrames(context.e2ee, [
        encodeUtf8(JSON.stringify({
          session_id: sessionId,
          path,
          offset_bytes: 0,
        } satisfies SessionFileHttpDownloadPayload)),
      ])),
      context.e2ee,
      {
        // 中文注释：HTTP 下载的文件体可以很长，不能设置整体超时；
        // 但元数据首帧应快速返回，否则 UI 会一直停在“开始下载”状态。
        firstFrameTimeoutMs: options.timeoutMs ?? this.timeoutMs,
        collectFrames: false,
        onFrame: async (frame) => {
          if (!ready) {
            ready = parseHttpJsonFrame<SessionFileDownloadStreamReadyPayload>(frame);
            if (collectBytes && ready.size_bytes > FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES) {
              throw new ProtocolClientError("file_too_large", "file is too large to buffer in browser memory");
            }
            return;
          }
          const nextReceivedBytes = receivedBytes + frame.byteLength;
          if (nextReceivedBytes > ready.size_bytes) {
            throw new ProtocolClientError("invalid_file_transfer", "file download returned more bytes than declared");
          }
          receivedBytes = nextReceivedBytes;
          if (collectBytes) {
            chunks.push(frame);
          }
          await options.onChunk?.(frame, receivedBytes, ready.size_bytes);
          options.onProgress?.(receivedBytes, ready.size_bytes);
        },
      },
    );
    if (!ready) {
      throw new ProtocolClientError("invalid_file_transfer", "file download did not return metadata");
    }
    if (receivedBytes !== ready.size_bytes) {
      throw new ProtocolClientError("invalid_file_transfer", "file download ended before all bytes arrived");
    }
    return {
      path: ready.path,
      name: ready.name,
      bytes: concatByteChunks(chunks),
      size_bytes: ready.size_bytes,
      modified_at_ms: ready.modified_at_ms,
    };
  }

  private async httpE2eeRequest(
    method: "POST",
    path: string,
    plaintextFrames: Uint8Array[],
    timeoutMs = this.timeoutMs,
  ): Promise<Uint8Array[]> {
    const context = await this.createHttpE2eeContext(method, path);
    return this.httpE2eeFetch(
      method,
      path,
      context.headers,
      bodyToArrayBuffer(encodeHttpE2eeFrames(context.e2ee, plaintextFrames)),
      context.e2ee,
      { timeoutMs },
    );
  }

  private async createHttpE2eeContext(
    method: "POST",
    path: string,
  ): Promise<{ e2ee: E2eeSession; headers: Record<string, string> }> {
    const device = this.authenticatedDevice;
    const server = this.authenticatedServer;
    if (!device || !server) {
      throw new HttpFileTransferUnsupported();
    }
    const keypair = generateE2eeKeyPair();
    const e2ee = E2eeSession.device({
      serverId: this.serverIdValue,
      deviceId: this.deviceId,
      localKeypair: keypair,
      daemonPublicKeyWire: this.daemonE2eePublicKeyWire,
    });
    const auth = await signHttpE2eeAuthPayload(
      {
        device_id: device.device_id,
        e2ee_public_key: keypair.publicKeyWire,
        nonce: nonce(),
        timestamp_ms: nowMs(),
        method,
        path,
      },
      server,
      device.device_signing_key_secret,
    );
    return {
      e2ee,
      headers: {
        "content-type": "application/octet-stream",
        "x-termd-server-id": this.serverIdValue,
        "x-termd-device-id": auth.device_id,
        "x-termd-e2ee-public-key": auth.e2ee_public_key,
        "x-termd-e2ee-nonce": auth.nonce,
        "x-termd-e2ee-timestamp-ms": String(auth.timestamp_ms),
        "x-termd-e2ee-signature": auth.signature,
      },
    };
  }

  private async httpE2eeFetch(
    method: "POST",
    path: string,
    headers: Record<string, string>,
    body: BodyInit,
    e2ee: E2eeSession,
    options: HttpE2eeFetchOptions = {},
  ): Promise<Uint8Array[]> {
    const controller = new AbortController();
    const collectFrames = options.collectFrames ?? true;
    let timedOut = false;
    let externallyAborted = false;
    const abortForTimeout = () => {
      timedOut = true;
      controller.abort();
    };
    const abortForCaller = () => {
      externallyAborted = true;
      controller.abort();
    };
    if (options.signal?.aborted) {
      abortForCaller();
    } else {
      options.signal?.addEventListener("abort", abortForCaller, { once: true });
    }
    // 中文注释：HTTP 文件上传/下载是长流式传输，默认不设置整体耗时上限；
    // 短请求由调用方显式传入 timeoutMs，长流则依赖连接背压和断开信号收敛。
    const timer = options.timeoutMs === undefined ? undefined : setTimeout(abortForTimeout, options.timeoutMs);
    let firstFrameTimer =
      options.firstFrameTimeoutMs === undefined ? undefined : setTimeout(abortForTimeout, options.firstFrameTimeoutMs);
    const clearFirstFrameTimer = () => {
      if (firstFrameTimer !== undefined) {
        clearTimeout(firstFrameTimer);
        firstFrameTimer = undefined;
      }
    };
    let sawFirstFrame = false;
    const onFrame = async (frame: Uint8Array) => {
      if (!sawFirstFrame) {
        sawFirstFrame = true;
        clearFirstFrameTimer();
      }
      await options.onFrame?.(frame);
    };
    try {
      const init: RequestInit & { duplex?: "half" } = {
        method,
        headers,
        body,
        signal: controller.signal,
      };
      if (isReadableStreamBody(body)) {
        // 中文注释：只有浏览器 ReadableStream request body 需要 duplex=half；
        // 普通 Blob/ArrayBuffer 上传不要声明流式语义，避免 relay/HTTP1.1 路径被浏览器拒绝。
        init.duplex = "half";
      }
      const response = await fetch(httpUrlFromSocketUrl(this.socketUrl, path), init);
      if (response.status === 404 || response.status === 426) {
        throw new HttpFileTransferUnsupported();
      }
      if (!response.ok) {
        const payload = await decodeHttpE2eeErrorResponse(response, e2ee);
        throw protocolError(payload);
      }
      if (response.body) {
        return await decodeHttpE2eeReadable(e2ee, response.body, onFrame, collectFrames, () => controller.abort());
      }
      if (!collectFrames) {
        // 中文注释：文件下载必须通过 ReadableStream 边解密边写入；没有 response.body 时
        // 退回 arrayBuffer 会把整个文件密文和明文都攒进内存。
        throw new HttpFileTransferUnsupported();
      }
      const frames = decodeHttpE2eeFrames(e2ee, new Uint8Array(await response.arrayBuffer()));
      for (const frame of frames) {
        await onFrame?.(frame);
      }
      return collectFrames ? frames : [];
    } catch (error) {
      if (timedOut && isAbortError(error)) {
        throw new ProtocolClientError("response_timeout", "operation timed out");
      }
      if (externallyAborted && isAbortError(error)) {
        throw abortedConnectionError();
      }
      throw error;
    } finally {
      options.signal?.removeEventListener("abort", abortForCaller);
      if (timer !== undefined) {
        clearTimeout(timer);
      }
      clearFirstFrameTimer();
    }
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
    for (const streamId of [...this.fileStreamsById.keys()]) {
      this.removeFileStream(streamId, error);
    }
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
          this.rejectFileStreams(error);
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
      if (this.fileStreamsById.has(packet.stream_id)) {
        this.removeFileStream(packet.stream_id, error);
        return;
      }
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
    const fileStream = this.fileStreamsById.get(packet.stream_id);
    if (fileStream) {
      this.handleFileStreamChunk(fileStream, packet);
      return;
    }
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

  private handleFileStreamChunk(stream: FileStreamState, packet: ProtocolPacket): void {
    if (stream.kind === "upload") {
      const payload = packet.payload as SessionFileUploadProgressPayload;
      if (payload.session_id !== stream.sessionId) {
        throw new ProtocolClientError("invalid_file_transfer", "file upload progress belongs to another session");
      }
      const waiter = stream.waiters.shift();
      if (waiter) {
        waiter.resolve(payload);
      } else {
        stream.progress.push(payload);
      }
      return;
    }

    const payload = packet.payload as SessionFileTransferChunkPayload;
    if (payload.session_id !== stream.sessionId || !(payload.data_bytes instanceof Uint8Array)) {
      throw new ProtocolClientError("invalid_file_transfer", "file download chunk is invalid");
    }
    const chunk: FileDownloadChunk = {
      session_id: payload.session_id,
      offset_bytes: payload.offset_bytes,
      data_bytes: payload.data_bytes,
      size_bytes: payload.size_bytes,
      eof: payload.eof,
    };
    const waiter = stream.waiters.shift();
    if (waiter) {
      waiter.resolve(chunk);
    } else {
      stream.chunks.push(chunk);
    }
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

  private ensureBinaryFileTransfer(): void {
    if (!this.binaryMode) {
      throw new ProtocolClientError("unsupported_protocol_version", "binary file transfer requires binary protocol support");
    }
  }

  private waitForFileUploadProgress(
    stream: FileUploadStreamState,
    timeoutMs: number,
  ): Promise<SessionFileUploadProgressPayload> {
    const pending = stream.progress.shift();
    if (pending) {
      return Promise.resolve(pending);
    }
    if (stream.closed) {
      return Promise.reject(new ProtocolClientError("connection_closed", "connection closed"));
    }
    return withTimeout(
      new Promise((resolve, reject) => {
        stream.waiters.push({ resolve, reject });
      }),
      timeoutMs,
      "response_timeout",
    );
  }

  private waitForFileDownloadChunk(stream: FileDownloadStreamState, timeoutMs: number): Promise<FileDownloadChunk> {
    const pending = stream.chunks.shift();
    if (pending) {
      return Promise.resolve(pending);
    }
    if (stream.closed) {
      return Promise.reject(new ProtocolClientError("connection_closed", "connection closed"));
    }
    return withTimeout(
      new Promise((resolve, reject) => {
        stream.waiters.push({ resolve, reject });
      }),
      timeoutMs,
      "response_timeout",
    );
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
    if (this.fileStreamsById.has(streamId)) {
      this.removeFileStream(streamId);
      return;
    }
    const stream = this.terminalStreamsById.get(streamId);
    if (!stream) {
      return;
    }
    this.terminalStreamsById.delete(streamId);
    this.terminalStreamsBySession.delete(stream.sessionId);
  }

  private removeFileStream(streamId: PacketStreamId, error?: Error): void {
    const stream = this.fileStreamsById.get(streamId);
    if (!stream) {
      return;
    }
    this.fileStreamsById.delete(streamId);
    stream.closed = true;
    const closeError = error ?? new ProtocolClientError("connection_closed", "connection closed");
    let waiter = stream.waiters.shift();
    while (waiter) {
      waiter.reject(closeError);
      waiter = stream.waiters.shift();
    }
  }

  private rejectFileStreams(error: Error): void {
    for (const streamId of [...this.fileStreamsById.keys()]) {
      this.removeFileStream(streamId, error);
    }
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
    const payload = packet.payload as {
      session_id?: UUID;
      data_base64?: string;
      data_bytes?: Uint8Array;
      kind?: string;
      offset_bytes?: number;
      size_bytes?: number;
      eof?: boolean;
    };
    if (payload.kind) {
      return {
        ...binary,
        payload: { type: "terminal_frame", frame: terminalFrameJsonToBinary(payload) },
      };
    }
    if (
      payload.session_id &&
      typeof payload.offset_bytes === "number" &&
      typeof payload.size_bytes === "number" &&
      typeof payload.eof === "boolean" &&
      (payload.data_bytes instanceof Uint8Array || typeof payload.data_base64 === "string")
    ) {
      const data = payload.data_bytes ?? base64ToBytes(payload.data_base64 ?? "");
      return {
        ...binary,
        payload: {
          type: "file_chunk",
          session_id: payload.session_id,
          offset_bytes: payload.offset_bytes,
          data,
          size_bytes: payload.size_bytes,
          eof: payload.eof,
        },
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
  } else if (packet.payload?.type === "file_chunk") {
    payload = {
      session_id: packet.payload.session_id,
      offset_bytes: packet.payload.offset_bytes,
      data_bytes: packet.payload.data,
      size_bytes: packet.payload.size_bytes,
      eof: packet.payload.eof,
    } satisfies SessionFileTransferChunkPayload;
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

class HttpFileTransferUnsupported extends Error {
  constructor() {
    super("http_file_transfer_unsupported");
  }
}

function isHttpFileTransferUnsupported(error: unknown): boolean {
  return error instanceof HttpFileTransferUnsupported;
}

function isReadableStreamBody(body: BodyInit): boolean {
  return typeof ReadableStream !== "undefined" && body instanceof ReadableStream;
}

function encodeHttpE2eeFrame(e2ee: E2eeSession, plaintext: Uint8Array): Uint8Array {
  const encrypted = encodeBinaryEncryptedFrame(e2ee.encryptBinary(plaintext));
  if (encrypted.byteLength > HTTP_E2EE_MAX_FRAME_BYTES) {
    throw new ProtocolClientError("invalid_file_transfer", "HTTP E2EE frame exceeds transport limit");
  }
  const frame = new Uint8Array(4 + encrypted.byteLength);
  new DataView(frame.buffer, frame.byteOffset, 4).setUint32(0, encrypted.byteLength, false);
  frame.set(encrypted, 4);
  return frame;
}

function encodeHttpE2eeFrames(e2ee: E2eeSession, plaintextFrames: Uint8Array[]): Uint8Array {
  return concatByteChunks(plaintextFrames.map((plaintext) => encodeHttpE2eeFrame(e2ee, plaintext)));
}

function bytesToBlobPart(bytes: Uint8Array): BlobPart {
  // 中文注释：这里的 Uint8Array 都由本地加密/封包代码创建，底层一定是 ArrayBuffer；
  // TypeScript 只能看到 ArrayBufferLike，直接窄化避免为每个上传分片再复制一次内存。
  return bytes as Uint8Array<ArrayBuffer>;
}

function decodeHttpE2eeFrames(e2ee: E2eeSession, wire: Uint8Array): Uint8Array[] {
  const frames: Uint8Array[] = [];
  let offset = 0;
  while (offset < wire.byteLength) {
    if (wire.byteLength - offset < 4) {
      throw new ProtocolClientError("invalid_file_transfer", "invalid HTTP E2EE frame length");
    }
    const len = new DataView(wire.buffer, wire.byteOffset + offset, 4).getUint32(0, false);
    offset += 4;
    if (len === 0 || len > HTTP_E2EE_MAX_FRAME_BYTES || wire.byteLength - offset < len) {
      throw new ProtocolClientError("invalid_file_transfer", "invalid HTTP E2EE frame body");
    }
    const encrypted = decodeBinaryEncryptedFrame(wire.slice(offset, offset + len));
    offset += len;
    frames.push(e2ee.decryptBinary(encrypted));
  }
  return frames;
}

async function decodeHttpE2eeReadable(
  e2ee: E2eeSession,
  body: ReadableStream<Uint8Array>,
  onFrame?: (frame: Uint8Array) => void | Promise<void>,
  collectFrames = true,
  onError?: () => void,
): Promise<Uint8Array[]> {
  const reader = body.getReader();
  const frames: Uint8Array[] = [];
  let pending: Uint8Array<ArrayBufferLike> = new Uint8Array();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (value) {
        let valueOffset = 0;
        while (valueOffset < value.byteLength) {
          const capacity = HTTP_E2EE_MAX_PENDING_BYTES - pending.byteLength;
          if (capacity <= 0) {
            throw new ProtocolClientError("invalid_file_transfer", "invalid HTTP E2EE frame body");
          }
          // 中文注释：底层 ReadableStream chunk 可能合并多个合法帧；分段搬运能在
          // append 前做内存上限保护，同时不误伤合并帧。
          const take = Math.min(capacity, value.byteLength - valueOffset);
          pending = concatByteChunks([pending, value.slice(valueOffset, valueOffset + take)]);
          valueOffset += take;
          while (pending.byteLength >= 4) {
            const len = new DataView(pending.buffer, pending.byteOffset, 4).getUint32(0, false);
            if (len === 0 || len > HTTP_E2EE_MAX_FRAME_BYTES) {
              throw new ProtocolClientError("invalid_file_transfer", "invalid HTTP E2EE frame body");
            }
            if (pending.byteLength < 4 + len) {
              break;
            }
            const encrypted = decodeBinaryEncryptedFrame(pending.slice(4, 4 + len));
            const plaintext = e2ee.decryptBinary(encrypted);
            if (collectFrames) {
              frames.push(plaintext);
            }
            await onFrame?.(plaintext);
            pending = pending.slice(4 + len);
          }
        }
      }
      if (done) {
        if (pending.byteLength !== 0) {
          throw new ProtocolClientError("invalid_file_transfer", "truncated HTTP E2EE frame");
        }
        return frames;
      }
    }
  } catch (error) {
    onError?.();
    try {
      await reader.cancel();
    } catch {
      // 中文注释：原始错误更重要；cancel 只是为了通知 fetch/relay/daemon 停止推流。
    }
    throw error;
  } finally {
    reader.releaseLock();
  }
}

function buildHttpUploadChunkBody(
  e2ee: E2eeSession,
  metaFrame: Uint8Array,
  chunk?: Uint8Array,
): Blob {
  const parts: BlobPart[] = [bytesToBlobPart(encodeHttpE2eeFrame(e2ee, metaFrame))];
  if (chunk) {
    for (let offset = 0; offset < chunk.byteLength; offset += HTTP_UPLOAD_FRAME_PLAINTEXT_BYTES) {
      // 中文注释：业务分片保持 10MiB，密文帧按 1MiB 切开，避免触发 daemon 的
      // HTTP_E2EE_MAX_FRAME_BYTES 防护，同时让后端可边解密边 seek patch 目标文件。
      parts.push(bytesToBlobPart(encodeHttpE2eeFrame(
        e2ee,
        chunk.slice(offset, Math.min(chunk.byteLength, offset + HTTP_UPLOAD_FRAME_PLAINTEXT_BYTES)),
      )));
    }
  }
  return new Blob(parts, { type: "application/octet-stream" });
}

function parseHttpJsonFrame<T>(frame: Uint8Array | undefined): T {
  if (!frame) {
    throw new ProtocolClientError("invalid_file_transfer", "missing HTTP E2EE JSON frame");
  }
  return JSON.parse(decodeUtf8(frame)) as T;
}

async function decodeHttpE2eeErrorResponse(response: Response, e2ee: E2eeSession): Promise<ErrorPayload> {
  const fallback: ErrorPayload = {
    code: "http_file_transfer_failed",
    message: "HTTP file transfer failed",
  };
  let body: Uint8Array;
  try {
    body = new Uint8Array(await response.arrayBuffer());
  } catch {
    return fallback;
  }

  // 中文注释：post-auth HTTP 文件错误由 daemon 放在 E2EE frame 里返回；relay 仍只看密文。
  try {
    const frames = decodeHttpE2eeFrames(e2ee, body);
    const payload = parseHttpJsonFrame<ErrorPayload>(frames[0]);
    if (isErrorPayload(payload)) {
      return payload;
    }
  } catch {
    // 兼容未进入 E2EE 的明文 HTTP 错误，例如反代或旧服务返回的 JSON。
  }

  try {
    const payload = JSON.parse(decodeUtf8(body)) as unknown;
    if (isErrorPayload(payload)) {
      return payload;
    }
  } catch {
    // Keep the stable generic error when the response is not a protocol JSON error.
  }
  return fallback;
}

function isErrorPayload(payload: unknown): payload is ErrorPayload {
  return (
    typeof payload === "object" &&
    payload !== null &&
    typeof (payload as ErrorPayload).code === "string" &&
    typeof (payload as ErrorPayload).message === "string"
  );
}

function httpUrlFromSocketUrl(socketUrl: string, path: string): string {
  const url = new URL(socketUrl);
  url.protocol = url.protocol === "wss:" ? "https:" : "http:";
  const socketPath = url.pathname.replace(/\/+$/, "");
  const prefix = socketPath.endsWith("/ws") ? socketPath.slice(0, -"/ws".length) : socketPath;
  const apiPath = path.startsWith("/") ? path : `/${path}`;
  // 中文注释：relay/daemon 可能部署在 `/termd/ws` 这类子路径下；HTTP API 要复用
  // 同一个前缀和 query，否则会绕到站点根路径或丢失 relay token。
  url.pathname = `${prefix}${apiPath}` || "/";
  url.hash = "";
  return url.toString();
}

function fileNameFromPath(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() || "download";
}

function bodyToArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

async function blobSliceBytes(blob: Blob, start: number, end: number): Promise<Uint8Array> {
  const sliced = blob.slice(start, end);
  return readBlobBytes(sliced);
}

async function readBlobBytes(blob: Blob): Promise<Uint8Array> {
  if (typeof blob.arrayBuffer === "function") {
    return new Uint8Array(await blob.arrayBuffer());
  }
  if (typeof FileReader !== "undefined") {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error ?? new Error("failed to read blob"));
      reader.onload = () => {
        const result = reader.result;
        if (result instanceof ArrayBuffer) {
          resolve(new Uint8Array(result));
          return;
        }
        reject(new Error("failed to read blob as bytes"));
      };
      // jsdom 的 File/Blob 缺少 arrayBuffer；FileReader 路径让测试和旧浏览器都能读出原始字节。
      reader.readAsArrayBuffer(blob);
    });
  }
  throw new Error("blob byte reading is not supported in this environment");
}

interface OpenWebSocketOptions {
  timeoutMs: number;
  hedgeDelayMs?: number;
  webSocketFactory?: (url: string) => WebSocket;
  signal?: AbortSignal;
}

function openWebSocket(url: string, options: OpenWebSocketOptions): Promise<WebSocket> {
  const maxSockets = options.hedgeDelayMs && options.hedgeDelayMs > 0 ? 2 : 1;
  const sockets: WebSocket[] = [];
  const timers = new Set<ReturnType<typeof setTimeout>>();
  let settled = false;
  let started = 0;
  let active = 0;
  let lastError: Error = new ProtocolClientError("connect_timeout", "operation timed out");

  const closeSocket = (socket: WebSocket) => {
    try {
      socket.close();
    } catch {
      // 浏览器 WebSocket close 本身不应影响连接重试路径。
    }
  };
  const closeLosers = (winner?: WebSocket) => {
    for (const socket of sockets) {
      if (socket !== winner) {
        closeSocket(socket);
      }
    }
  };
  const clearTimers = () => {
    for (const timer of timers) {
      clearTimeout(timer);
    }
    timers.clear();
  };

  return new Promise((resolve, reject) => {
    const finishReject = (error: Error) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimers();
      closeLosers();
      options.signal?.removeEventListener("abort", abort);
      reject(error);
    };
    const finishResolve = (socket: WebSocket) => {
      if (settled) {
        closeSocket(socket);
        return;
      }
      settled = true;
      clearTimers();
      closeLosers(socket);
      options.signal?.removeEventListener("abort", abort);
      resolve(socket);
    };
    const maybeStartAnother = () => {
      if (!settled && started < maxSockets) {
        startSocket();
        return true;
      }
      return false;
    };
    const maybeReject = () => {
      if (!settled && active === 0 && started >= maxSockets) {
        finishReject(lastError);
      }
    };
    const startSocket = () => {
      started += 1;
      active += 1;
      const candidate = options.webSocketFactory?.(url) ?? new WebSocket(url);
      candidate.binaryType = "arraybuffer";
      sockets.push(candidate);

      // 中文注释：公网 relay 偶发卡在 TCP/TLS/WebSocket open 阶段。hedge 会在首条
      // 连接迟迟不 open 时并行开第二条，谁先 open 用谁，避免等待坏握手完整超时。
      waitForOpen(candidate, options.timeoutMs).then(
        () => finishResolve(candidate),
        (error) => {
          active -= 1;
          lastError = error instanceof Error ? error : new ProtocolClientError("connect_timeout", "operation timed out");
          if (!maybeStartAnother()) {
            maybeReject();
          }
        },
      );
    };
    const abort = () => finishReject(abortedConnectionError());

    if (options.signal?.aborted) {
      finishReject(abortedConnectionError());
      return;
    }
    options.signal?.addEventListener("abort", abort, { once: true });
    startSocket();
    if (maxSockets > 1) {
      const timer = setTimeout(() => {
        timers.delete(timer);
        maybeStartAnother();
      }, options.hedgeDelayMs);
      timers.add(timer);
    }
  });
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

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
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
