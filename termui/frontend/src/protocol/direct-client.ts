import { authPayloadForChallenge, signAuthPayload } from "./auth";
import { E2eeSession, generateE2eeKeyPair } from "./e2ee";
import { ProtocolClientError, protocolError } from "./errors";
import type {
  AuthChallengePayload,
  ControlGrantPayload,
  DeviceState,
  E2eeKeyExchangePayload,
  EncryptedFramePayload,
  Envelope,
  ErrorPayload,
  HelloPayload,
  PairAcceptPayload,
  PairedServerState,
  PublicKeyWire,
  SessionClosePayload,
  SessionClosedPayload,
  SessionAttachIntent,
  SessionAttachedPayload,
  DaemonClientsResultPayload,
  SessionCreatePayload,
  SessionCreatedPayload,
  SessionDataPayload,
  SessionListResultPayload,
  SessionRenamedPayload,
  SessionRenamePayload,
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

  static async connect(url: string, deviceId: UUID, options: DirectClientOptions = {}): Promise<DirectClient> {
    const socket = options.webSocketFactory?.(url) ?? new WebSocket(url);
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const inbox = new SocketInbox(socket);
    await waitForOpen(socket, timeoutMs);

    const initial = (
      await withTimeout(Promise.all([inbox.read(), inbox.read()]), timeoutMs, "handshake_timeout")
    ).map((message) => message.envelope);

    let serverId: UUID | undefined;
    let daemonPublicKeyWire: PublicKeyWire | undefined;
    for (const message of initial) {
      if (message.type === "hello") {
        const payload = message.payload as HelloPayload;
        serverId = payload.server_id ?? serverId;
      } else if (message.type === "e2ee_key_exchange") {
        const payload = message.payload as E2eeKeyExchangePayload;
        serverId = payload.server_id;
        daemonPublicKeyWire = payload.public_key;
      } else if (message.type === "error") {
        throw protocolError(message.payload as ErrorPayload);
      } else {
        throw new ProtocolClientError("unexpected_message", "unexpected handshake message");
      }
    }

    if (!serverId || !daemonPublicKeyWire) {
      throw new ProtocolClientError("invalid_handshake", "daemon handshake was incomplete");
    }

    const keypair = generateE2eeKeyPair();
    const e2ee = E2eeSession.device({
      serverId,
      deviceId,
      localKeypair: keypair,
      daemonPublicKeyWire,
    });
    const client = new DirectClient(socket, inbox, serverId, deviceId, e2ee, { timeoutMs });
    client.sendOuter(
      envelope("e2ee_key_exchange", {
        server_id: serverId,
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
    return this.expectPayload<PairAcceptPayload>("pair_accept");
  }

  async authenticate(device: DeviceState, server: PairedServerState): Promise<void> {
    const challenge = await this.expectPayload<AuthChallengePayload>("auth_challenge");
    const auth = await signAuthPayload(
      authPayloadForChallenge(device.device_id, challenge.challenge),
      server,
      device.device_signing_key_secret,
    );
    await this.sendInner(envelope("auth", auth));
  }

  async listSessions(): Promise<SessionListResultPayload> {
    await this.sendInner(envelope("session_list", {}));
    return this.expectPayload<SessionListResultPayload>("session_list_result");
  }

  async listDaemonClients(): Promise<DaemonClientsResultPayload> {
    await this.sendInner(envelope("daemon_clients", {}));
    return this.expectPayload<DaemonClientsResultPayload>("daemon_clients_result");
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

  async attachSession(sessionId: UUID, intent?: SessionAttachIntent): Promise<SessionAttachedPayload> {
    await this.sendInner(
      envelope("session_attach", {
        session_id: sessionId,
        ...(intent ? { intent } : {}),
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

  async resizeSession(sessionId: UUID, size: TerminalSize): Promise<void> {
    await this.sendInner(envelope("session_resize", { session_id: sessionId, size }));
  }

  async renameSession(sessionId: UUID, name: string): Promise<SessionRenamedPayload> {
    await this.sendInner(
      envelope("session_rename", {
        session_id: sessionId,
        name,
      } satisfies SessionRenamePayload),
    );
    return this.expectPayload<SessionRenamedPayload>("session_renamed");
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

  private async expectPayload<T>(expectedType: Envelope["type"]): Promise<T> {
    while (true) {
      const inner = await withTimeout(this.receiveInner(), this.timeoutMs, "response_timeout");
      if (inner.type === "pong") {
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
    this.socket.send(JSON.stringify(message));
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
    // 监听器必须在等待 open 前注册；daemon 会在连接建立后立即发送 hello/E2EE 公钥。
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
