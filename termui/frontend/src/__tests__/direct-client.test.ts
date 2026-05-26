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

  it("连接阶段被取消时关闭半开 WebSocket，避免后台标签页残留 relay client", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      routeReadyDelayMs: 200,
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000317");
    const controller = new AbortController();
    const connect = DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
      timeoutMs: 3000,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
      signal: controller.signal,
    });

    controller.abort();

    await expect(connect).rejects.toMatchObject({ code: "connection_closed" });
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

  it("terminal.create 使用当前连接的请求超时", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      sessionCreateDelayMs: 80,
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000314");
    const pairClient = await connectDevice(device.device_id, 300);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const shortClient = await connectDevice(device.device_id, 30);
    await shortClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    await expect(shortClient.createSession([], { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })).rejects.toMatchObject({
      code: "response_timeout",
    });
    shortClient.close();

    const longClient = await connectDevice(device.device_id, 300);
    await longClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const created = await longClient.createSession([], { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 });
    longClient.close();

    expect(created.session_id).toMatch(/^00000000-0000-0000-0000-/);
    expect(daemon.createdCommands).toEqual([[], []]);
  });

  it("连接认证长预算不会放大普通请求短超时", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000318");
    const pairClient = await connectDevice(device.device_id, 300);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    daemon.delayNextRouteReady(80);
    const client = await DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
      timeoutMs: 300,
      requestTimeoutMs: 30,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
    });
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    // 中文注释：relay route/E2EE/auth 可以比普通 RPC 慢；但连接成功后，
    // session.list 仍要按 UI 的短请求预算失败，不能被长握手预算拖住。
    daemon.queueSessionListResponse(
      [
        {
          session_id: "00000000-0000-0000-0000-000000000301",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      80,
    );
    await expect(client.listSessions()).rejects.toMatchObject({ code: "response_timeout" });

    // 中文注释：普通 RPC timeout 是 UI deadline，不是连接失败。
    // 迟到的 session.list response 会被 request id 丢弃；同一 WebSocket 后续 RPC 仍可继续。
    await new Promise((resolve) => setTimeout(resolve, 90));
    await expect(client.getDaemonStatus()).resolves.toMatchObject({ host_name: "mock-daemon" });
    expect(daemon.activeConnectionCount()).toBe(1);
    client.close();
  });

  it("terminal attach 可以使用长于普通 RPC 的 stream 超时", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000301",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachDelayMs: 80,
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000320");
    const pairClient = await connectDevice(device.device_id, 300);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
      timeoutMs: 300,
      requestTimeoutMs: 30,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
    });
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await client.listSessions();
    await expect(client.attachSession(list.sessions[0].session_id, { timeoutMs: 300 })).resolves.toMatchObject({
      session_id: list.sessions[0].session_id,
    });
    client.close();
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

  it("terminal attach 使用 stream packet，输出和输入带 seq，关闭发送 cancel 但不发送 flow", async () => {
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
    expect(attachOpen).not.toHaveProperty("credit");
    expect(receivedPackets.find((packet) => packet.kind === "flow" && packet.stream_id === streamId)).toBeUndefined();
    expect(receivedPackets.find((packet) => packet.kind === "cancel" && packet.stream_id === streamId)).toBeTruthy();
  });

  it("取消 terminal stream 不关闭工作台 WebSocket，后续 RPC 继续可用", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000319");
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
    await client.receiveInner();
    client.detachSession(attached.session_id);
    await new Promise((resolve) => setTimeout(resolve, 20));

    // 中文注释：session 切换只取消 terminal stream；同一条 WebSocket 仍要承载状态/RPC。
    expect(daemon.activeConnectionCount()).toBe(1);
    await expect(client.getDaemonStatus()).resolves.toMatchObject({ host_name: "mock-daemon" });
    await expect(client.sendSessionData(attached.session_id, new TextEncoder().encode("after-detach"))).rejects.toMatchObject({
      code: "invalid_state",
    });
    client.close();

    const cancelPacket = daemon.receivedPackets.find(
      (packet) => packet.kind === "cancel" && packet.stream_id === daemon.receivedPackets.find(
        (candidate) => candidate.kind === "stream_open" && candidate.method === "terminal.attach",
      )?.stream_id,
    );
    expect(cancelPacket).toBeTruthy();
  });

  it("切换 session 后丢弃已取消 terminal stream 的排队输出", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000321",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
        {
          session_id: "00000000-0000-0000-0000-000000000322",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000323");
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
    const firstSessionId = list.sessions[0].session_id;
    const secondSessionId = list.sessions[1].session_id;
    await client.attachSession(firstSessionId);
    await client.receiveInner();
    daemon.pushTerminalFrame(firstSessionId, {
      kind: "output",
      session_id: firstSessionId,
      terminal_seq: 1,
      data_base64: "c3RhbGUtc3RyZWFtLW91dHB1dAo=",
    });
    // 中文注释：模拟旧 stream 的大输出已经进入 DirectClient 待消费队列，
    // 用户随后切到另一个 session；这些旧输出不能继续挡在新 session 前面。
    await new Promise((resolve) => setTimeout(resolve, 20));
    client.detachSession(firstSessionId);

    await client.attachSession(secondSessionId);
    const output = await client.receiveInner();
    client.close();

    expect(output.payload).toMatchObject({ session_id: secondSessionId });
  });

  it("二进制模式下 terminal stream packet 使用 WebSocket binary 和 raw bytes", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000316");
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
    await client.sendSessionData(attached.session_id, new TextEncoder().encode("stream-input"));
    client.close();
    await new Promise((resolve) => setTimeout(resolve, 20));

    const binaryWireFrames = (
      daemon as unknown as {
        binaryWireFrames?: Array<{ direction: "in" | "out"; byteLength: number }>;
      }
    ).binaryWireFrames ?? [];
    const binaryPackets = (
      daemon as unknown as {
        binaryPacketLog?: Array<{
          direction: "in" | "out";
          kind: string;
          payload_type?: string;
          data_text?: string;
        }>;
      }
    ).binaryPacketLog ?? [];

    expect(output.type).toBe("session_data");
    expect(binaryWireFrames.some((frame) => frame.direction === "in" && frame.byteLength > 0)).toBe(true);
    expect(binaryWireFrames.some((frame) => frame.direction === "out" && frame.byteLength > 0)).toBe(true);
    expect(
      binaryPackets.some(
        (packet) =>
          packet.direction === "in" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "session_data" &&
          packet.data_text === "stream-input",
      ),
    ).toBe(true);
    expect(
      binaryPackets.some(
        (packet) =>
          packet.direction === "out" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "session_data" &&
          packet.data_text === "termd-e2e-ready\n",
      ),
    ).toBe(true);
    expect(daemon.outerWireText()).not.toContain("data_base64");
  });

  it("二进制模式下单个 terminal_frame output 不会退化成 session_data", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000320");
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
    await client.receiveInner();
    daemon.pushTerminalFrame(attached.session_id, {
      kind: "output",
      session_id: attached.session_id,
      terminal_seq: 9,
      data_base64: "c2luZ2xlLWZyYW1lCg==",
    });

    const frame = await client.receiveInner();
    client.close();

    expect(frame).toMatchObject({
      type: "terminal_frame",
      payload: {
        kind: "output",
        terminal_seq: 9,
        data_bytes: expect.any(Uint8Array),
      },
    });
    expect(new TextDecoder().decode((frame as { payload: { data_bytes?: Uint8Array } }).payload.data_bytes)).toBe(
      "single-frame\n",
    );
    expect(
      daemon.binaryPacketLog.some(
        (packet) =>
          packet.direction === "out" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "terminal_frame",
      ),
    ).toBe(true);
  });

  it("terminal stream batch 会展开为多个 terminal_frame，且不携带 render_credit", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000315");
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
    await client.receiveInner();
    daemon.pushTerminalFrameBatch(attached.session_id, [
      {
        kind: "output",
        session_id: attached.session_id,
        terminal_seq: 1,
        data_base64: "YWJjZA==",
      },
      {
        kind: "output",
        session_id: attached.session_id,
        terminal_seq: 2,
        data_base64: "ZWZnaGlq",
      },
    ]);

    const first = await client.receiveInner();
    const second = await client.receiveInner();
    const batchTransportSeq = (first.payload as { transport_seq: number }).transport_seq;
    client.close();
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(first).toMatchObject({
      type: "terminal_frame",
      payload: { kind: "output", terminal_seq: 1, transport_seq: batchTransportSeq },
    });
    expect(second).toMatchObject({
      type: "terminal_frame",
      payload: { kind: "output", terminal_seq: 2, transport_seq: batchTransportSeq },
    });
    expect(first.payload).not.toHaveProperty("render_credit");
    expect(second.payload).not.toHaveProperty("render_credit");

    const streamId = (
      daemon as unknown as {
        receivedPackets?: Array<{ kind: string; method?: string; stream_id?: string }>;
      }
    ).receivedPackets?.find((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach")?.stream_id;
    const flows = (
      daemon as unknown as {
        receivedPackets?: Array<{ kind: string; stream_id?: string; ack?: number; credit?: number }>;
      }
    ).receivedPackets?.filter((packet) => packet.kind === "flow" && packet.stream_id === streamId && packet.ack === batchTransportSeq) ?? [];
    expect(flows).toHaveLength(0);
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
