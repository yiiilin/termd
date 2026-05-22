import { describe, expect, it } from "vitest";
import {
  authSigningInputBytes,
  daemonE2eeSigningInputBytes,
  decodeEd25519PublicKey,
  e2eeAuthTranscriptDigestWire,
  generateDeviceIdentity,
  signAuthPayload,
  verifyEd25519Signature,
} from "../protocol/auth";
import type { AuthPayload, E2eeKeyExchangePayload, PairedServerState } from "../protocol/types";

describe("设备认证签名", () => {
  it("auth canonical signing input 与 Rust AuthSigningInput 字段顺序一致", () => {
    const payload: AuthPayload = {
      device_id: "00000000-0000-0000-0000-000000000001",
      challenge: "challenge-value",
      nonce: "nonce-value",
      timestamp_ms: 1710000000000,
      signature: "ed25519-v1:placeholder",
    };
    const server: PairedServerState = {
      server_id: "00000000-0000-0000-0000-000000000002",
      daemon_public_key: "ed25519-v1:daemon-public",
      url: "ws://127.0.0.1:8765/ws",
      paired_at_ms: 1710000000100,
    };

    const text = new TextDecoder().decode(authSigningInputBytes(payload, server));

    expect(text).toBe(
      "termd-auth-v1\n" +
        "server_id:36:00000000-0000-0000-0000-000000000002\n" +
        "daemon_public_key:24:ed25519-v1:daemon-public\n" +
        "device_id:36:00000000-0000-0000-0000-000000000001\n" +
        "challenge:15:challenge-value\n" +
        "nonce:11:nonce-value\n" +
        "timestamp_ms:13:1710000000000\n",
    );
  });

  it("生成的 Ed25519 设备签名可用本地 public key 验证", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000003");
    const server: PairedServerState = {
      server_id: "00000000-0000-0000-0000-000000000004",
      daemon_public_key: "ed25519-v1:daemon-public",
      url: "ws://127.0.0.1:8765/ws",
      paired_at_ms: 1710000000100,
    };
    const payload: Omit<AuthPayload, "signature"> = {
      device_id: device.device_id,
      challenge: "challenge-value",
      nonce: "nonce-value",
      timestamp_ms: 1710000000000,
    };

    const signed = await signAuthPayload(payload, server, device.device_signing_key_secret);
    const publicKey = decodeEd25519PublicKey(device.device_public_key);

    expect(signed.signature).toMatch(/^ed25519-v1:/);
    expect(
      await verifyEd25519Signature(
        publicKey,
        authSigningInputBytes(signed, server),
        signed.signature,
      ),
    ).toBe(true);
  });

  it("E2EE key exchange 签名输入和 auth transcript digest 与 Rust 字段顺序一致", () => {
    const server: PairedServerState = {
      server_id: "00000000-0000-0000-0000-000000000006",
      daemon_public_key: "ed25519-v1:daemon-public",
      url: "ws://127.0.0.1:8765/ws",
      paired_at_ms: 1710000000100,
    };
    const daemonExchange: E2eeKeyExchangePayload = {
      server_id: server.server_id,
      device_id: "00000000-0000-0000-0000-000000000000",
      public_key: "x25519-v1:daemon-e2ee",
      nonce: "daemon-nonce",
      timestamp_ms: 1710000000200,
      packet_version: 3,
      binary_version: 1,
      signature: "ed25519-v1:daemon-signature",
    };
    const deviceExchange: E2eeKeyExchangePayload = {
      server_id: server.server_id,
      device_id: "00000000-0000-0000-0000-000000000005",
      public_key: "x25519-v1:device-e2ee",
      nonce: "device-nonce",
      timestamp_ms: 1710000000300,
      packet_version: 3,
      binary_version: 1,
    };
    const authPayload: AuthPayload = {
      device_id: deviceExchange.device_id,
      challenge: "challenge-value",
      nonce: "auth-nonce",
      timestamp_ms: 1710000000400,
      signature: "ed25519-v1:placeholder",
    };

    const daemonSigningText = new TextDecoder().decode(daemonE2eeSigningInputBytes(daemonExchange, server));
    const transcriptDigest = e2eeAuthTranscriptDigestWire(daemonExchange, deviceExchange, server);
    const authSigningText = new TextDecoder().decode(authSigningInputBytes(authPayload, server, transcriptDigest));

    expect(daemonSigningText).toBe(
      "termd-daemon-e2ee-key-exchange-v1\n" +
        "server_id:36:00000000-0000-0000-0000-000000000006\n" +
        "daemon_public_key:24:ed25519-v1:daemon-public\n" +
        "device_id:36:00000000-0000-0000-0000-000000000000\n" +
        "e2ee_public_key:21:x25519-v1:daemon-e2ee\n" +
        "nonce:12:daemon-nonce\n" +
        "timestamp_ms:13:1710000000200\n" +
        "packet_version:1:3\n" +
        "binary_version:1:1\n",
    );
    expect(transcriptDigest).toMatch(/^sha256-v1:/);
    expect(authSigningText).toContain(`e2ee_transcript_sha256:${transcriptDigest.length}:${transcriptDigest}\n`);
  });
});
