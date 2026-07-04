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
import { recordProtocolTimeout } from "../diagnostics";
import { BINARY_PROTOCOL_VERSION, PROTOCOL_PACKET_VERSION } from "./types";
import type {
  ErrorPayload,
  HelloPayload,
  PublicKeyWire,
  RelayAdmissionPayload,
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
  relayAdmission?: RelayAdmissionPayload;
  webSocketFactory?: (url: string) => WebSocket;
  signal?: AbortSignal;
  createInbox: (socket: WebSocket) => DirectClientInbox;
}

export interface DirectHandshakeResult {
  socket: WebSocket;
  inbox: DirectClientInbox;
  binaryMode: boolean;
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
      diagnostics: {
        layer: "client",
        phase: "connect",
        transport: "websocket",
        serverId: routeServerId,
        deviceId,
      },
    });
    inbox = options.createInbox(socket);

    // 中文注释：route_hello 是统一 /ws 入口的第一帧；只有 route 被接受后，
    // 才能继续 daemon/client 明文 hello。
    sendOuterMessage(
      socket,
      envelope("route_hello", {
        server_id: routeServerId,
        role: "client",
        protocol_version: PROTOCOL_PACKET_VERSION,
        nonce: nonce(),
        admission: options.relayAdmission,
        timestamp_ms: nowMs(),
      }),
    );
    let routeReadyMessage;
    try {
      routeReadyMessage = await withAbort(
        withTimeout(inbox.read(), options.timeoutMs, "route_prelude_timeout"),
        abortSignal,
      );
    } catch (error) {
      if (error instanceof ProtocolClientError && error.code === "route_prelude_timeout") {
        recordProtocolTimeout({
          layer: "client",
          phase: "route_prelude",
          transport: "websocket",
          timeout_code: error.code,
          timeout_ms: options.timeoutMs,
          server_id: routeServerId,
          device_id: deviceId,
        });
      }
      throw error;
    }
    const routeReady = expectQueuedEnvelope(routeReadyMessage);
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

    let initialMessage;
    try {
      initialMessage = await withAbort(
        withTimeout(inbox.read(), options.timeoutMs, "handshake_timeout"),
        abortSignal,
      );
    } catch (error) {
      if (error instanceof ProtocolClientError && error.code === "handshake_timeout") {
        recordProtocolTimeout({
          layer: "client",
          phase: "handshake",
          transport: "websocket",
          timeout_code: error.code,
          timeout_ms: options.timeoutMs,
          server_id: routeServerId,
          device_id: deviceId,
        });
      }
      throw error;
    }
    const initial = expectQueuedEnvelope(initialMessage);
    if (initial.type === "error") {
      throw protocolError(initial.payload as ErrorPayload);
    }
    if (initial.type !== "hello") {
      throw new ProtocolClientError("unexpected_message", "unexpected handshake message");
    }
    const hello = initial.payload as HelloPayload;
    if (hello.server_id && hello.server_id !== routeServerId) {
      throw new ProtocolClientError("route_server_mismatch", "daemon hello does not match requested route");
    }
    if (hello.protocol_version !== PROTOCOL_PACKET_VERSION) {
      throw new ProtocolClientError("unsupported_protocol_version", "daemon hello does not support packet v3");
    }
    if (
      options.expectedDaemonPublicKey &&
      hello.daemon_public_key !== options.expectedDaemonPublicKey
    ) {
      throw new ProtocolClientError("daemon_identity_mismatch", "daemon hello public key does not match expected identity");
    }
    const binaryMode = hello.binary_version === BINARY_PROTOCOL_VERSION;
    if (socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolClientError("connection_closed", "connection closed");
    }
    sendOuterMessage(socket, envelope("hello", {
      protocol_version: PROTOCOL_PACKET_VERSION,
      binary_version: binaryMode ? BINARY_PROTOCOL_VERSION : null,
      nonce: nonce(),
      timestamp_ms: nowMs(),
      server_id: routeServerId,
      device_id: deviceId,
    }));

    return {
      socket,
      inbox,
      binaryMode,
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
