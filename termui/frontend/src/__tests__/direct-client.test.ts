import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { DirectClient, ProtocolClientError } from "../protocol/direct-client";
import { generateDeviceIdentity } from "../protocol/auth";
import { PROTOCOL_PACKET_VERSION } from "../protocol/types";
import { MockDaemon } from "../test/mock-daemon";

describe("DirectClient", () => {
  let daemon: MockDaemon;
  const missingSessionId = "00000000-0000-0000-0000-000000000399";

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

  function connectDevice(deviceId: string, timeoutMs = 3000): Promise<DirectClient> {
    return DirectClient.connect(daemon.url, daemon.serverId, deviceId, {
      timeoutMs,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
    });
  }

  it("连接后第一帧发送 route_hello，然后才进入 hello/E2EE 握手", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000306");
    const client = await connectDevice(device.device_id);
    client.close();

    const firstOuter = JSON.parse(daemon.outerWireLog[0]) as {
      type: string;
      payload: { server_id: string; role: string; protocol_version: number; nonce?: string };
    };
    expect(firstOuter).toMatchObject({
      type: "route_hello",
      payload: {
        server_id: daemon.serverId,
        role: "client",
        protocol_version: PROTOCOL_PACKET_VERSION,
      },
    });
    expect(firstOuter.payload.nonce).toMatch(/^nonce-/);
  });

  it("连接阶段会校验 daemon E2EE key_exchange 的签名和 packet_version", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000312");
    const wrongIdentity = await generateDeviceIdentity("00000000-0000-0000-0000-000000000313");

    await expect(
      DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
        timeoutMs: 3000,
        expectedDaemonPublicKey: wrongIdentity.device_public_key,
      }),
    ).rejects.toMatchObject({ code: "daemon_identity_mismatch" });

    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      daemonPacketVersion: 2,
    });
    await expect(connectDevice(device.device_id)).rejects.toMatchObject({ code: "unsupported_protocol_version" });
  });

  it("连接阶段超时会关闭半开 WebSocket，避免 relay 重试残留旧连接", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      routeReadyDelayMs: 200,
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000309");

    await expect(connectDevice(device.device_id, 20)).rejects.toMatchObject({
      code: "route_prelude_timeout",
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(daemon.activeConnectionCount()).toBe(0);
  });

  it("完成 E2EE 内层 pairing，并且 outer wire 不包含 token", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000302");
    const client = await connectDevice(device.device_id);

    const accepted = await client.pair("secret-token", device.device_public_key);
    client.close();

    expect(accepted.server_id).toBe(daemon.serverId);
    expect(accepted.device_id).toBe(device.device_id);
    expect(daemon.outerWireText()).not.toContain("secret-token");
    expect(daemon.outerWireText()).not.toContain("pair_request");
  });

  it("已信任的同一浏览器 device 可以重新 pairing", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000305");
    const firstClient = await connectDevice(device.device_id);
    const firstAccepted = await firstClient.pair("secret-token", device.device_public_key);
    firstClient.close();

    const secondClient = await connectDevice(device.device_id);
    const secondAccepted = await secondClient.pair("secret-token", device.device_public_key);
    secondClient.close();

    expect(secondAccepted.server_id).toBe(firstAccepted.server_id);
    expect(secondAccepted.device_id).toBe(device.device_id);
  });

  it("已配对设备可 auth、list、attach、shared-control noop，并隐藏终端输入明文", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000303");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
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

  it("packet dispatcher 按 request id 归属响应和错误，错误不会污染并发请求", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000310");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    daemon.queueSessionListResponse(
      [
        {
          session_id: "00000000-0000-0000-0000-000000000301",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      40,
    );
    const listPromise = client.listSessions();
    const closePromise = client.closeSession(missingSessionId);

    await expect(closePromise).rejects.toMatchObject({ code: "session_not_found" });
    await expect(listPromise).resolves.toMatchObject({ sessions: [{ session_id: "00000000-0000-0000-0000-000000000301" }] });
    client.close();

    const receivedPackets = (
      daemon as unknown as {
        receivedPackets?: Array<{ id?: string; kind: string; method?: string; payload: unknown }>;
      }
    ).receivedPackets ?? [];
    const closePacket = receivedPackets.find((packet) => packet.method === "session.close");
    const errorPacket = (
      daemon as unknown as {
        sentPackets?: Array<{ id?: string; kind: string; payload: { code?: string } }>;
      }
    ).sentPackets?.find((packet) => packet.kind === "error" && packet.payload.code === "session_not_found");
    expect(closePacket?.id).toBeTruthy();
    expect(errorPacket?.id).toBe(closePacket?.id);
  });

  it("terminal attach 使用 stream packet，输出和输入带 seq，渲染完成后发送 flow 与 cancel", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000311");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id);
    const output = await client.receiveInner();
    client.ackTerminalRender(attached.session_id, 1, 1);
    await client.sendSessionData(attached.session_id, new TextEncoder().encode("stream-input"));
    client.close();
    await new Promise((resolve) => setTimeout(resolve, 20));

    const receivedPackets = (
      daemon as unknown as {
        receivedPackets?: Array<{ kind: string; method?: string; stream_id?: string; seq?: number; ack?: number; credit?: number }>;
      }
    ).receivedPackets ?? [];
    const sentPackets = (
      daemon as unknown as {
        sentPackets?: Array<{ kind: string; method?: string; stream_id?: string; seq?: number }>;
      }
    ).sentPackets ?? [];
    const attachOpen = receivedPackets.find((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach");
    const streamId = attachOpen?.stream_id;

    expect(attached.session_id).toBe(list.sessions[0].session_id);
    expect(output.type).toBe("session_data");
    expect(streamId).toMatch(/^[0-9a-f-]{36}$/);
    expect(sentPackets.find((packet) => packet.kind === "stream_chunk" && packet.stream_id === streamId)).toMatchObject({
      seq: 1,
    });
    expect(receivedPackets.find((packet) => packet.kind === "stream_chunk" && packet.stream_id === streamId)).toMatchObject({
      seq: 1,
    });
    expect(receivedPackets.find((packet) => packet.kind === "flow" && packet.stream_id === streamId)).toMatchObject({
      ack: 1,
      credit: 1,
    });
    expect(receivedPackets.find((packet) => packet.kind === "cancel" && packet.stream_id === streamId)).toBeTruthy();
  });

  it("短连接 attach 可以只拿 session 权限，不订阅终端输出", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000307");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id, { watchUpdates: false });
    const files = await client.listSessionFiles(attached.session_id);
    client.close();

    expect(attached.resize_owner).toBe(false);
    expect(files.session_id).toBe(attached.session_id);
    expect(daemon.attachRequests.at(-1)).toEqual({ session_id: attached.session_id, watch_updates: false });
  });

  it("terminal attach 会把已渲染的 session terminal_seq 作为 last_terminal_seq 发送", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000314");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id, { lastTerminalSeq: 10 });
    client.close();

    expect(daemon.attachRequests.at(-1)).toEqual({
      session_id: attached.session_id,
      watch_updates: true,
      last_terminal_seq: 10,
    });
  });

  it("可以通过 ping/pong 测量 daemon 网络延迟", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000308");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const latencyMs = await client.measureLatency();
    client.close();

    expect(latencyMs).toBeGreaterThanOrEqual(0);
    expect(daemon.pingMessages).toBe(1);
  });

  it("协议错误只暴露稳定 code 和安全 message", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000304");
    const client = await connectDevice(device.device_id);

    await expect(client.pair("wrong-token", device.device_public_key)).rejects.toMatchObject({
      code: "pairing_failed",
    } satisfies Partial<ProtocolClientError>);
    client.close();
  });
});
