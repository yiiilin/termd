import { chacha20poly1305 } from "@noble/ciphers/chacha";
import { x25519 } from "@noble/curves/ed25519";
import { hkdf } from "@noble/hashes/hkdf";
import { sha256 } from "@noble/hashes/sha2";
import type { EncryptedFramePayload, Envelope, PublicKeyWire, UUID } from "./types";
import {
  base64ToBytes,
  bytesToBase64,
  concatBytes,
  encodeUtf8,
  parseEnvelope,
  sequenceNonce,
  uuidToBytes,
} from "./wire";

const X25519_WIRE_PREFIX = "x25519-v1:";
const PROTOCOL_LABEL = encodeUtf8("termd-e2ee-v1");
const CLIENT_TO_SERVER_INFO = encodeUtf8("termd-e2ee-v1/client-to-server");
const SERVER_TO_CLIENT_INFO = encodeUtf8("termd-e2ee-v1/server-to-client");
const BINARY_E2EE_FRAME_MAGIC = new Uint8Array([0x54, 0x44, 0x32, 0x45]); // TD2E
const BINARY_E2EE_FRAME_VERSION = 1;
const BINARY_E2EE_FRAME_KIND_ENCRYPTED = 1;
const BINARY_E2EE_FRAME_HEADER_LEN = 32;

export interface E2eeKeyPair {
  secretKey: Uint8Array;
  publicKey: Uint8Array;
  publicKeyWire: PublicKeyWire;
}

export interface BinaryEncryptedFramePayload {
  server_id: UUID;
  sequence: number;
  ciphertext: Uint8Array;
}

interface E2eeContext {
  serverId: UUID;
  deviceId: UUID;
  daemonPublicKey: Uint8Array;
  devicePublicKey: Uint8Array;
}

interface DeviceSessionInput {
  serverId: UUID;
  deviceId: UUID;
  localKeypair: E2eeKeyPair;
  daemonPublicKeyWire: PublicKeyWire;
}

interface DaemonSessionInput {
  serverId: UUID;
  deviceId: UUID;
  localKeypair: E2eeKeyPair;
  devicePublicKeyWire: PublicKeyWire;
}

export function generateE2eeKeyPair(): E2eeKeyPair {
  const secretKey = x25519.utils.randomSecretKey();
  const publicKey = x25519.getPublicKey(secretKey);
  return {
    secretKey,
    publicKey,
    publicKeyWire: encodeX25519PublicKey(publicKey),
  };
}

export function encodeX25519PublicKey(publicKey: Uint8Array): string {
  return `${X25519_WIRE_PREFIX}${bytesToBase64(publicKey)}`;
}

export function decodeX25519PublicKey(publicKeyWire: PublicKeyWire): Uint8Array {
  if (!publicKeyWire.startsWith(X25519_WIRE_PREFIX)) {
    throw new Error("unsupported_x25519_public_key");
  }
  const bytes = base64ToBytes(publicKeyWire.slice(X25519_WIRE_PREFIX.length));
  if (bytes.length !== 32) {
    throw new Error("invalid_x25519_public_key");
  }
  return bytes;
}

export class E2eeSession {
  private nextSendSequence = 0;
  private nextReceiveSequence = 0;

  private constructor(
    private readonly context: E2eeContext,
    private readonly sendKey: Uint8Array,
    private readonly receiveKey: Uint8Array,
  ) {}

  static device(input: DeviceSessionInput): E2eeSession {
    const daemonPublicKey = decodeX25519PublicKey(input.daemonPublicKeyWire);
    const context: E2eeContext = {
      serverId: input.serverId,
      deviceId: input.deviceId,
      daemonPublicKey,
      devicePublicKey: input.localKeypair.publicKey,
    };
    const [clientToServerKey, serverToClientKey] = deriveDirectionKeys(
      input.localKeypair.secretKey,
      daemonPublicKey,
      context,
    );
    return new E2eeSession(context, clientToServerKey, serverToClientKey);
  }

  static daemon(input: DaemonSessionInput): E2eeSession {
    const devicePublicKey = decodeX25519PublicKey(input.devicePublicKeyWire);
    const context: E2eeContext = {
      serverId: input.serverId,
      deviceId: input.deviceId,
      daemonPublicKey: input.localKeypair.publicKey,
      devicePublicKey,
    };
    const [clientToServerKey, serverToClientKey] = deriveDirectionKeys(
      input.localKeypair.secretKey,
      devicePublicKey,
      context,
    );
    return new E2eeSession(context, serverToClientKey, clientToServerKey);
  }

  encryptJson(inner: Envelope): EncryptedFramePayload {
    const plaintext = encodeUtf8(JSON.stringify(inner));
    const { sequence, ciphertext } = this.encryptCiphertext(plaintext);
    return {
      server_id: this.context.serverId,
      sequence,
      ciphertext_base64: bytesToBase64(ciphertext),
    };
  }

  encryptBinary(plaintext: Uint8Array): BinaryEncryptedFramePayload {
    const { sequence, ciphertext } = this.encryptCiphertext(plaintext);
    return {
      server_id: this.context.serverId,
      sequence,
      ciphertext,
    };
  }

  decryptJson(frame: EncryptedFramePayload): Envelope {
    const plaintext = this.decryptBytes(frame);
    return parseEnvelope(new TextDecoder().decode(plaintext));
  }

  decryptBinary(frame: BinaryEncryptedFramePayload): Uint8Array {
    return this.decryptCiphertext(frame.server_id, frame.sequence, frame.ciphertext);
  }

  private encryptCiphertext(plaintext: Uint8Array): { sequence: number; ciphertext: Uint8Array } {
    const sequence = this.nextSendSequence;
    const ciphertext = chacha20poly1305(
      this.sendKey,
      sequenceNonce(sequence),
      associatedData(this.context, sequence),
    ).encrypt(plaintext);

    this.nextSendSequence += 1;
    return { sequence, ciphertext };
  }

  private decryptBytes(frame: EncryptedFramePayload): Uint8Array {
    let ciphertext: Uint8Array;
    try {
      ciphertext = base64ToBytes(frame.ciphertext_base64);
    } catch (error) {
      throw new Error("decrypt failed", { cause: error });
    }
    return this.decryptCiphertext(frame.server_id, frame.sequence, ciphertext);
  }

  private decryptCiphertext(serverId: UUID, sequence: number, ciphertext: Uint8Array): Uint8Array {
    if (serverId !== this.context.serverId) {
      throw new Error("server_id mismatch");
    }
    if (sequence !== this.nextReceiveSequence) {
      throw new Error(`unexpected sequence: expected ${this.nextReceiveSequence}, received ${sequence}`);
    }

    let plaintext: Uint8Array;
    try {
      plaintext = chacha20poly1305(
        this.receiveKey,
        sequenceNonce(sequence),
        associatedData(this.context, sequence),
      ).decrypt(ciphertext);
    } catch (error) {
      // 解密失败不得推进 receive sequence，否则攻击者可用坏帧造成后续合法帧失序。
      throw new Error("decrypt failed", { cause: error });
    }

    this.nextReceiveSequence += 1;
    return plaintext;
  }
}

export function encodeBinaryEncryptedFrame(frame: BinaryEncryptedFramePayload): Uint8Array {
  const wire = new Uint8Array(BINARY_E2EE_FRAME_HEADER_LEN + frame.ciphertext.length);
  wire.set(BINARY_E2EE_FRAME_MAGIC, 0);
  wire[4] = BINARY_E2EE_FRAME_VERSION;
  wire[5] = BINARY_E2EE_FRAME_KIND_ENCRYPTED;
  wire.set(uuidToBytes(frame.server_id), 8);
  new DataView(wire.buffer, wire.byteOffset + 24, 8).setBigUint64(0, BigInt(frame.sequence), false);
  wire.set(frame.ciphertext, BINARY_E2EE_FRAME_HEADER_LEN);
  return wire;
}

export function decodeBinaryEncryptedFrame(wire: Uint8Array): BinaryEncryptedFramePayload {
  if (
    wire.length < BINARY_E2EE_FRAME_HEADER_LEN ||
    !BINARY_E2EE_FRAME_MAGIC.every((byte, index) => wire[index] === byte) ||
    wire[4] !== BINARY_E2EE_FRAME_VERSION ||
    wire[5] !== BINARY_E2EE_FRAME_KIND_ENCRYPTED ||
    wire[6] !== 0 ||
    wire[7] !== 0
  ) {
    throw new Error("invalid_binary_e2ee_frame");
  }
  return {
    server_id: bytesToUuid(wire.subarray(8, 24)),
    sequence: Number(new DataView(wire.buffer, wire.byteOffset + 24, 8).getBigUint64(0, false)),
    ciphertext: wire.slice(BINARY_E2EE_FRAME_HEADER_LEN),
  };
}

function bytesToUuid(bytes: Uint8Array): UUID {
  if (bytes.length !== 16) {
    throw new Error("invalid_uuid");
  }
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function deriveDirectionKeys(
  localSecretKey: Uint8Array,
  peerPublicKey: Uint8Array,
  context: E2eeContext,
): [Uint8Array, Uint8Array] {
  const sharedSecret = x25519.getSharedSecret(localSecretKey, peerPublicKey);
  if (sharedSecret.every((byte) => byte === 0)) {
    throw new Error("non_contributory_x25519_shared_secret");
  }
  const salt = kdfSalt(context);
  return [
    hkdf(sha256, sharedSecret, salt, CLIENT_TO_SERVER_INFO, 32),
    hkdf(sha256, sharedSecret, salt, SERVER_TO_CLIENT_INFO, 32),
  ];
}

function kdfSalt(context: E2eeContext): Uint8Array {
  return sha256(
    concatBytes(
      PROTOCOL_LABEL,
      encodeUtf8("/kdf-context"),
      uuidToBytes(context.serverId),
      uuidToBytes(context.deviceId),
      context.daemonPublicKey,
      context.devicePublicKey,
    ),
  );
}

function associatedData(context: E2eeContext, sequence: number): Uint8Array {
  const sequenceBytes = new Uint8Array(8);
  new DataView(sequenceBytes.buffer).setBigUint64(0, BigInt(sequence), false);
  return concatBytes(
    PROTOCOL_LABEL,
    encodeUtf8("/aead"),
    uuidToBytes(context.serverId),
    uuidToBytes(context.deviceId),
    sequenceBytes,
    sha256(context.daemonPublicKey),
    sha256(context.devicePublicKey),
  );
}
