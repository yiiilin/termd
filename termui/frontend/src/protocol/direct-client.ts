import {
  authPayloadForChallenge,
  signAuthPayload,
  signHttpE2eeAuthPayload,
} from "./auth";
import {
  E2eeSession,
  decodeBinaryEncryptedFrame,
  encodeBinaryEncryptedFrame,
  generateE2eeKeyPair,
} from "./e2ee";
import {
  decodeBinaryProtocolPacket,
  encodeBinaryProtocolPacket,
} from "./binary-packet";
import { type DirectClientInbox, performDirectHandshake } from "./direct-handshake";
import { ProtocolClientError, protocolError } from "./errors";
import {
  HttpFileTransferUnsupported,
  type HttpE2eeFetchOptions,
  blobSliceBytes,
  bodyToArrayBuffer,
  buildHttpUploadChunkBody,
  concatByteChunks,
  decodeHttpE2eeErrorResponse,
  decodeHttpE2eeFrames,
  decodeHttpE2eeReadable,
  encodeHttpE2eeFrames,
  fileNameFromPath,
  httpUrlFromSocketUrl,
  isHttpFileTransferUnsupported,
  isReadableStreamBody,
  parseHttpJsonFrame,
  readBlobBytes,
} from "./http-e2ee";
import { envelopeTypeForProtocolEventMethod } from "./methods";
import { binaryPacketToProtocol, protocolPacketToBinary } from "./packet-codec";
import {
  abortedConnectionError,
  expectQueuedEnvelope,
  isAbortError,
  messageDataToBytes,
  queuedMessageBytes,
  sendOuterMessage,
  throwIfAborted,
  withAbort,
  type QueuedMessage,
  withTimeout,
  yieldToEventLoop,
} from "./socket-transport";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "./types";
import { recordTermdDiagnostic } from "../diagnostics";
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

type DirectClientPhase = "connecting" | "e2ee_ready" | "authenticated" | "terminal_stream_open" | "closed";

const E2EE_READY_PACKET_METHODS = new Set(["pair.request", "auth", "auth.verify"]);

interface TerminalStreamState {
  sessionId: UUID;
  streamId: PacketStreamId;
  open: boolean;
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
// 中文注释：正常上传仍依赖 HTTP 连接背压；这个宽限只处理连接半开但 fetch 永不返回的故障态。
// 10MiB/10min 约等于 17KiB/s，已经覆盖很差的实际网络。
const HTTP_UPLOAD_CHUNK_TIMEOUT_MS = 10 * 60 * 1000;
const HTTP_UPLOAD_ABORT_TIMEOUT_MS = 5000;
const FILE_TRANSFER_MEMORY_FALLBACK_MAX_BYTES = 16 * 1024 * 1024;
const FILE_TRANSFER_WEBSOCKET_COMPAT_MAX_BYTES = 16 * 1024 * 1024;
// 中文注释：RPC file_write/file_read 只服务浏览器内置文本编辑器和很小的兼容上传；
// 大文件必须走 HTTP E2EE 或 binary stream，不能再整包 base64 进入 RPC。
const SESSION_FILE_RPC_MAX_BYTES = 1024 * 1024;

export { ProtocolClientError };

export class DirectClient {
  private readonly timeoutMs: number;
  private readonly authTimeoutMs: number;
  private e2ee: E2eeSession;
  private closed = false;
  private closedError: Error | undefined;
  private phase: DirectClientPhase = "connecting";
  private receivePumpStarted = false;
  private e2eeTranscriptSha256?: string;
  private readonly pendingRequests = new Map<UUID, PendingRequest>();
  private readonly pendingInner: Envelope[] = [];
  private readonly innerWaiters: QueuedInnerWaiter[] = [];
  private readonly terminalStreamsBySession = new Map<UUID, TerminalStreamState>();
  private readonly terminalStreamsById = new Map<PacketStreamId, TerminalStreamState>();
  private readonly pendingTerminalStreamIds = new Set<PacketStreamId>();
  private readonly pendingTerminalStreamLastOutputSeq = new Map<PacketStreamId, number>();
  private readonly fileStreamsById = new Map<PacketStreamId, FileStreamState>();
  private authenticatedDevice?: DeviceState;
  private authenticatedServer?: PairedServerState;

  private constructor(
    private readonly socket: WebSocket,
    private readonly inbox: DirectClientInbox,
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
    this.phase = "e2ee_ready";
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
    const handshake = await performDirectHandshake(url, routeServerId, deviceId, {
      timeoutMs,
      socketOpenTimeoutMs,
      socketOpenHedgeDelayMs: options.socketOpenHedgeDelayMs,
      expectedDaemonPublicKey: options.expectedDaemonPublicKey,
      webSocketFactory: options.webSocketFactory,
      signal: options.signal,
      createInbox: (socket) => new SocketInbox(socket),
    });
    const client = new DirectClient(
      handshake.socket,
      handshake.inbox,
      url,
      routeServerId,
      deviceId,
      handshake.daemonE2eePublicKeyWire,
      handshake.e2ee,
      { timeoutMs, requestTimeoutMs },
      handshake.binaryMode,
    );
    client.e2eeTranscriptSha256 = handshake.e2eeTranscriptSha256;
    client.startReceivePump();
    return client;
  }

  get serverId(): UUID {
    return this.serverIdValue;
  }

  get isClosed(): boolean {
    return this.closed || this.socket.readyState === WebSocket.CLOSING || this.socket.readyState === WebSocket.CLOSED;
  }

  async pair(token: string, devicePublicKey: PublicKeyWire): Promise<PairAcceptPayload> {
    this.requireE2eeReady();
    const accepted = await this.request<PairAcceptPayload>("pair.request", {
      device_id: this.deviceId,
      device_public_key: devicePublicKey,
      token,
      nonce: nonce(),
      timestamp_ms: nowMs(),
    } satisfies PairRequestPayload);
    this.phase = "authenticated";
    return accepted;
  }

  async authenticate(device: DeviceState, server: PairedServerState): Promise<void> {
    this.requireE2eeReady();
    const challenge = await this.expectQueuedPayload<AuthChallengePayload>("auth_challenge", this.authTimeoutMs);
    const auth = await signAuthPayload(
      authPayloadForChallenge(device.device_id, challenge.challenge),
      server,
      device.device_signing_key_secret,
      this.e2eeTranscriptSha256,
    );
    await this.request("auth.verify", auth, this.authTimeoutMs);
    this.phase = "authenticated";
    await this.request("client.hello", { name: device.name?.trim() || "Web client" } satisfies ClientHelloPayload, this.authTimeoutMs);
    this.authenticatedDevice = device;
    this.authenticatedServer = server;
  }

  async listSessions(timeoutMs = this.timeoutMs): Promise<SessionListResultPayload> {
    // 中文注释：session.list 在 relay bootstrap/recovery 路径里经常和 route/E2EE/auth、
    // daemon mux/data pipe 配对同处一条用户可见关键路径。调用方可按场景显式放宽预算，
    // 但普通短请求默认值仍保持不变。
    return this.request<SessionListResultPayload>("session.list", {}, timeoutMs);
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
    this.requireAuthenticated();
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
        // 中文注释：只有 404/426/501 这类“老 daemon/old relay 明确不支持 HTTP 文件端点”
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
        // 中文注释：WebSocket 兼容上传没有浏览器原生 upload progress；
        // 分片写入 socket 队列后先回报发送进度，daemon 提交 ack 再回报 confirmed/committing 进度。
        options.onSentProgress?.(end, file.size);
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
    this.requireAuthenticated();
    if (this.authenticatedDevice && this.authenticatedServer) {
      // 中文注释：认证后的下载优先保证 HTTP E2EE 流式路径；端点不支持时不退回主 WebSocket，
      // 避免大文件下载重新占用终端控制连接或在浏览器内存中整包缓冲。
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
      if (response.status === 404 || response.status === 426 || response.status === 501) {
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
    this.requireAuthenticated();
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
    options: { watchUpdates?: boolean; lastTerminalSeq?: number; timeoutMs?: number; signal?: AbortSignal } = {},
  ): Promise<SessionAttachedPayload> {
    this.requireAuthenticated();
    return this.openTerminalStream<SessionAttachedPayload>(
      "terminal.attach",
      {
        session_id: sessionId,
        watch_updates: options.watchUpdates ?? true,
        ...(options.lastTerminalSeq !== undefined ? { last_terminal_seq: options.lastTerminalSeq } : {}),
      } satisfies SessionAttachPayload,
      sessionId,
      options.timeoutMs,
      options.signal,
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
    const stream = this.requireTerminalStream(sessionId);
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
    this.requireTerminalStream(sessionId);
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
    this.requireTerminalStream(sessionId);
    return this.request<SessionResizedPayload>("session.resize", { session_id: sessionId, size } satisfies SessionResizePayload);
  }

  async requestSessionResize(sessionId: UUID, size: TerminalSize): Promise<void> {
    this.requireTerminalStream(sessionId);
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

  async request<T = unknown>(
    method: string,
    payload: unknown,
    timeoutMs = this.timeoutMs,
    signal?: AbortSignal,
  ): Promise<T> {
    if (E2EE_READY_PACKET_METHODS.has(method)) {
      this.requireE2eeReady();
    } else {
      this.requireAuthenticated();
    }
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
      signal,
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
    if (this.closed || this.phase === "closed") {
      // 中文注释：receive pump 可能在 UI 消费 backlog 期间已经关闭连接；
      // 已排队输出必须先按 FIFO 交付，队列清空后不能再创建永远不会被唤醒的 waiter。
      throw this.connectionClosedError();
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
    this.closedError ??= error;
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
    this.phase = "closed";
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
          // 中文注释：测试需要在 receive pump 真正让出事件循环的瞬间注入 close/error，
          // 这样可以稳定覆盖“已有 backlog 继续排空，但新的 RPC 立刻失败”的分支。
          __directClientTestInternals.onReceivePumpYield?.();
          await yieldToEventLoop();
        }
      } catch (caught) {
        if (!this.closed) {
          const error = caught instanceof Error ? caught : new ProtocolClientError("protocol_error", "protocol operation failed");
          // receive pump 已经证明这条 WebSocket 不再可信；标记关闭并主动 close，
          // 避免上层继续复用一个不会再消费入站消息的 DirectClient。
          this.closedError ??= error;
          this.closed = true;
          this.phase = "closed";
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
      return;
    }
    this.enqueueInner(envelope("error", { code: error.code, message: error.message } satisfies ErrorPayload));
  }

  private enqueuePacketEvent(packet: ProtocolPacket): void {
    const envelopeType = envelopeTypeForProtocolEventMethod(packet.method);
    if (!envelopeType) {
      return;
    }
    this.enqueueInner(envelope(envelopeType, packet.payload));
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
      if (this.pendingTerminalStreamIds.has(packet.stream_id)) {
        // 中文注释：terminal.create 的 session_id 要等 response 才知道，但 daemon 可能已经
        // 按同一个 stream_id 推了首屏 snapshot/output。这里按 stream_id 暂存，response 绑定
        // session 后 receive loop 会按 payload.session_id 交给当前 Ghostty。
        this.pendingTerminalStreamLastOutputSeq.set(packet.stream_id, seq);
        this.enqueueTerminalStreamPayload(packet, seq);
        return;
      }
      // 中文注释：用户快速切换 session 后，旧 stream 的少量输出可能已经在
      // WebSocket/TCP 队列里。stream 已取消时这些 chunk 必须在协议层丢弃，
      // 否则会继续堆进 pendingInner，把新 session 的 snapshot/tail 挡在后面。
      return;
    }
    stream.lastOutputSeq = seq;
    this.enqueueTerminalStreamPayload(packet, seq);
  }

  private enqueueTerminalStreamPayload(packet: ProtocolPacket, seq: number): void {
    if (!packet.stream_id) {
      throw new ProtocolClientError("invalid_packet", "stream chunk is missing stream id");
    }
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
    if (payload.kind === "snapshot") {
      recordTermdDiagnostic("direct_client_enqueue_snapshot", {
        sessionId: payload.session_id,
        streamId,
        transportSeq,
        baseSeq: payload.base_seq,
      });
    } else if (payload.kind === "output" && payload.terminal_seq % 1024 === 1) {
      recordTermdDiagnostic("direct_client_enqueue_output_sample", {
        sessionId: payload.session_id,
        streamId,
        transportSeq,
        terminalSeq: payload.terminal_seq,
      });
    }
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
    signal?: AbortSignal,
  ): Promise<T> {
    const id = randomUuid();
    const streamId = randomUuid();
    let provisionalStream: TerminalStreamState | undefined;
    if (sessionId) {
      provisionalStream = { sessionId, streamId, open: false, nextInputSeq: 1, lastOutputSeq: 0 };
      this.terminalStreamsBySession.set(sessionId, provisionalStream);
      this.terminalStreamsById.set(streamId, provisionalStream);
    } else {
      this.pendingTerminalStreamIds.add(streamId);
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
      signal,
    ).then(
      (response) => {
        const resolvedSessionId = response.session_id;
        const lastOutputSeq = this.pendingTerminalStreamLastOutputSeq.get(streamId) ?? 0;
        this.pendingTerminalStreamIds.delete(streamId);
        this.pendingTerminalStreamLastOutputSeq.delete(streamId);
        if (provisionalStream && provisionalStream.sessionId !== resolvedSessionId) {
          this.terminalStreamsBySession.delete(provisionalStream.sessionId);
        }
        const stream = provisionalStream ?? { sessionId: resolvedSessionId, streamId, open: false, nextInputSeq: 1, lastOutputSeq };
        stream.sessionId = resolvedSessionId;
        stream.open = true;
        stream.lastOutputSeq = Math.max(stream.lastOutputSeq, lastOutputSeq);
        this.terminalStreamsBySession.set(resolvedSessionId, stream);
        this.terminalStreamsById.set(streamId, stream);
        this.phase = "terminal_stream_open";
        return response;
      },
      (error) => {
        this.pendingTerminalStreamIds.delete(streamId);
        this.pendingTerminalStreamLastOutputSeq.delete(streamId);
        this.discardQueuedTerminalOutputByStream(streamId);
        this.removeStream(streamId);
        throw error;
      },
    );
  }

  private requireOpen(): void {
    if (this.closed || this.phase === "closed") {
      throw this.connectionClosedError();
    }
  }

  private requireE2eeReady(): void {
    this.requireOpen();
    if (this.phase !== "e2ee_ready") {
      throw new ProtocolClientError("invalid_state", "client is not ready for pairing or authentication");
    }
  }

  private requireAuthenticated(): void {
    this.requireOpen();
    if (this.phase !== "authenticated" && this.phase !== "terminal_stream_open") {
      throw new ProtocolClientError("invalid_state", "client is not authenticated");
    }
  }

  private requireTerminalStream(sessionId: UUID): TerminalStreamState {
    this.requireAuthenticated();
    const stream = this.terminalStreamsBySession.get(sessionId);
    if (!stream?.open) {
      throw new ProtocolClientError("invalid_state", "terminal stream is not attached");
    }
    return stream;
  }

  private refreshTerminalStreamPhase(): void {
    if (this.closed || this.phase === "closed") {
      return;
    }
    if ([...this.terminalStreamsById.values()].some((stream) => stream.open)) {
      this.phase = "terminal_stream_open";
      return;
    }
    if (this.phase === "terminal_stream_open") {
      this.phase = "authenticated";
    }
  }

  private sendTrackedPacket<T>(
    packet: ProtocolPacket,
    id: UUID,
    method: string,
    timeoutMs = this.timeoutMs,
    signal?: AbortSignal,
  ): Promise<T> {
    if (this.closed) {
      return Promise.reject(this.connectionClosedError());
    }
    throwIfAborted(signal);
    return withAbort(new Promise<T>((resolve, reject) => {
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
    }), signal).catch((error) => {
      if (signal?.aborted) {
        this.rejectTrackedRequest(id, abortedConnectionError());
        const streamId = packet.kind === "stream_open" ? packet.stream_id : undefined;
        if (streamId) {
          this.sendPacketBestEffort({
            version: PROTOCOL_PACKET_VERSION,
            kind: "cancel",
            stream_id: streamId,
            payload: { reason: "request_aborted" },
          });
          this.discardQueuedTerminalOutputByStream(streamId);
          this.removeStream(streamId);
        }
      }
      throw error;
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

  private discardQueuedTerminalOutputByStream(streamId: PacketStreamId): void {
    if (this.pendingInner.length === 0) {
      return;
    }
    const retained = this.pendingInner.filter((inner) => {
      if (inner.type !== "session_data" && inner.type !== "terminal_frame") {
        return true;
      }
      const payload = inner.payload as { stream_id?: unknown };
      return payload.stream_id !== streamId;
    });
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
    this.refreshTerminalStreamPhase();
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

  private connectionClosedError(): Error {
    return this.closedError ?? new ProtocolClientError("connection_closed", "connection closed");
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
  private closedError: Error | undefined;

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
    if (this.closedError) {
      return this.rejectedRead(this.closedError);
    }
    if (this.socket.readyState === WebSocket.CLOSING || this.socket.readyState === WebSocket.CLOSED) {
      // 中文注释：极端事件顺序下 close 事件可能还没派发，但 readyState 已关闭；
      // read 不能继续挂起，否则上层 receive pump 无法进入重连路径。
      this.closedError = new ProtocolClientError("connection_closed", "connection closed");
      return this.rejectedRead(this.closedError);
    }
    const pending = new Promise<QueuedMessage>((resolve, reject) => {
      this.waiters.push(resolve);
      this.errors.push(reject);
    });
    // 中文注释：SocketInbox 是 DirectClient 内部队列，WebSocket close/error 会从事件回调
    // 异步拒绝这个 promise；先挂一个空 catch 只用于标记“拒绝已被观察”，避免
    // Node/Vitest 在上层 receive pump 接管前把它判定为未处理拒绝。原 promise 仍保持
    // rejected 状态，调用方的 await / expect(...).rejects 能照常拿到同一个错误。
    void pending.catch(() => {});
    return pending;
  }

  rejectPending(error: Error): void {
    // 中文注释：close/error 可能发生在 receive pump 处理积压消息并让出事件循环期间；
    // 此时没有 read waiter，必须记住错误，等队列清空后让下一次 read 失败触发上层重连。
    this.closedError ??= error;
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

  private rejectedRead(error: Error): Promise<QueuedMessage> {
    const pending = Promise.reject<QueuedMessage>(error);
    // 中文注释：和 pending read 一样，立即失败的 read 也先标记为已观察；
    // 这不吞错误，只避免内部状态检查路径产生测试环境里的未处理拒绝噪声。
    void pending.catch(() => {});
    return pending;
  }
}

// 中文注释：只给 Vitest 覆盖传输层边界条件使用；业务代码不应依赖这些内部类型。
export const __directClientTestInternals = {
  SocketInbox,
  onReceivePumpYield: undefined as undefined | (() => void),
};
