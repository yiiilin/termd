import type { AddressInfo } from "node:net";
import { WebSocketServer, type WebSocket } from "ws";
import {
  authSigningInputBytes,
  decodeEd25519PublicKey,
  verifyEd25519Signature,
} from "../protocol/auth";
import { E2eeSession, generateE2eeKeyPair, type E2eeKeyPair } from "../protocol/e2ee";
import type {
  AttachRole,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  PairRequestPayload,
  SessionDataPayload,
  SessionSummaryPayload,
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
  sessions: SessionSummaryPayload[];
  attachOutput?: string;
  pairFailure?: ErrorPayload;
  sessionDataError?: ErrorPayload;
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
  public readonly sessionDataMessages: string[] = [];
  public readonly decryptedInputs: string[] = [];
  public nextAttachRole: AttachRole = "controller";
  private readonly e2eeKeypair: E2eeKeyPair;
  private readonly trustedDevices = new Map<UUID, TrustedDevice>();

  private constructor(
    private readonly server: WebSocketServer,
    private readonly options: MockDaemonOptions,
  ) {
    this.serverId = randomUuid();
    this.e2eeKeypair = generateE2eeKeyPair();
  }

  static async start(options: MockDaemonOptions): Promise<MockDaemon> {
    const server = new WebSocketServer({ port: 0, host: "127.0.0.1" });
    await new Promise<void>((resolve) => server.once("listening", resolve));
    const daemon = new MockDaemon(server, options);
    server.on("connection", (socket) => daemon.accept(socket));
    return daemon;
  }

  get url(): string {
    const address = this.server.address() as AddressInfo;
    return `ws://127.0.0.1:${address.port}/ws`;
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
      case "session_attach": {
        const payload = inner.payload as { session_id: UUID };
        const session = this.options.sessions.find((candidate) => candidate.session_id === payload.session_id);
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
          // controller_required 等拒绝路径只记录收到的加密业务帧，不模拟写入 PTY。
          this.sendError(connection, this.options.sessionDataError.code, this.options.sessionDataError.message);
          return;
        }
        this.decryptedInputs.push(input);
        return;
      }
      case "session_resize":
        return;
      case "control_request": {
        const payload = inner.payload as { session_id: UUID; device_id: UUID };
        this.nextAttachRole = "controller";
        this.sendInner(connection, envelope("control_grant", payload));
        return;
      }
      case "ping": {
        const payload = inner.payload as { nonce: string };
        this.sendInner(connection, envelope("pong", { nonce: payload.nonce, timestamp_ms: nowMs() }));
        return;
      }
      default:
        this.sendError(connection, "invalid_state", "invalid protocol state");
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
