import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { DirectClient, ProtocolClientError } from "../protocol/direct-client";
import { generateDeviceIdentity } from "../protocol/auth";
import { MockDaemon } from "../test/mock-daemon";

describe("DirectClient", () => {
  let daemon: MockDaemon;

  beforeEach(async () => {
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000301",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
    });
  });

  afterEach(async () => {
    await daemon.stop();
  });

  it("完成 E2EE 内层 pairing，并且 outer wire 不包含 token", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000302");
    const client = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });

    const accepted = await client.pair("secret-token", device.device_public_key);
    client.close();

    expect(accepted.server_id).toBe(daemon.serverId);
    expect(accepted.device_id).toBe(device.device_id);
    expect(daemon.outerWireText()).not.toContain("secret-token");
    expect(daemon.outerWireText()).not.toContain("pair_request");
  });

  it("已信任的同一浏览器 device 可以重新 pairing", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000305");
    const firstClient = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });
    const firstAccepted = await firstClient.pair("secret-token", device.device_public_key);
    firstClient.close();

    const secondClient = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });
    const secondAccepted = await secondClient.pair("secret-token", device.device_public_key);
    secondClient.close();

    expect(secondAccepted.server_id).toBe(firstAccepted.server_id);
    expect(secondAccepted.device_id).toBe(device.device_id);
  });

  it("已配对设备可 auth、list、attach、shared-control noop，并隐藏终端输入明文", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000303");
    const pairClient = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id);
    await client.sendSessionData(attached.session_id, new TextEncoder().encode("terminal-secret"));
    await client.sendSessionCursor(attached.session_id, { row: 12, col: 8, focused: true });
    const output = await client.receiveInner();
    const grant = await client.requestControl(attached.session_id);
    client.close();

    expect(list.sessions).toHaveLength(1);
    expect(attached.role).toBe("operator");
    expect(output.type).toBe("session_data");
    expect(grant.device_id).toBe(device.device_id);
    expect(daemon.decryptedInputs).toContain("terminal-secret");
    expect(daemon.sessionCursorUpdates).toContainEqual({ session_id: attached.session_id, row: 12, col: 8, focused: true });
    expect(daemon.outerWireText()).not.toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("session_data");
    expect(daemon.outerWireText()).not.toContain("session_cursor");
  });

  it("协议错误只暴露稳定 code 和安全 message", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000304");
    const client = await DirectClient.connect(daemon.url, device.device_id, { timeoutMs: 3000 });

    await expect(client.pair("wrong-token", device.device_public_key)).rejects.toMatchObject({
      code: "pairing_failed",
    } satisfies Partial<ProtocolClientError>);
    client.close();
  });
});
