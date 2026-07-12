import { describe, expect, it } from "vitest";
import {
  authSigningInputBytes,
  decodeEd25519PublicKey,
  generateDeviceIdentity,
  signAuthPayload,
  verifyEd25519Signature,
} from "../protocol/auth";
import type { AuthPayload, PairedServerState } from "../protocol/types";

describe("设备认证签名", () => {
  it("access-token signing input matches Rust AccessTokenProofInput", () => {
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
      "termd-access-token-v1\n" +
        "server_id=00000000-0000-0000-0000-000000000002\n" +
        "device_id=00000000-0000-0000-0000-000000000001\n" +
        "challenge=challenge-value\n" +
        "nonce=nonce-value\n" +
        "timestamp_ms=1710000000000\n",
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
});
