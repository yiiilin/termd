import type { Envelope, MessageType, Nonce } from "./types";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export function envelope<P>(type: MessageType, payload: P): Envelope<P> {
  return { type, payload };
}

export function parseEnvelope(raw: string): Envelope {
  const parsed = JSON.parse(raw) as Envelope;
  if (!parsed || typeof parsed !== "object" || typeof parsed.type !== "string" || !("payload" in parsed)) {
    throw new Error("invalid_envelope");
  }
  return parsed;
}

export function encodeUtf8(value: string): Uint8Array {
  return textEncoder.encode(value);
}

export function decodeUtf8(bytes: Uint8Array): string {
  return textDecoder.decode(bytes);
}

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  bytes.forEach((byte) => {
    binary += String.fromCharCode(byte);
  });
  return btoa(binary);
}

export function base64ToBytes(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

export function nowMs(): number {
  return Date.now();
}

export function nonce(): Nonce {
  return `nonce-${randomUuid()}`;
}

export function randomUuid(): string {
  return globalThis.crypto.randomUUID();
}

export function concatBytes(...parts: Uint8Array[]): Uint8Array {
  const length = parts.reduce((sum, part) => sum + part.length, 0);
  const out = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

export function uuidToBytes(uuid: string): Uint8Array {
  const normalized = uuid.replaceAll("-", "");
  if (!/^[0-9a-fA-F]{32}$/.test(normalized)) {
    throw new Error("invalid_uuid");
  }
  const bytes = new Uint8Array(16);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(normalized.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

export function sequenceNonce(sequence: number): Uint8Array {
  const nonceBytes = new Uint8Array(12);
  const view = new DataView(nonceBytes.buffer);
  view.setBigUint64(4, BigInt(sequence), false);
  return nonceBytes;
}

export function sessionDataToBase64(bytes: Uint8Array): string {
  return bytesToBase64(bytes);
}

export function sessionDataFromBase64(value: string): Uint8Array {
  return base64ToBytes(value);
}

export async function messageDataToText(data: unknown): Promise<string> {
  if (typeof data === "string") {
    return data;
  }
  if (data instanceof Blob) {
    return data.text();
  }
  if (data instanceof ArrayBuffer) {
    return decodeUtf8(new Uint8Array(data));
  }
  if (ArrayBuffer.isView(data)) {
    return decodeUtf8(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  }
  return String(data);
}
