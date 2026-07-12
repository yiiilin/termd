import { ed25519 } from "@noble/curves/ed25519";
import type { AuthPayload, DeviceState, PairedServerState, PublicKeyWire, UUID } from "./types";
import { base64ToBytes, bytesToBase64, encodeUtf8, nonce, nowMs, randomUuid } from "./wire";

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
): Uint8Array {
  // Must match Rust `AccessTokenProofInput::to_bytes()` exactly.
  return encodeUtf8(
    "termd-access-token-v1\n" +
      `server_id=${daemon.server_id}\n` +
      `device_id=${payload.device_id}\n` +
      `challenge=${payload.challenge}\n` +
      `nonce=${payload.nonce}\n` +
      `timestamp_ms=${payload.timestamp_ms}\n`,
  );
}

export async function signAuthPayload(
  payload: Omit<AuthPayload, "signature">,
  daemon: DaemonPublicIdentity,
  deviceSigningKeySecret: string,
): Promise<AuthPayload> {
  const unsigned: AuthPayload = { ...payload, signature: "ed25519-v1:placeholder" };
  const secretKey = decodeEd25519SecretKey(deviceSigningKeySecret);
  const signature = ed25519.sign(authSigningInputBytes(unsigned, daemon), secretKey);

  return { ...payload, signature: encodeEd25519Wire(signature) };
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
