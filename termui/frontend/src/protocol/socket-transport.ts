import { ProtocolClientError } from "./errors";
import type { Envelope } from "./types";
import { encodeUtf8 } from "./wire";

export interface QueuedMessage {
  envelope?: Envelope;
  binary?: Uint8Array;
}

export interface OpenWebSocketOptions {
  timeoutMs: number;
  hedgeDelayMs?: number;
  webSocketFactory?: (url: string) => WebSocket;
  signal?: AbortSignal;
}

export function queuedMessageBytes(message: QueuedMessage): number {
  if (message.binary) {
    return message.binary.byteLength;
  }
  return 0;
}

export function yieldToEventLoop(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

export function expectQueuedEnvelope(message: QueuedMessage): Envelope {
  if (!message.envelope) {
    throw new ProtocolClientError("unexpected_message", "expected JSON outer message");
  }
  return message.envelope;
}

export async function messageDataToBytes(data: unknown): Promise<Uint8Array> {
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

export function sendOuterMessage(socket: WebSocket, message: Envelope): void {
  socket.send(JSON.stringify(message));
}

export function openWebSocket(url: string, options: OpenWebSocketOptions): Promise<WebSocket> {
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

export function waitForOpen(socket: WebSocket, timeoutMs: number): Promise<void> {
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

export function abortedConnectionError(): ProtocolClientError {
  return new ProtocolClientError("connection_closed", "connection closed");
}

export function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

export function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw abortedConnectionError();
  }
}

export function withAbort<T>(promise: Promise<T>, signal?: AbortSignal): Promise<T> {
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

export function withTimeout<T>(promise: Promise<T>, timeoutMs: number, code: string): Promise<T> {
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
