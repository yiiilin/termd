import { ed25519 } from "@noble/curves/ed25519";
import { sha256 } from "@noble/hashes/sha2";
import type { AuthPayload, DeviceState, E2eeKeyExchangePayload, HttpE2eeAuthPayload, PairedServerState, PublicKeyWire, RelayAdmissionPayload, UUID } from "./types";
import { base64ToBytes, bytesToBase64, concatBytes, encodeUtf8, nonce, nowMs, randomUuid } from "./wire";

const ED25519_WIRE_PREFIX = "ed25519-v1:";
const ED25519_SECRET_KEY_LEN = 32;
const ED25519_PUBLIC_KEY_LEN = 32;
const ED25519_SIGNATURE_LEN = 64;

export async function generateDeviceIdentity(deviceId: UUID = randomUuid()): Promise<DeviceState> {
  const secretKey = ed25519.utils.randomSecretKey();
  const publicKey = ed25519.getPublicKey(secretKey);

  return {
    device_id: deviceId,
    device_public_key: encodeEd25519Wire(publicKey),
    device_signing_key_secret: encodeEd25519Wire(secretKey),
  };
}

type DaemonPublicIdentity = Pick<PairedServerState, "server_id" | "daemon_public_key">;

export function authSigningInputBytes(
  payload: AuthPayload,
  daemon: DaemonPublicIdentity,
  e2eeTranscriptSha256?: string,
): Uint8Array {
  // 这里必须与 Rust `AuthSigningInput::to_bytes()` 完全一致；签名不包含 signature 本身。
  const fields = [
    encodeUtf8("termd-auth-v1\n"),
    canonicalField("server_id", daemon.server_id),
    canonicalField("daemon_public_key", daemon.daemon_public_key),
    canonicalField("device_id", payload.device_id),
    canonicalField("challenge", payload.challenge),
    canonicalField("nonce", payload.nonce),
    canonicalField("timestamp_ms", String(payload.timestamp_ms)),
  ];
  if (e2eeTranscriptSha256) {
    fields.push(canonicalField("e2ee_transcript_sha256", e2eeTranscriptSha256));
  }
  return concatBytes(...fields);
}

export async function signAuthPayload(
  payload: Omit<AuthPayload, "signature">,
  daemon: DaemonPublicIdentity,
  deviceSigningKeySecret: string,
  e2eeTranscriptSha256?: string,
): Promise<AuthPayload> {
  const unsigned: AuthPayload = { ...payload, signature: "ed25519-v1:placeholder" };
  const secretKey = decodeEd25519SecretKey(deviceSigningKeySecret);
  const signature = ed25519.sign(authSigningInputBytes(unsigned, daemon, e2eeTranscriptSha256), secretKey);

  return { ...payload, signature: encodeEd25519Wire(signature) };
}

export function httpE2eeSigningInputBytes(
  payload: HttpE2eeAuthPayload,
  daemon: DaemonPublicIdentity,
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

export async function signHttpE2eeAuthPayload(
  payload: Omit<HttpE2eeAuthPayload, "signature">,
  daemon: DaemonPublicIdentity,
  deviceSigningKeySecret: string,
): Promise<HttpE2eeAuthPayload> {
  const unsigned: HttpE2eeAuthPayload = { ...payload, signature: "ed25519-v1:placeholder" };
  const secretKey = decodeEd25519SecretKey(deviceSigningKeySecret);
  const signature = ed25519.sign(httpE2eeSigningInputBytes(unsigned, daemon), secretKey);

  return { ...payload, signature: encodeEd25519Wire(signature) };
}

export function relayAdmissionSigningInputBytes(
  payload: Omit<Extract<RelayAdmissionPayload, { kind: "device" }>, "kind" | "signature">,
  serverId: UUID,
): Uint8Array {
  // 中文注释：relay admission 只证明“这个设备愿意进入这个 daemon 房间”；
  // daemon 后续仍用 auth challenge 做最终认证。
  return concatBytes(
    encodeUtf8("termd-relay-admission-v1\n"),
    canonicalField("server_id", serverId),
    canonicalField("device_id", payload.device_id),
    canonicalField("nonce", payload.nonce),
    canonicalField("timestamp_ms", String(payload.timestamp_ms)),
  );
}

export async function signRelayAdmissionPayload(
  payload: Omit<Extract<RelayAdmissionPayload, { kind: "device" }>, "kind" | "signature">,
  serverId: UUID,
  deviceSigningKeySecret: string,
): Promise<Extract<RelayAdmissionPayload, { kind: "device" }>> {
  const secretKey = decodeEd25519SecretKey(deviceSigningKeySecret);
  const signature = ed25519.sign(relayAdmissionSigningInputBytes(payload, serverId), secretKey);
  return { kind: "device", ...payload, signature: encodeEd25519Wire(signature) };
}

export function daemonE2eeSigningInputBytes(
  payload: E2eeKeyExchangePayload,
  daemon: DaemonPublicIdentity,
): Uint8Array {
  // 这里必须与 Rust `DaemonE2eeSigningInput::to_bytes()` 完全一致。
  return concatBytes(
    encodeUtf8("termd-daemon-e2ee-key-exchange-v1\n"),
    canonicalField("server_id", daemon.server_id),
    canonicalField("daemon_public_key", daemon.daemon_public_key),
    canonicalField("device_id", payload.device_id),
    canonicalField("e2ee_public_key", payload.public_key),
    canonicalField("nonce", payload.nonce),
    canonicalField("timestamp_ms", String(payload.timestamp_ms)),
    canonicalField("packet_version", String(payload.packet_version ?? 0)),
    canonicalField("binary_version", String(payload.binary_version ?? 0)),
  );
}

export function e2eeAuthTranscriptDigestWire(
  daemonExchange: E2eeKeyExchangePayload,
  deviceExchange: E2eeKeyExchangePayload,
  daemon: DaemonPublicIdentity,
): string {
  // 摘要字段顺序和 Rust `E2eeAuthTranscript::to_bytes()` 保持一致。
  const fields = [
    encodeUtf8("termd-e2ee-auth-transcript-v1\n"),
    canonicalField("server_id", daemon.server_id),
    canonicalField("daemon_public_key", daemon.daemon_public_key),
    canonicalField("daemon_e2ee_public_key", daemonExchange.public_key),
    canonicalField("daemon_nonce", daemonExchange.nonce),
    canonicalField("daemon_timestamp_ms", String(daemonExchange.timestamp_ms)),
    canonicalField("daemon_packet_version", String(daemonExchange.packet_version ?? 0)),
    canonicalField("daemon_binary_version", String(daemonExchange.binary_version ?? 0)),
  ];
  if (daemonExchange.signature) {
    fields.push(canonicalField("daemon_signature", daemonExchange.signature));
  }
  fields.push(
    canonicalField("device_id", deviceExchange.device_id),
    canonicalField("device_e2ee_public_key", deviceExchange.public_key),
    canonicalField("device_nonce", deviceExchange.nonce),
    canonicalField("device_timestamp_ms", String(deviceExchange.timestamp_ms)),
    canonicalField("device_packet_version", String(deviceExchange.packet_version ?? 0)),
    canonicalField("device_binary_version", String(deviceExchange.binary_version ?? 0)),
  );
  return `sha256-v1:${bytesToBase64(sha256(concatBytes(...fields)))}`;
}

export function authPayloadForChallenge(
  deviceId: UUID,
  challenge: string,
): Omit<AuthPayload, "signature"> {
  return {
    device_id: deviceId,
    challenge,
    nonce: nonce(),
    timestamp_ms: nowMs(),
  };
}

export function decodeEd25519PublicKey(publicKey: PublicKeyWire): Uint8Array {
  return decodeEd25519Wire(publicKey, ED25519_PUBLIC_KEY_LEN);
}

export function decodeEd25519SecretKey(secretKey: string): Uint8Array {
  return decodeEd25519Wire(secretKey, ED25519_SECRET_KEY_LEN);
}

export function decodeEd25519Signature(signature: string): Uint8Array {
  return decodeEd25519Wire(signature, ED25519_SIGNATURE_LEN);
}

export async function verifyEd25519Signature(
  publicKey: Uint8Array,
  signingInput: Uint8Array,
  signatureWire: string,
): Promise<boolean> {
  return ed25519.verify(decodeEd25519Signature(signatureWire), signingInput, publicKey);
}

export function encodeEd25519Wire(bytes: Uint8Array): string {
  return `${ED25519_WIRE_PREFIX}${bytesToBase64(bytes)}`;
}

function decodeEd25519Wire(value: string, expectedLength: number): Uint8Array {
  const encoded = value.startsWith(ED25519_WIRE_PREFIX)
    ? value.slice(ED25519_WIRE_PREFIX.length)
    : "";
  const bytes = base64ToBytes(encoded);
  if (bytes.length !== expectedLength) {
    throw new Error("invalid_ed25519_wire_key");
  }
  return bytes;
}

function canonicalField(name: string, value: string): Uint8Array {
  const valueLength = encodeUtf8(value).length;
  return encodeUtf8(`${name}:${valueLength}:${value}\n`);
}
