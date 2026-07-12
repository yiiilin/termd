import type { PairedServerState, PublicKeyWire, SignatureWire, UUID } from "../protocol/types";
import { concatBytes, encodeUtf8 } from "../protocol/wire";

export const PROTOCOL_PACKET_VERSION = 3;
export const BINARY_PROTOCOL_VERSION = 2;
export type PacketStreamId = UUID;
export type PacketKind = "request" | "response" | "event" | "stream_open" | "stream_chunk" | "stream_end" | "cancel" | "flow" | "error";
export interface PacketErrorPayload { code: string; message: string; retryable: boolean }
export interface ProtocolPacket<P = unknown> {
  version: number;
  kind: PacketKind;
  id?: UUID;
  stream_id?: PacketStreamId;
  method?: string;
  seq?: number;
  ack?: number;
  credit?: number;
  payload: P;
}
export interface E2eeKeyExchangePayload {
  server_id: UUID;
  device_id: UUID;
  public_key: PublicKeyWire;
  nonce: string;
  timestamp_ms: number;
  packet_version?: number | null;
  binary_version?: number | null;
  signature?: SignatureWire | null;
}
export interface EncryptedFramePayload { server_id: UUID; sequence: number; ciphertext_base64: string }
export interface HttpE2eeAuthPayload {
  device_id: UUID;
  e2ee_public_key: PublicKeyWire;
  nonce: string;
  timestamp_ms: number;
  method: string;
  path: string;
  signature: SignatureWire;
}
export interface SessionScopeGrantPayload { session_id: UUID; token: string; expires_at_ms: number }
export interface SessionCursorPayload { session_id: UUID; row: number; col: number; focused: boolean }
export interface MetadataSubscribePayload { status_interval_ms?: number | null; clients?: boolean }

export type BinaryProtocolPacket = any;
export interface ProtocolPacketBinaryEncodingOptions {
  attachFrameBytes?: Uint8Array;
  streamChunkPayloadType?: string;
}

export function decodeBinaryProtocolPacket(bytes: Uint8Array): BinaryProtocolPacket {
  return JSON.parse(new TextDecoder().decode(bytes)) as BinaryProtocolPacket;
}

export function encodeBinaryProtocolPacket(packet: BinaryProtocolPacket): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(packet));
}

export function binaryPacketToProtocol(packet: BinaryProtocolPacket): ProtocolPacket {
  return packet;
}

export function protocolPacketToBinary(
  packet: ProtocolPacket,
  _options?: ProtocolPacketBinaryEncodingOptions,
): BinaryProtocolPacket {
  return packet;
}

export function legacyEnvelopeTypeForProtocolMethod(method?: string): any {
  return method?.replaceAll(".", "_");
}

export function protocolEventMethodForLegacyEnvelopeType(type: string): string {
  return type.replaceAll("_", ".");
}

export function protocolMethodNeedsEmptyAck(_method?: string): boolean {
  return false;
}

export function httpE2eeSigningInputBytes(
  payload: HttpE2eeAuthPayload,
  daemon: Pick<PairedServerState, "server_id" | "daemon_public_key">,
): Uint8Array {
  return concatBytes(
    encodeUtf8("termd-http-e2ee-v1\n"),
    canonicalField("server_id", daemon.server_id),
    canonicalField("daemon_public_key", daemon.daemon_public_key),
    canonicalField("device_id", payload.device_id),
    canonicalField("e2ee_public_key", payload.e2ee_public_key),
    canonicalField("nonce", payload.nonce),
    canonicalField("timestamp_ms", String(payload.timestamp_ms)),
    canonicalField("method", payload.method),
    canonicalField("path", payload.path),
  );
}

function canonicalField(name: string, value: string): Uint8Array {
  return encodeUtf8(`${name}:${encodeUtf8(value).length}:${value}\n`);
}
