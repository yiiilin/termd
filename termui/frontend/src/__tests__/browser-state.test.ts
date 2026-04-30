import { beforeEach, describe, expect, it } from "vitest";
import { generateDeviceIdentity } from "../protocol/auth";
import {
  clearBrowserState,
  ensureDevice,
  loadBrowserState,
  recordPairing,
  saveBrowserState,
} from "../state/browser-state";
import type { BrowserState } from "../protocol/types";

describe("浏览器本地状态", () => {
  beforeEach(async () => {
    await clearBrowserState();
  });

  it("保存设备身份和 daemon 公开身份，但不持久化 pairing token 或终端明文", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000011");

    await saveBrowserState({ device, pairedServers: [] });
    await recordPairing(
      {
        server_id: "00000000-0000-0000-0000-000000000022",
        daemon_public_key: "ed25519-v1:daemon-public",
        device_id: device.device_id,
        expires_at_ms: 1710000060000,
      },
      "ws://127.0.0.1:8765/ws",
    );

    const state = await loadBrowserState();
    const raw = JSON.stringify(state);

    expect(state.device?.device_signing_key_secret).toMatch(/^ed25519-v1:/);
    expect(state.defaultUrl).toBe("ws://127.0.0.1:8765/ws");
    expect(state.pairedServers).toHaveLength(1);
    expect(raw).not.toContain("pairing_token");
    expect(raw).not.toContain("secret-token");
    expect(raw).not.toContain("server_private_key");
    expect(raw).not.toContain("terminal-secret");
  });

  it("写入前按 schema 白名单丢弃旧状态里的敏感字段", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000012");
    const pollutedState = {
      device: {
        ...device,
        pairing_token: "persisted-pairing-token",
        server_private_key: "persisted-server-private-key",
      },
      pairedServers: [
        {
          server_id: "00000000-0000-0000-0000-000000000023",
          daemon_public_key: "ed25519-v1:daemon-public",
          url: "ws://127.0.0.1:8765/ws",
          paired_at_ms: 1710000000000,
          terminal_transcript: "persisted-terminal-plaintext",
          server_private_key: "persisted-server-private-key",
        },
      ],
      defaultServerId: "00000000-0000-0000-0000-000000000023",
      defaultUrl: "ws://127.0.0.1:8765/ws",
      terminalChunks: ["persisted-terminal-plaintext"],
    } as unknown as BrowserState;

    await saveBrowserState(pollutedState);

    const raw = JSON.stringify(await loadBrowserState());

    expect(raw).not.toContain("persisted-pairing-token");
    expect(raw).not.toContain("persisted-server-private-key");
    expect(raw).not.toContain("persisted-terminal-plaintext");
  });

  it("ensureDevice 复用现有 device identity，避免重复轮换破坏配对", async () => {
    const first = await ensureDevice();
    const second = await ensureDevice();

    expect(second).toEqual(first);
  });
});
