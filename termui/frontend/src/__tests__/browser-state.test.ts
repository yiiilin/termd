import { beforeEach, describe, expect, it } from "vitest";
import { generateDeviceIdentity } from "../protocol/auth";
import {
  clearBrowserState,
  ensureDevice,
  forgetDaemon,
  loadBrowserState,
  recordPairing,
  recordServerUrl,
  renameDaemon,
  saveBrowserPreferences,
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

  it("允许把已配对 daemon 的连接地址切换到 relay URL，但不持久化 secret query", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000026");
    const serverId = "00000000-0000-0000-0000-000000000027";
    const directUrl = "ws://127.0.0.1:8765/ws";
    const legacyRelayUrl = `wss://relay.example/ws/${serverId}/client?relay_token=relay-secret#debug`;
    const relayUrl = "wss://relay.example/ws";

    await saveBrowserState({ device, pairedServers: [] });
    await recordPairing(
      {
        server_id: serverId,
        daemon_public_key: "ed25519-v1:daemon-public",
        device_id: device.device_id,
        expires_at_ms: 1710000060000,
      },
      directUrl,
    );

    const next = await recordServerUrl(serverId, legacyRelayUrl);
    const loaded = await loadBrowserState();
    const raw = JSON.stringify(loaded);

    expect(next.defaultUrl).toBe(relayUrl);
    expect(defaultServerUrl(next)).toBe(relayUrl);
    expect(defaultServerUrl(loaded)).toBe(relayUrl);
    expect(raw).not.toContain("relay-secret");
    expect(raw).not.toContain("relay_token");
  });

  it("会把旧的裸 websocket 主机地址归一到 /ws", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000028");

    await saveBrowserState({
      device,
      pairedServers: [
        {
          server_id: "00000000-0000-0000-0000-000000000029",
          daemon_public_key: "ed25519-v1:daemon-public",
          url: "ws://127.0.0.1:8765",
          paired_at_ms: 1710000000000,
        },
      ],
      defaultServerId: "00000000-0000-0000-0000-000000000029",
      defaultUrl: "ws://127.0.0.1:8765",
    });

    const loaded = await loadBrowserState();

    expect(loaded.defaultUrl).toBe("ws://127.0.0.1:8765/ws");
    expect(defaultServerUrl(loaded)).toBe("ws://127.0.0.1:8765/ws");
  });

  it("支持重命名和删除已配对 daemon，并维护默认 daemon", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000030");
    const firstServerId = "00000000-0000-0000-0000-000000000031";
    const secondServerId = "00000000-0000-0000-0000-000000000032";

    await saveBrowserState({ device, pairedServers: [] });
    await recordPairing(
      {
        server_id: firstServerId,
        daemon_public_key: "ed25519-v1:first",
        device_id: device.device_id,
        expires_at_ms: 1710000060000,
      },
      "ws://127.0.0.1:8765/ws",
    );
    await recordPairing(
      {
        server_id: secondServerId,
        daemon_public_key: "ed25519-v1:second",
        device_id: device.device_id,
        expires_at_ms: 1710000060000,
      },
      "wss://relay.example/ws",
    );

    const renamed = await renameDaemon(secondServerId, "  Laptop relay  ");
    expect(renamed.defaultServerId).toBe(secondServerId);
    expect(renamed.pairedServers.find((server) => server.server_id === secondServerId)?.name).toBe("Laptop relay");

    const fallback = await forgetDaemon(secondServerId);
    expect(fallback.pairedServers.map((server) => server.server_id)).toEqual([firstServerId]);
    expect(fallback.defaultServerId).toBe(firstServerId);
    expect(fallback.defaultUrl).toBe("ws://127.0.0.1:8765/ws");

    const empty = await forgetDaemon(firstServerId);
    expect(empty.device).toEqual(device);
    expect(empty.pairedServers).toEqual([]);
    expect(empty.defaultServerId).toBeUndefined();
    expect(empty.defaultUrl).toBeUndefined();
  });

  it("不再把 session 文件树位置写入浏览器本地状态", async () => {
    await saveBrowserState({
      pairedServers: [],
      sessionUiState: {
        "00000000-0000-0000-0000-000000000024": {
          "00000000-0000-0000-0000-000000000025": {
            filesPath: "/home/me/project/src",
            updated_at_ms: 1710000000000,
          },
        },
      },
    } as BrowserState);
    const loaded = await loadBrowserState();
    const raw = JSON.stringify(loaded);

    expect(loaded).not.toHaveProperty("sessionUiState");
    expect(raw).not.toContain("/home/me/project/src");
    expect(raw).not.toContain("pairing_token");
    expect(raw).not.toContain("terminal-secret");
  });

  it("持久化客户端偏好，并对旧数据或异常值做安全归一化", async () => {
    const next = await saveBrowserPreferences({ language: "zh-CN", theme: "light" });
    expect(next.preferences).toEqual({ language: "zh-CN", theme: "light", notifications: "off", mobileShortcuts: [] });
    expect((await loadBrowserState()).preferences).toEqual({ language: "zh-CN", theme: "light", notifications: "off", mobileShortcuts: [] });

    await saveBrowserState({
      pairedServers: [],
      preferences: {
        language: "pirate",
        theme: "neon",
      },
    } as unknown as BrowserState);

    expect((await loadBrowserState()).preferences).toEqual({ language: "auto", theme: "system", notifications: "off", mobileShortcuts: [] });
  });

  it("持久化移动端快捷键和通知偏好，并过滤异常快捷键", async () => {
    const next = await saveBrowserPreferences({
      language: "en-US",
      theme: "dark",
      notifications: "mentions",
      mobileShortcuts: [
        { label: "PgUp", data: "\x1b[5~" },
        { label: "", data: "ignored" },
        { label: "Bad", data: "\0" },
      ],
    });

    expect(next.preferences).toEqual({
      language: "en-US",
      theme: "dark",
      notifications: "mentions",
      mobileShortcuts: [{ label: "PgUp", data: "\x1b[5~" }],
    });
  });
});

function defaultServerUrl(state: BrowserState): string | undefined {
  return state.pairedServers.find((server) => server.server_id === state.defaultServerId)?.url;
}
