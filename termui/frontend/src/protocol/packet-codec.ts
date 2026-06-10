import type { BinaryProtocolPacket } from "./binary-packet";
import {
  terminalFrameBinaryToJson,
  terminalFrameJsonToBinary,
} from "./binary-packet";
import type {
  AttachFramePayload,
  PacketErrorPayload,
  ProtocolPacket,
  SessionDataPayload,
  SessionFileTransferChunkPayload,
  UUID,
} from "./types";
import {
  base64ToBytes,
  bytesToBase64,
  decodeUtf8,
  encodeUtf8,
} from "./wire";

export interface ProtocolPacketBinaryEncodingOptions {
  streamChunkPayloadType?: "attach_frame";
}

export function protocolPacketToBinary(
  packet: ProtocolPacket,
  options: ProtocolPacketBinaryEncodingOptions = {},
): BinaryProtocolPacket {
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
    // 中文注释：stream chunk 优先使用专用二进制 payload，避免终端和文件数据退回 JSON/base64。
    const payload = packet.payload as {
      session_id?: UUID;
      data_base64?: string;
      data_bytes?: Uint8Array;
      kind?: string;
      offset_bytes?: number;
      size_bytes?: number;
      eof?: boolean;
    };
    if (
      options.streamChunkPayloadType === "attach_frame" &&
      payload.session_id &&
      (payload.data_bytes instanceof Uint8Array || typeof payload.data_base64 === "string")
    ) {
      const data = payload.data_bytes ?? base64ToBytes(payload.data_base64 ?? "");
      return {
        ...binary,
        payload: { type: "attach_frame", session_id: payload.session_id, data },
      };
    }
    if (
      payload.session_id &&
      !(payload.kind) &&
      !(typeof payload.offset_bytes === "number") &&
      (payload.data_bytes instanceof Uint8Array || typeof payload.data_base64 === "string")
    ) {
      // 中文注释：attach_frame 必须由调用方结合 terminal stream 语义显式标注；
      // 否则这里按 legacy session_data 处理，避免把普通二进制 chunk 误判成 attach。
    }
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
    // 中文注释：error packet 也走专用 payload，保留 request/stream 归属字段。
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

export function binaryPacketToProtocol(packet: BinaryProtocolPacket): ProtocolPacket {
  let payload: unknown = {};
  if (packet.payload?.type === "json") {
    payload = JSON.parse(decodeUtf8(packet.payload.data));
  } else if (packet.payload?.type === "session_data") {
    payload = {
      session_id: packet.payload.session_id,
      data_base64: bytesToBase64(packet.payload.data),
      data_bytes: packet.payload.data,
    } satisfies SessionDataPayload;
  } else if (packet.payload?.type === "attach_frame") {
    payload = {
      session_id: packet.payload.session_id,
      data_base64: bytesToBase64(packet.payload.data),
      data_bytes: packet.payload.data,
    } satisfies AttachFramePayload;
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
