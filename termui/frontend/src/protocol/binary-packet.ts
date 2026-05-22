import type { PacketKind, TerminalSize, UUID } from "./types";
import { base64ToBytes, decodeUtf8, encodeUtf8, uuidToBytes } from "./wire";

export type BinaryTerminalFramePayload =
  | { kind: "snapshot"; session_id: UUID; base_seq: number; terminal_seq: number; size: TerminalSize; data: Uint8Array }
  | { kind: "output"; session_id: UUID; terminal_seq: number; data: Uint8Array }
  | { kind: "resize"; session_id: UUID; terminal_seq: number; size: TerminalSize }
  | { kind: "exit"; session_id: UUID; terminal_seq: number; code?: number | null }
  | { kind: "batch"; session_id: UUID; frames: BinaryTerminalFramePayload[] };

export type BinaryProtocolPacketPayload =
  | { type: "json"; data: Uint8Array }
  | { type: "session_data"; session_id: UUID; data: Uint8Array }
  | { type: "terminal_frame"; frame: BinaryTerminalFramePayload }
  | { type: "error"; code: string; message: string; retryable: boolean };

export interface BinaryProtocolPacket {
  version: number;
  kind: PacketKind;
  id?: UUID;
  stream_id?: UUID;
  method?: string;
  seq?: number;
  ack?: number;
  credit?: number;
  payload?: BinaryProtocolPacketPayload;
}

const PACKET_KIND_TO_WIRE: Record<PacketKind, number> = {
  request: 1,
  response: 2,
  event: 3,
  stream_open: 4,
  stream_chunk: 5,
  stream_end: 6,
  cancel: 7,
  flow: 8,
  error: 9,
};

const PACKET_KIND_FROM_WIRE = new Map<number, PacketKind>(
  Object.entries(PACKET_KIND_TO_WIRE).map(([kind, value]) => [value, kind as PacketKind]),
);

const TERMINAL_FRAME_KIND_TO_WIRE: Record<BinaryTerminalFramePayload["kind"], number> = {
  snapshot: 1,
  output: 2,
  resize: 3,
  exit: 4,
  batch: 5,
};

const TERMINAL_FRAME_KIND_FROM_WIRE = new Map<number, BinaryTerminalFramePayload["kind"]>([
  // 兼容早期 binary v1：snapshot 曾经编码成 protobuf enum 默认值 0，字段 1 会被省略。
  [0, "snapshot"],
  ...Object.entries(TERMINAL_FRAME_KIND_TO_WIRE).map(([kind, value]) => [value, kind as BinaryTerminalFramePayload["kind"]] as const),
]);

export function encodeBinaryProtocolPacket(packet: BinaryProtocolPacket): Uint8Array {
  const writer = new ProtoWriter();
  writer.uint32(1, packet.version);
  writer.uint32(2, PACKET_KIND_TO_WIRE[packet.kind]);
  if (packet.id) {
    writer.bytes(3, uuidToBytes(packet.id));
  }
  if (packet.stream_id) {
    writer.bytes(4, uuidToBytes(packet.stream_id));
  }
  if (packet.method) {
    writer.string(5, packet.method);
  }
  if (packet.seq) {
    writer.uint64(6, packet.seq);
  }
  if (packet.ack) {
    writer.uint64(7, packet.ack);
  }
  if (packet.credit) {
    writer.uint32(8, packet.credit);
  }
  if (packet.payload?.type === "json") {
    writer.bytes(20, packet.payload.data);
  } else if (packet.payload?.type === "session_data") {
    const payload = new ProtoWriter();
    payload.bytes(1, uuidToBytes(packet.payload.session_id));
    payload.bytes(2, packet.payload.data);
    writer.bytes(21, payload.finish());
  } else if (packet.payload?.type === "terminal_frame") {
    writer.bytes(22, encodeTerminalFramePayload(packet.payload.frame));
  } else if (packet.payload?.type === "error") {
    const payload = new ProtoWriter();
    payload.string(1, packet.payload.code);
    payload.string(2, packet.payload.message);
    payload.bool(3, packet.payload.retryable);
    writer.bytes(23, payload.finish());
  }
  return writer.finish();
}

export function decodeBinaryProtocolPacket(bytes: Uint8Array): BinaryProtocolPacket {
  const reader = new ProtoReader(bytes);
  const packet: BinaryProtocolPacket = {
    version: 0,
    kind: "request",
    payload: undefined,
  };
  while (!reader.done()) {
    const { field, wireType } = reader.tag();
    switch (field) {
      case 1:
        packet.version = reader.uint32(wireType);
        break;
      case 2: {
        const kind = PACKET_KIND_FROM_WIRE.get(reader.uint32(wireType));
        if (!kind) {
          throw new Error("invalid_binary_packet_kind");
        }
        packet.kind = kind;
        break;
      }
      case 3:
        packet.id = bytesToUuid(reader.bytes(wireType));
        break;
      case 4:
        packet.stream_id = bytesToUuid(reader.bytes(wireType));
        break;
      case 5:
        packet.method = decodeUtf8(reader.bytes(wireType));
        break;
      case 6:
        packet.seq = reader.uint64(wireType);
        break;
      case 7:
        packet.ack = reader.uint64(wireType);
        break;
      case 8:
        packet.credit = reader.uint32(wireType);
        break;
      case 20:
        packet.payload = { type: "json", data: reader.bytes(wireType) };
        break;
      case 21:
        packet.payload = decodeSessionDataPayload(reader.bytes(wireType));
        break;
      case 22:
        packet.payload = { type: "terminal_frame", frame: decodeTerminalFramePayload(reader.bytes(wireType)) };
        break;
      case 23:
        packet.payload = decodeErrorPayload(reader.bytes(wireType));
        break;
      default:
        reader.skip(wireType);
        break;
    }
  }
  return packet;
}

function decodeSessionDataPayload(bytes: Uint8Array): BinaryProtocolPacketPayload {
  const reader = new ProtoReader(bytes);
  let sessionId: UUID | undefined;
  let data = new Uint8Array();
  while (!reader.done()) {
    const { field, wireType } = reader.tag();
    if (field === 1) {
      sessionId = bytesToUuid(reader.bytes(wireType));
    } else if (field === 2) {
      data = new Uint8Array(reader.bytes(wireType));
    } else {
      reader.skip(wireType);
    }
  }
  if (!sessionId) {
    throw new Error("invalid_binary_session_data");
  }
  return { type: "session_data", session_id: sessionId, data };
}

export function terminalFrameJsonToBinary(payload: unknown): BinaryTerminalFramePayload {
  const frame = payload as {
    kind?: string;
    session_id?: UUID;
    base_seq?: number;
    terminal_seq?: number;
    size?: TerminalSize;
    data_base64?: string;
    data_bytes?: Uint8Array;
    frames?: unknown[];
  };
  if (!frame.session_id) {
    throw new Error("invalid_binary_terminal_frame");
  }
  switch (frame.kind) {
    case "snapshot":
      return {
        kind: "snapshot",
        session_id: frame.session_id,
        base_seq: frame.base_seq ?? 0,
        terminal_seq: frame.terminal_seq ?? 0,
        size: frame.size ?? { rows: 0, cols: 0, pixel_width: 0, pixel_height: 0 },
        data: frame.data_bytes ?? base64ToBytes(frame.data_base64 ?? ""),
      };
    case "output":
      return {
        kind: "output",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq ?? 0,
        data: frame.data_bytes ?? base64ToBytes(frame.data_base64 ?? ""),
      };
    case "resize":
      return {
        kind: "resize",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq ?? 0,
        size: frame.size ?? { rows: 0, cols: 0, pixel_width: 0, pixel_height: 0 },
      };
    case "exit":
      return {
        kind: "exit",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq ?? 0,
        code: (frame as { code?: number | null }).code ?? null,
      };
    case "batch":
      return {
        kind: "batch",
        session_id: frame.session_id,
        frames: (frame.frames ?? []).map(terminalFrameJsonToBinary),
      };
    default:
      throw new Error("invalid_binary_terminal_frame");
  }
}

export function terminalFrameBinaryToJson(frame: BinaryTerminalFramePayload): unknown {
  switch (frame.kind) {
    case "snapshot":
      return {
        kind: "snapshot",
        session_id: frame.session_id,
        base_seq: frame.base_seq,
        terminal_seq: frame.terminal_seq,
        size: frame.size,
        data_bytes: frame.data,
      };
    case "output":
      return {
        kind: "output",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq,
        data_bytes: frame.data,
      };
    case "resize":
      return {
        kind: "resize",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq,
        size: frame.size,
      };
    case "exit":
      return {
        kind: "exit",
        session_id: frame.session_id,
        terminal_seq: frame.terminal_seq,
        code: frame.code ?? null,
      };
    case "batch":
      return {
        kind: "batch",
        session_id: frame.session_id,
        frames: frame.frames.map(terminalFrameBinaryToJson),
      };
  }
}

function encodeTerminalFramePayload(frame: BinaryTerminalFramePayload): Uint8Array {
  const writer = new ProtoWriter();
  writer.uint32(1, TERMINAL_FRAME_KIND_TO_WIRE[frame.kind]);
  writer.bytes(2, uuidToBytes(frame.session_id));
  if (frame.kind === "snapshot") {
    writer.uint64(3, frame.base_seq);
  }
  if (frame.kind !== "batch") {
    writer.uint64(4, frame.terminal_seq);
  }
  if (frame.kind === "snapshot" || frame.kind === "resize") {
    writer.bytes(5, encodeTerminalSize(frame.size));
  }
  if (frame.kind === "snapshot" || frame.kind === "output") {
    writer.bytes(6, frame.data);
  }
  if (frame.kind === "batch") {
    for (const child of frame.frames) {
      writer.bytes(7, encodeTerminalFramePayload(child));
    }
  }
  if (frame.kind === "exit" && frame.code !== undefined && frame.code !== null) {
    writer.int32(8, frame.code);
  }
  return writer.finish();
}

function decodeTerminalFramePayload(bytes: Uint8Array): BinaryTerminalFramePayload {
  const reader = new ProtoReader(bytes);
  let kind: BinaryTerminalFramePayload["kind"] | undefined;
  let sessionId: UUID | undefined;
  let baseSeq = 0;
  let terminalSeq = 0;
  let size: TerminalSize | undefined;
  let data = new Uint8Array();
  const frames: BinaryTerminalFramePayload[] = [];
  let code: number | null = null;
  while (!reader.done()) {
    const { field, wireType } = reader.tag();
    switch (field) {
      case 1:
        kind = TERMINAL_FRAME_KIND_FROM_WIRE.get(reader.uint32(wireType));
        break;
      case 2:
        sessionId = bytesToUuid(reader.bytes(wireType));
        break;
      case 3:
        baseSeq = reader.uint64(wireType);
        break;
      case 4:
        terminalSeq = reader.uint64(wireType);
        break;
      case 5:
        size = decodeTerminalSize(reader.bytes(wireType));
        break;
      case 6:
        data = new Uint8Array(reader.bytes(wireType));
        break;
      case 7:
        frames.push(decodeTerminalFramePayload(reader.bytes(wireType)));
        break;
      case 8:
        code = reader.int32(wireType);
        break;
      default:
        reader.skip(wireType);
        break;
    }
  }
  if (!kind && sessionId && size) {
    // 旧 snapshot 的 kind=0 会被 prost 当默认值省略；带 size 的无 kind terminal frame 只可能是 snapshot。
    kind = "snapshot";
  }
  if (!kind || !sessionId) {
    throw new Error("invalid_binary_terminal_frame");
  }
  switch (kind) {
    case "snapshot":
      return { kind, session_id: sessionId, base_seq: baseSeq, terminal_seq: terminalSeq, size: size ?? { rows: 0, cols: 0, pixel_width: 0, pixel_height: 0 }, data };
    case "output":
      return { kind, session_id: sessionId, terminal_seq: terminalSeq, data };
    case "resize":
      return { kind, session_id: sessionId, terminal_seq: terminalSeq, size: size ?? { rows: 0, cols: 0, pixel_width: 0, pixel_height: 0 } };
    case "exit":
      return { kind, session_id: sessionId, terminal_seq: terminalSeq, code };
    case "batch":
      return { kind, session_id: sessionId, frames };
  }
}

function encodeTerminalSize(size: TerminalSize): Uint8Array {
  const writer = new ProtoWriter();
  writer.uint32(1, size.rows);
  writer.uint32(2, size.cols);
  writer.uint32(3, size.pixel_width);
  writer.uint32(4, size.pixel_height);
  return writer.finish();
}

function decodeTerminalSize(bytes: Uint8Array): TerminalSize {
  const reader = new ProtoReader(bytes);
  const size: TerminalSize = { rows: 0, cols: 0, pixel_width: 0, pixel_height: 0 };
  while (!reader.done()) {
    const { field, wireType } = reader.tag();
    switch (field) {
      case 1:
        size.rows = reader.uint32(wireType);
        break;
      case 2:
        size.cols = reader.uint32(wireType);
        break;
      case 3:
        size.pixel_width = reader.uint32(wireType);
        break;
      case 4:
        size.pixel_height = reader.uint32(wireType);
        break;
      default:
        reader.skip(wireType);
        break;
    }
  }
  return size;
}

function decodeErrorPayload(bytes: Uint8Array): BinaryProtocolPacketPayload {
  const reader = new ProtoReader(bytes);
  const payload = { type: "error" as const, code: "", message: "", retryable: false };
  while (!reader.done()) {
    const { field, wireType } = reader.tag();
    if (field === 1) {
      payload.code = decodeUtf8(reader.bytes(wireType));
    } else if (field === 2) {
      payload.message = decodeUtf8(reader.bytes(wireType));
    } else if (field === 3) {
      payload.retryable = reader.bool(wireType);
    } else {
      reader.skip(wireType);
    }
  }
  return payload;
}

class ProtoWriter {
  private readonly chunks: number[] = [];

  uint32(field: number, value: number): void {
    this.tag(field, 0);
    this.varint(BigInt(value >>> 0));
  }

  uint64(field: number, value: number): void {
    this.tag(field, 0);
    this.varint(BigInt(value));
  }

  int32(field: number, value: number): void {
    this.tag(field, 0);
    this.varint(BigInt(value >>> 0));
  }

  bool(field: number, value: boolean): void {
    this.tag(field, 0);
    this.varint(value ? 1n : 0n);
  }

  string(field: number, value: string): void {
    this.bytes(field, encodeUtf8(value));
  }

  bytes(field: number, value: Uint8Array): void {
    this.tag(field, 2);
    this.varint(BigInt(value.length));
    this.chunks.push(...value);
  }

  finish(): Uint8Array {
    return new Uint8Array(this.chunks);
  }

  private tag(field: number, wireType: number): void {
    this.varint(BigInt((field << 3) | wireType));
  }

  private varint(value: bigint): void {
    let current = value;
    while (current >= 0x80n) {
      this.chunks.push(Number((current & 0x7fn) | 0x80n));
      current >>= 7n;
    }
    this.chunks.push(Number(current));
  }
}

class ProtoReader {
  private offset = 0;

  constructor(private readonly bytesValue: Uint8Array) {}

  done(): boolean {
    return this.offset >= this.bytesValue.length;
  }

  tag(): { field: number; wireType: number } {
    const tag = this.varint();
    return { field: Number(tag >> 3n), wireType: Number(tag & 0x07n) };
  }

  uint32(wireType: number): number {
    this.expectWireType(wireType, 0);
    return Number(this.varint());
  }

  uint64(wireType: number): number {
    this.expectWireType(wireType, 0);
    return Number(this.varint());
  }

  int32(wireType: number): number {
    const value = this.uint32(wireType);
    return value > 0x7fffffff ? value - 0x100000000 : value;
  }

  bool(wireType: number): boolean {
    return this.uint32(wireType) !== 0;
  }

  bytes(wireType: number): Uint8Array {
    this.expectWireType(wireType, 2);
    const length = Number(this.varint());
    const end = this.offset + length;
    if (end > this.bytesValue.length) {
      throw new Error("invalid_binary_packet_length");
    }
    const value = this.bytesValue.slice(this.offset, end);
    this.offset = end;
    return value;
  }

  skip(wireType: number): void {
    if (wireType === 0) {
      this.varint();
      return;
    }
    if (wireType === 2) {
      this.bytes(wireType);
      return;
    }
    throw new Error("unsupported_binary_packet_wire_type");
  }

  private expectWireType(actual: number, expected: number): void {
    if (actual !== expected) {
      throw new Error("invalid_binary_packet_wire_type");
    }
  }

  private varint(): bigint {
    let shift = 0n;
    let result = 0n;
    while (this.offset < this.bytesValue.length) {
      const byte = this.bytesValue[this.offset];
      this.offset += 1;
      result |= BigInt(byte & 0x7f) << shift;
      if ((byte & 0x80) === 0) {
        return result;
      }
      shift += 7n;
    }
    throw new Error("invalid_binary_packet_varint");
  }
}

function bytesToUuid(bytes: Uint8Array): UUID {
  if (bytes.length !== 16) {
    throw new Error("invalid_uuid");
  }
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
