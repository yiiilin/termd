import type { AddressInfo } from "node:net";
import { WebSocketServer, type WebSocket } from "ws";
import {
  authSigningInputBytes,
  decodeEd25519PublicKey,
  verifyEd25519Signature,
} from "../protocol/auth";
import { E2eeSession, generateE2eeKeyPair, type E2eeKeyPair } from "../protocol/e2ee";
import type {
  DaemonClientSummaryPayload,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  PairRequestPayload,
  SessionCreatePayload,
  SessionCreatedPayload,
  SessionCursorPayload,
  SessionDataPayload,
  SessionFileReadResultPayload,
  SessionFileWrittenPayload,
  SessionFilesResultPayload,
  SessionSummaryPayload,
  TerminalSize,
  UUID,
} from "../protocol/types";
import {
  decodeUtf8,
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
  pairFailure?: ErrorPayload;
  sessionDataError?: ErrorPayload;
  daemonClients?: DaemonClientSummaryPayload[];
  sessionFiles?: Record<UUID, SessionFilesResultPayload>;
  sessionFileReads?: Record<string, SessionFileReadResultPayload>;
}

interface TrustedDevice {
  deviceId: UUID;
  devicePublicKey: string;
}

interface MockConnection {
  socket: WebSocket;
  deviceId?: UUID;
  e2ee?: E2eeSession;
}

export class MockDaemon {
  public readonly serverId: UUID;
  public readonly daemonPublicKey = "ed25519-v1:daemon-public";
  public readonly outerWireLog: string[] = [];
  public readonly createdCommands: string[][] = [];
  public readonly sessionDataMessages: string[] = [];
  public readonly attachedSessions: UUID[] = [];
  public readonly sessionCursorUpdates: SessionCursorPayload[] = [];
  public readonly sessionResizes: Array<{ session_id: UUID; size: TerminalSize }> = [];
  public readonly sessionRenames: Array<{ session_id: UUID; name: string }> = [];
  public readonly closedSessions: UUID[] = [];
  public readonly sessionFileRequests: Array<{ session_id: UUID; path?: string | null }> = [];
  public readonly sessionFileReadRequests: Array<{ session_id: UUID; path: string }> = [];
  public readonly sessionFileWrites: Array<{ session_id: UUID; path: string; text: string }> = [];
  public readonly sessionFileDeletes: Array<{ session_id: UUID; path: string }> = [];
  public pingMessages = 0;
  public readonly decryptedInputs: string[] = [];
  public nextAttachRole = "operator" as const;
  private createdSessionCounter = 0;
  private readonly e2eeKeypair: E2eeKeyPair;
  private readonly trustedDevices = new Map<UUID, TrustedDevice>();

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
    server.on("connection", (socket) => daemon.accept(socket));
    return daemon;
  }

  get url(): string {
    return this.urlValue;
  }

  outerWireText(): string {
    return this.outerWireLog.join("\n");
  }

  async stop(): Promise<void> {
    this.server.clients.forEach((client) => client.close());
    await new Promise<void>((resolve, reject) => {
      this.server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  private accept(socket: WebSocket): void {
    const connection: MockConnection = { socket };
    this.sendOuter(
      socket,
      envelope("hello", {
        protocol_version: 1,
        nonce: nonce(),
        timestamp_ms: nowMs(),
        server_id: this.serverId,
        device_id: null,
      }),
    );
    this.sendOuter(
      socket,
      envelope("e2ee_key_exchange", {
        server_id: this.serverId,
        device_id: randomUuid(),
        public_key: this.e2eeKeypair.publicKeyWire,
        nonce: nonce(),
        timestamp_ms: nowMs(),
      }),
    );

    socket.on("message", (raw) => {
      void this.handleOuter(connection, raw.toString());
    });
  }

  private async handleOuter(connection: MockConnection, raw: string): Promise<void> {
    this.outerWireLog.push(raw);
    const outer = parseEnvelope(raw);

    if (outer.type === "e2ee_key_exchange") {
      const payload = outer.payload as E2eeKeyExchangePayload;
      connection.deviceId = payload.device_id;
      connection.e2ee = E2eeSession.daemon({
        serverId: this.serverId,
        deviceId: payload.device_id,
        localKeypair: this.e2eeKeypair,
        devicePublicKeyWire: payload.public_key,
      });

      if (this.trustedDevices.has(payload.device_id)) {
        this.sendInner(
          connection,
          envelope("auth_challenge", {
            device_id: payload.device_id,
            challenge: `challenge-${payload.device_id}`,
            expires_at_ms: nowMs() + 60_000,
          }),
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

  private async handleInner(connection: MockConnection, inner: Envelope): Promise<void> {
    switch (inner.type) {
      case "pair_request":
        this.handlePairRequest(connection, inner.payload as PairRequestPayload);
        return;
      case "auth":
        await this.handleAuth(connection, inner.payload as Record<string, unknown>);
        return;
      case "session_list":
        this.sendInner(connection, envelope("session_list_result", { sessions: this.options.sessions }));
        return;
      case "daemon_clients": {
        this.sendInner(
          connection,
          envelope("daemon_clients_result", {
            clients: this.options.daemonClients ?? [],
          }),
        );
        return;
      }
      case "session_create":
        this.handleSessionCreate(connection, inner.payload as SessionCreatePayload);
        return;
      case "session_attach": {
        const payload = inner.payload as { session_id: UUID };
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        this.attachedSessions.push(payload.session_id);
        this.sendInner(
          connection,
          envelope("session_attached", {
            session_id: payload.session_id,
            role: this.nextAttachRole,
            state: session?.state ?? "running",
            size: session?.size ?? { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          }),
        );
        if (this.options.attachOutput) {
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
        const input = decodeUtf8(sessionDataFromBase64(payload.data_base64));
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
        this.sessionResizes.push(inner.payload as { session_id: UUID; size: TerminalSize });
        return;
      }
      case "session_rename": {
        const payload = inner.payload as { session_id: UUID; name: string };
        this.sessionRenames.push(payload);
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
        if (session) {
          session.name = payload.name;
        }
        this.sendInner(connection, envelope("session_renamed", payload));
        return;
      }
      case "session_close": {
        const payload = inner.payload as { session_id: UUID };
        this.closedSessions.push(payload.session_id);
        this.options.sessions = this.options.sessions.filter((session) => session.session_id !== payload.session_id);
        this.sendInner(connection, envelope("session_closed", payload));
        return;
      }
      case "session_files": {
        const payload = inner.payload as { session_id: UUID; path?: string | null };
        this.sessionFileRequests.push(payload);
        // 指定 path 时必须按该目录返回，避免测试里把“任意切换目录”误回退成 session 根目录。
        const files =
          payload.path && payload.path.trim()
            ? this.options.sessionFiles?.[payload.path]
            : this.options.sessionFiles?.[payload.session_id];
        this.sendInner(
          connection,
          envelope(
            "session_files_result",
            files ?? {
              session_id: payload.session_id,
              path: payload.path ?? "",
              entries: [],
            },
          ),
        );
        return;
      }
      case "session_file_read": {
        const payload = inner.payload as { session_id: UUID; path: string };
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
      case "session_file_write": {
        const payload = inner.payload as { session_id: UUID; path: string; data_base64: string };
        const bytes = sessionDataFromBase64(payload.data_base64);
        this.sessionFileWrites.push({
          session_id: payload.session_id,
          path: payload.path,
          text: decodeUtf8(bytes),
        });
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
        this.sessionFileDeletes.push(payload);
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
    const created = {
      session_id: sessionId,
      role: this.nextAttachRole,
      state: "running",
      size: payload.size,
    } satisfies SessionCreatedPayload;

    // mock daemon 模拟真实 daemon：session_create 会立刻 attach 当前连接。
    this.options.sessions.unshift({
      session_id: created.session_id,
      state: created.state,
      size: created.size,
    });
    this.sendInner(connection, envelope("session_created", created));
    if (this.options.attachOutput) {
      this.sendInner(
        connection,
        envelope("session_data", {
          session_id: created.session_id,
          data_base64: sessionDataToBase64(new TextEncoder().encode(this.options.attachOutput)),
        }),
      );
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
        url: this.url,
        paired_at_ms: nowMs(),
      }),
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
    this.sendOuter(connection.socket, envelope("encrypted_frame", connection.e2ee.encryptJson(inner)));
  }

  private sendError(connection: MockConnection, code: string, message: string): void {
    const error = envelope("error", { code, message } satisfies ErrorPayload);
    if (connection.e2ee) {
      this.sendInner(connection, error);
      return;
    }
    this.sendOuter(connection.socket, error);
  }

  private sendOuter(socket: WebSocket, outer: Envelope): void {
    socket.send(JSON.stringify(outer));
  }
}
