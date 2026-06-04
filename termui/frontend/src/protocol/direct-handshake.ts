import {
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  e2eeAuthTranscriptDigestWire,
  verifyEd25519Signature,
} from "./auth";
import { E2eeSession, generateE2eeKeyPair } from "./e2ee";
import { ProtocolClientError, protocolError } from "./errors";
import {
  expectQueuedEnvelope,
  openWebSocket,
  sendOuterMessage,
  throwIfAborted,
  type QueuedMessage,
  withAbort,
  withTimeout,
} from "./socket-transport";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "./types";
import type {
  E2eeKeyExchangePayload,
  ErrorPayload,
  HelloPayload,
  PublicKeyWire,
  RouteReadyPayload,
  UUID,
} from "./types";
import { envelope, nonce, nowMs } from "./wire";

export interface DirectClientInbox {
  read(): Promise<QueuedMessage>;
  rejectPending(error: Error): void;
}

export interface DirectHandshakeOptions {
  timeoutMs: number;
  socketOpenTimeoutMs: number;
  socketOpenHedgeDelayMs?: number;
  expectedDaemonPublicKey?: PublicKeyWire;
  webSocketFactory?: (url: string) => WebSocket;
  signal?: AbortSignal;
  createInbox: (socket: WebSocket) => DirectClientInbox;
}

export interface DirectHandshakeResult {
  socket: WebSocket;
  inbox: DirectClientInbox;
  daemonE2eePublicKeyWire: PublicKeyWire;
  e2ee: E2eeSession;
  binaryMode: boolean;
  e2eeTranscriptSha256: string;
}

export async function performDirectHandshake(
  url: string,
  routeServerId: UUID,
  deviceId: UUID,
  options: DirectHandshakeOptions,
): Promise<DirectHandshakeResult> {
  const abortSignal = options.signal;
  let socket: WebSocket | undefined;
  let inbox: DirectClientInbox | undefined;
  const closeSocketOnAbort = () => socket?.close();
  abortSignal?.addEventListener("abort", closeSocketOnAbort, { once: true });

  try {
    throwIfAborted(abortSignal);
    socket = await openWebSocket(url, {
      timeoutMs: options.socketOpenTimeoutMs,
      hedgeDelayMs: options.socketOpenHedgeDelayMs,
      webSocketFactory: options.webSocketFactory,
      signal: abortSignal,
    });
    inbox = options.createInbox(socket);

    // 中文注释：route_hello 是统一 /ws 入口的第一帧；只有 route 被接受后，
    // 才能继续原有 daemon hello / E2EE 握手。
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
        withTimeout(inbox.read(), options.timeoutMs, "route_prelude_timeout"),
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
        withTimeout(Promise.all([inbox.read(), inbox.read()]), options.timeoutMs, "handshake_timeout"),
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
    const deviceKeyExchange: E2eeKeyExchangePayload = {
      server_id: routeServerId,
      device_id: deviceId,
      public_key: keypair.publicKeyWire,
      nonce: nonce(),
      timestamp_ms: nowMs(),
      packet_version: PROTOCOL_PACKET_VERSION,
      ...(binaryMode ? { binary_version: BINARY_PROTOCOL_VERSION } : {}),
    };
    if (socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolClientError("connection_closed", "connection closed");
    }
    sendOuterMessage(socket, envelope("e2ee_key_exchange", deviceKeyExchange));

    return {
      socket,
      inbox,
      daemonE2eePublicKeyWire: daemonKeyExchange.public_key,
      e2ee,
      binaryMode,
      e2eeTranscriptSha256: e2eeAuthTranscriptDigestWire(
        daemonKeyExchange,
        deviceKeyExchange,
        {
          server_id: routeServerId,
          daemon_public_key: expectedDaemonPublicKey,
        },
      ),
    };
  } catch (error) {
    // 中文注释：连接建立阶段一旦超时、握手失败或被取消，必须关闭半开 socket，
    // 避免 relay/daemon 残留旧 client。
    socket?.close();
    inbox?.rejectPending(new ProtocolClientError("connection_closed", "connection closed"));
    throw error;
  } finally {
    abortSignal?.removeEventListener("abort", closeSocketOnAbort);
  }
}
