import {
  decodeBinaryEncryptedFrame,
  encodeBinaryEncryptedFrame,
  type E2eeSession,
} from "./e2ee";
import { ProtocolClientError } from "./errors";
import type { ErrorPayload } from "./types";
import { decodeUtf8 } from "./wire";

const HTTP_UPLOAD_FRAME_PLAINTEXT_BYTES = 1024 * 1024;
const HTTP_E2EE_MAX_FRAME_BYTES = 2 * 1024 * 1024;
const HTTP_E2EE_MAX_PENDING_BYTES = 4 + HTTP_E2EE_MAX_FRAME_BYTES;

export interface HttpE2eeFetchOptions {
  timeoutMs?: number;
  firstFrameTimeoutMs?: number;
  onFrame?: (frame: Uint8Array) => void | Promise<void>;
  collectFrames?: boolean;
  signal?: AbortSignal;
}

export function concatByteChunks(chunks: Uint8Array[]): Uint8Array {
  const length = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

export class HttpFileTransferUnsupported extends Error {
  constructor() {
    super("http_file_transfer_unsupported");
  }
}

export function isHttpFileTransferUnsupported(error: unknown): boolean {
  return error instanceof HttpFileTransferUnsupported;
}

export function isReadableStreamBody(body: BodyInit): boolean {
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

export function encodeHttpE2eeFrames(e2ee: E2eeSession, plaintextFrames: Uint8Array[]): Uint8Array {
  return concatByteChunks(plaintextFrames.map((plaintext) => encodeHttpE2eeFrame(e2ee, plaintext)));
}

function bytesToBlobPart(bytes: Uint8Array): BlobPart {
  // 中文注释：这里的 Uint8Array 都由本地加密/封包代码创建，底层一定是 ArrayBuffer；
  // TypeScript 只能看到 ArrayBufferLike，直接窄化避免为每个上传分片再复制一次内存。
  return bytes as Uint8Array<ArrayBuffer>;
}

export function decodeHttpE2eeFrames(e2ee: E2eeSession, wire: Uint8Array): Uint8Array[] {
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

export async function decodeHttpE2eeReadable(
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

export function buildHttpUploadChunkBody(
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

export function parseHttpJsonFrame<T>(frame: Uint8Array | undefined): T {
  if (!frame) {
    throw new ProtocolClientError("invalid_file_transfer", "missing HTTP E2EE JSON frame");
  }
  return JSON.parse(decodeUtf8(frame)) as T;
}

export async function decodeHttpE2eeErrorResponse(
  response: Response,
  e2ee: E2eeSession,
): Promise<ErrorPayload> {
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

export function httpUrlFromSocketUrl(socketUrl: string, path: string): string {
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

export function fileNameFromPath(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() || "download";
}

export function bodyToArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

export async function blobSliceBytes(blob: Blob, start: number, end: number): Promise<Uint8Array> {
  const sliced = blob.slice(start, end);
  return readBlobBytes(sliced);
}

export async function readBlobBytes(blob: Blob): Promise<Uint8Array> {
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
