import { describe, expect, it } from "vitest";
import {
  E2eeSession,
  decodeX25519PublicKey,
  generateE2eeKeyPair,
} from "../protocol/e2ee";
import { envelope } from "../protocol/wire";

describe("E2EE 加密帧", () => {
  it("X25519 wire public key 使用 x25519-v1 前缀", () => {
    const keypair = generateE2eeKeyPair();

    expect(keypair.publicKeyWire).toMatch(/^x25519-v1:/);
    expect(decodeX25519PublicKey(keypair.publicKeyWire)).toHaveLength(32);
  });

  it("device 和 daemon 可以解密彼此帧，并且 outer wire 不暴露业务明文", () => {
    const serverId = "00000000-0000-0000-0000-000000000101";
    const deviceId = "00000000-0000-0000-0000-000000000102";
    const daemonKeypair = generateE2eeKeyPair();
    const deviceKeypair = generateE2eeKeyPair();
    const device = E2eeSession.device({
      serverId,
      deviceId,
      localKeypair: deviceKeypair,
      daemonPublicKeyWire: daemonKeypair.publicKeyWire,
    });
    const daemon = E2eeSession.daemon({
      serverId,
      deviceId,
      localKeypair: daemonKeypair,
      devicePublicKeyWire: deviceKeypair.publicKeyWire,
    });
    const inner = envelope("pair_request", {
      device_id: deviceId,
      device_public_key: "ed25519-v1:public",
      token: "secret-token",
      nonce: "nonce-1",
      timestamp_ms: 1710000000000,
    });

    const frame = device.encryptJson(inner);
    const outerWire = JSON.stringify(envelope("encrypted_frame", frame));

    expect(outerWire).not.toContain("pair_request");
    expect(outerWire).not.toContain("secret-token");
    expect(daemon.decryptJson(frame)).toEqual(inner);
  });

  it("解密失败或重放不会推进 receive sequence", () => {
    const serverId = "00000000-0000-0000-0000-000000000201";
    const deviceId = "00000000-0000-0000-0000-000000000202";
    const daemonKeypair = generateE2eeKeyPair();
    const deviceKeypair = generateE2eeKeyPair();
    const sender = E2eeSession.device({
      serverId,
      deviceId,
      localKeypair: deviceKeypair,
      daemonPublicKeyWire: daemonKeypair.publicKeyWire,
    });
    const receiver = E2eeSession.daemon({
      serverId,
      deviceId,
      localKeypair: daemonKeypair,
      devicePublicKeyWire: deviceKeypair.publicKeyWire,
    });
    const first = sender.encryptJson(envelope("ping", { nonce: "n1", timestamp_ms: 1 }));
    const second = sender.encryptJson(envelope("ping", { nonce: "n2", timestamp_ms: 2 }));
    const tampered = { ...first, ciphertext_base64: `${first.ciphertext_base64}A` };

    expect(() => receiver.decryptJson(tampered)).toThrow(/decrypt/i);
    expect(receiver.decryptJson(first)).toEqual(envelope("ping", { nonce: "n1", timestamp_ms: 1 }));
    expect(() => receiver.decryptJson(first)).toThrow(/sequence/i);
    expect(receiver.decryptJson(second)).toEqual(envelope("ping", { nonce: "n2", timestamp_ms: 2 }));
  });
});
