import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DirectClient, ProtocolClientError, __directClientTestInternals } from "../protocol/direct-client";
import {
  decodeEd25519PublicKey,
  generateDeviceIdentity,
  httpE2eeSigningInputBytes,
  verifyEd25519Signature,
} from "../protocol/auth";
import { E2eeSession, decodeBinaryEncryptedFrame, encodeBinaryEncryptedFrame, type E2eeKeyPair } from "../protocol/e2ee";
import { decodeSupervisorTerminalServerFrame } from "../protocol/supervisor-terminal";
import { PROTOCOL_PACKET_VERSION } from "../protocol/types";
import type {
  AttachFramePayload,
  HttpE2eeAuthPayload,
  PublicKeyWire,
  SessionFileDownloadStreamReadyPayload,
  SessionFileHttpUploadStreamPayload,
  SessionFileHttpUploadReadyPayload,
  SessionFileUploadProgressPayload,
} from "../protocol/types";
import { concatBytes, encodeUtf8 } from "../protocol/wire";
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
      pingDelayMs: 200,
    });
  });

  afterEach(async () => {
    await daemon.stop();
  });

  function connectDevice(deviceId: string, timeoutMs = 3000, url = daemon.url): Promise<DirectClient> {
    return DirectClient.connect(url, daemon.serverId, deviceId, {
      timeoutMs,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
    });
  }

  async function pairedDevice(deviceId: string) {
    const device = await generateDeviceIdentity(deviceId);
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();
    return {
      device,
      server: {
        server_id: accepted.server_id,
        daemon_public_key: accepted.daemon_public_key,
        url: daemon.url,
        paired_at_ms: 1710000000000,
      },
    };
  }

  async function authenticatedClient(deviceId: string): Promise<{ client: DirectClient; device: Awaited<ReturnType<typeof generateDeviceIdentity>>; server: Awaited<ReturnType<typeof pairedDevice>>["server"] }> {
    const { device, server } = await pairedDevice(deviceId);
    const client = await connectDevice(device.device_id);
    await client.authenticate(device, server);
    return { client, device, server };
  }

  function settleWithin<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
    return Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
      }),
    ]);
  }

  function waitUntil(predicate: () => boolean, label: string, timeoutMs = 500): Promise<void> {
    return settleWithin(new Promise<void>((resolve) => {
      const tick = () => {
        if (predicate()) {
          resolve();
          return;
        }
        setTimeout(tick, 5);
      };
      tick();
    }), timeoutMs, label);
  }

  async function receiveUntilType(client: DirectClient, expectedType: string): Promise<{ type: string; payload: unknown }> {
    while (true) {
      const envelope = await client.receiveInner();
      if (envelope.type === expectedType) {
        return envelope as { type: string; payload: unknown };
      }
    }
  }

  function httpE2eeSessionFromHeaders(headers: Headers): E2eeSession {
    const deviceId = headers.get("x-termd-device-id");
    const devicePublicKey = headers.get("x-termd-e2ee-public-key");
    if (!deviceId || !devicePublicKey) {
      throw new Error("missing HTTP E2EE test headers");
    }
    const daemonKeypair = (daemon as unknown as { e2eeKeypair: E2eeKeyPair }).e2eeKeypair;
    return E2eeSession.daemon({
      serverId: daemon.serverId,
      deviceId,
      localKeypair: daemonKeypair,
      devicePublicKeyWire: devicePublicKey as PublicKeyWire,
    });
  }

  function encodeHttpE2eeTestFrames(e2ee: E2eeSession, frames: Uint8Array[]): Uint8Array {
    return concatBytes(
      ...frames.map((frame) => {
        const encrypted = encodeBinaryEncryptedFrame(e2ee.encryptBinary(frame));
        const wire = new Uint8Array(4 + encrypted.byteLength);
        new DataView(wire.buffer, wire.byteOffset, 4).setUint32(0, encrypted.byteLength, false);
        wire.set(encrypted, 4);
        return wire;
      }),
    );
  }

  function decodeHttpE2eeTestFrames(e2ee: E2eeSession, wire: Uint8Array): Uint8Array[] {
    const frames: Uint8Array[] = [];
    let offset = 0;
    while (offset < wire.byteLength) {
      const len = new DataView(wire.buffer, wire.byteOffset + offset, 4).getUint32(0, false);
      offset += 4;
      const encrypted = decodeBinaryEncryptedFrame(wire.slice(offset, offset + len));
      frames.push(e2ee.decryptBinary(encrypted));
      offset += len;
    }
    return frames;
  }

  function httpE2eeRawFrameLengths(wire: Uint8Array): number[] {
    const lengths: number[] = [];
    let offset = 0;
    while (offset < wire.byteLength) {
      const len = new DataView(wire.buffer, wire.byteOffset + offset, 4).getUint32(0, false);
      lengths.push(len);
      offset += 4 + len;
    }
    expect(offset).toBe(wire.byteLength);
    return lengths;
  }

  async function requestBodyBytes(body: BodyInit | null | undefined): Promise<Uint8Array> {
    if (!body) {
      throw new Error("missing request body");
    }
    if (body instanceof ReadableStream) {
      throw new Error("upload body must not be a ReadableStream");
    }
    if (body instanceof Blob) {
      if ("arrayBuffer" in body && typeof body.arrayBuffer === "function") {
        return new Uint8Array(await body.arrayBuffer());
      }
      return await new Promise<Uint8Array>((resolve, reject) => {
        const reader = new FileReader();
        reader.onerror = () => reject(reader.error ?? new Error("failed to read blob"));
        reader.onload = () => resolve(new Uint8Array(reader.result as ArrayBuffer));
        reader.readAsArrayBuffer(body);
      });
    }
    if (body instanceof ArrayBuffer || Object.prototype.toString.call(body) === "[object ArrayBuffer]") {
      return new Uint8Array(body as ArrayBuffer);
    }
    if (ArrayBuffer.isView(body)) {
      const view = body as ArrayBufferView;
      return new Uint8Array(view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength));
    }
    return encodeUtf8(String(body));
  }

  function responseBodyBytes(bytes: Uint8Array): ArrayBuffer {
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
  }

  function testLargeUploadChunkMarker(offsetBytes: number): number {
    // 中文注释：大文件分片测试用小 Blob 标记块模拟真实 10MiB 分片，避免单测占用大量内存。
    return Math.floor(offsetBytes / (10 * 1024 * 1024));
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

  it("pairing 成功后同一连接可以继续执行 authenticated session RPC", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000333");
    const client = await connectDevice(device.device_id);

    await client.pair("secret-token", device.device_public_key);
    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id);
    client.close();

    expect(list.sessions).toHaveLength(1);
    expect(attached.session_id).toBe(list.sessions[0].session_id);
  });

  it("pairing 成功并提供完整 device 后，同一连接的 session.list 走 HTTP 控制面", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000344");
    const client = await connectDevice(device.device_id);

    await client.pair("secret-token", device.device_public_key, device);
    const list = await client.listSessions();

    expect(list.sessions).toHaveLength(1);
    expect(daemon.receivedHttpRequests.some((request) => request.path === "/api/control/session/list")).toBe(true);
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.list")).toBe(false);
    client.close();
  });

  it("authenticate 成功后会申请并缓存短期 session token", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000334");

    expect(daemon.receivedPackets.some((packet) => packet.method === "auth.session_token")).toBe(true);
    expect(client.getSessionToken()).toMatchObject({
      token: expect.any(String),
      expires_at_ms: expect.any(Number),
    });

    client.close();
  });

  it("attachSessionPermission 成功后会缓存 session scope token", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000337");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSessionPermission(sessionId);

    expect(client.getSessionScope(sessionId)).toMatchObject({
      token: expect.any(String),
      expires_at_ms: expect.any(Number),
    });
    expect(daemon.receivedHttpRequests.at(-1)).toMatchObject({
      path: "/api/control/session/attach",
      method: "POST",
      payload: { session_id: sessionId },
    });
    client.close();
  });

  it("HTTP closeSession 遇到未归属 error 帧时仍会保留当前 close ack", async () => {
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
      closeSessionUnownedError: {
        code: "connection_closed",
        message: "terminal stream closed during session shutdown",
      },
    });
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000343");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSessionPermission(sessionId);
    await receiveUntilType(client, "session_scope_grant");
    await expect(client.closeSession(sessionId)).resolves.toMatchObject({
      session_id: sessionId,
    });
    await expect(client.receiveInner()).rejects.toMatchObject({
      code: "connection_closed",
      message: "terminal stream closed during session shutdown",
    });

    client.close();
  });

  it("session token 过期后会在下一次 HTTP control 请求前自动刷新", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000341");
    const initialSessionTokenRequests = daemon.receivedPackets.filter((packet) => packet.method === "auth.session_token").length;

    const tokenState = client.getSessionToken() as { token: string; expires_at_ms: number };
    tokenState.expires_at_ms = 0;
    await client.listSessions();

    const sessionTokenRequests = daemon.receivedPackets.filter((packet) => packet.method === "auth.session_token");
    expect(sessionTokenRequests).toHaveLength(initialSessionTokenRequests + 1);
    client.close();
  });

  it("daemon 直接拒绝过期 bearer 时会刷新 session token 并重试 HTTP control", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000346");
    const issuedToken = client.getSessionToken() as { token: string; expires_at_ms: number };
    daemon.expireSessionToken(issuedToken.token, 0);

    const list = await client.listSessions();

    expect(list.sessions).toHaveLength(1);
    const sessionTokenRequests = daemon.receivedPackets.filter((packet) => packet.method === "auth.session_token");
    expect(sessionTokenRequests.length).toBeGreaterThanOrEqual(2);
    expect(daemon.receivedHttpRequests.some((request) => request.path === "/api/control/session/list")).toBe(true);
    client.close();
  });

  it("session scope 过期后会在下一次 session HTTP control 请求前自动续期", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000342");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSessionPermission(sessionId);
    const scope = client.getSessionScope(sessionId) as { token: string; expires_at_ms: number };
    scope.expires_at_ms = 0;

    await client.listSessionFiles(sessionId);

    const attachHttpRequests = daemon.receivedHttpRequests.filter((request) => request.path === "/api/control/session/attach");
    expect(attachHttpRequests).toHaveLength(2);
    client.close();
  });

  it("daemon 直接拒绝本地仍认为有效的 session scope 时会重新 attach 并重试 session HTTP control", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000347");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSessionPermission(sessionId);
    const scope = client.getSessionScope(sessionId) as { token: string; expires_at_ms: number };
    daemon.expireSessionScope(scope.token, 0);

    const files = await client.listSessionFiles(sessionId);

    expect(files.session_id).toBe(sessionId);
    const attachHttpRequests = daemon.receivedHttpRequests.filter((request) => request.path === "/api/control/session/attach");
    expect(attachHttpRequests).toHaveLength(2);
    expect(daemon.sessionFileRequests.at(-1)).toMatchObject({ session_id: sessionId });
    client.close();
  });

  it("session.list 和 daemon.status 认证后走 HTTP 控制面", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000338");

    const list = await client.listSessions();
    const status = await client.getDaemonStatus();

    expect(list.sessions).toHaveLength(1);
    expect(status.host_name).toBe("mock-daemon");
    expect(daemon.receivedHttpRequests.map((request) => request.path)).toEqual([
      "/api/control/session/list",
      "/api/control/daemon/status",
    ]);
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.list")).toBe(false);
    expect(daemon.receivedPackets.some((packet) => packet.method === "daemon.status")).toBe(false);
    client.close();
  });

  it("session.files 走 HTTP 控制面并携带 session scope token", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000339");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);

    const files = await client.listSessionFiles(sessionId);

    expect(files.session_id).toBe(sessionId);
    expect(daemon.receivedHttpRequests.at(-1)).toMatchObject({
      path: `/api/control/session/${sessionId}/files`,
      method: "POST",
      payload: { session_id: sessionId },
    });
    expect(daemon.sessionFileRequests.at(-1)).toMatchObject({ session_id: sessionId });
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.files")).toBe(false);
    client.close();
  });

  it("wrong phase calls fail locally without sending protocol requests", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000330");
    const unauthenticated = await connectDevice(device.device_id);
    const originalFetch = globalThis.fetch;
    let fetchCalls = 0;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async () => {
      fetchCalls += 1;
      return new Response(null, { status: 500 });
    }) as typeof fetch;

    try {
      await expect(unauthenticated.listSessions()).rejects.toMatchObject({ code: "invalid_state" });
      await expect(unauthenticated.attachSession(missingSessionId)).rejects.toMatchObject({ code: "invalid_state" });
      await expect(unauthenticated.request("client.hello", { name: "too early" })).rejects.toMatchObject({ code: "invalid_state" });
      await expect(unauthenticated.uploadSessionFile(missingSessionId, "/tmp/bad.bin", new File([new Uint8Array([1])], "bad.bin"))).rejects.toMatchObject({ code: "invalid_state" });
      await expect(unauthenticated.downloadSessionFile(missingSessionId, "/tmp/bad.bin")).rejects.toMatchObject({ code: "invalid_state" });
      expect(daemon.receivedPackets.some((packet) => packet.method === "session.list")).toBe(false);
      expect(daemon.receivedPackets.some((packet) => packet.method === "terminal.attach")).toBe(false);
      expect(daemon.receivedPackets.some((packet) => packet.method === "client.hello")).toBe(false);
      expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_upload")).toBe(false);
      expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_download")).toBe(false);
      expect(fetchCalls).toBe(0);
      unauthenticated.close();

      const { client, device: paired, server } = await authenticatedClient("00000000-0000-0000-0000-000000000331");
      await expect(client.pair("secret-token", paired.device_public_key)).rejects.toMatchObject({ code: "invalid_state" });
      await expect(client.authenticate(paired, server)).rejects.toMatchObject({ code: "invalid_state" });
      await expect(client.sendSessionData(missingSessionId, new TextEncoder().encode("input"))).rejects.toMatchObject({ code: "invalid_state" });
      await expect(client.sendSessionCursor(missingSessionId, { row: 1, col: 2, focused: true })).rejects.toMatchObject({ code: "invalid_state" });
      await expect(client.resizeSession(missingSessionId, { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })).rejects.toMatchObject({ code: "invalid_state" });
      client.close();
      await expect(client.uploadSessionFile(missingSessionId, "/tmp/closed.bin", new File([new Uint8Array([1])], "closed.bin"))).rejects.toMatchObject({ code: "connection_closed" });
      await expect(client.downloadSessionFile(missingSessionId, "/tmp/closed.bin")).rejects.toMatchObject({ code: "connection_closed" });
      expect(fetchCalls).toBe(0);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      unauthenticated.close();
    }
  });

  it("unowned packet error reaches the UI-facing error queue", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000332");

    daemon.sendUnownedPacketError("daemon_warning", "daemon sent an unowned error");

    await expect(client.receiveInner()).rejects.toMatchObject({
      code: "daemon_warning",
      message: "daemon sent an unowned error",
    });
    client.close();
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
    const output = await receiveUntilType(client, "attach_frame");
    const grant = await client.requestControl(attached.session_id);
    client.close();

    expect(list.sessions).toHaveLength(1);
    expect(attached.role).toBe("operator");
    expect(output.type).toBe("attach_frame");
    expect(grant.device_id).toBe(device.device_id);
    expect(daemon.decryptedInputs).toContain("terminal-secret");
    expect(daemon.sessionCursorUpdates).toContainEqual({ session_id: attached.session_id, row: 12, col: 8, focused: true });
    expect(daemon.outerWireText()).not.toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("session_data");
    expect(daemon.outerWireText()).not.toContain("session_cursor");
  });

  it("terminal.create 默认使用当前连接超时，也支持终端级超时覆盖", async () => {
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

    const overrideClient = await connectDevice(device.device_id, 30);
    await overrideClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const overrideCreated = await overrideClient.createSession(
      [],
      { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      { timeoutMs: 300 },
    );
    overrideClient.close();

    const longClient = await connectDevice(device.device_id, 300);
    await longClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const created = await longClient.createSession([], { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 });
    longClient.close();

    expect(overrideCreated.session_id).toMatch(/^00000000-0000-0000-0000-/);
    expect(created.session_id).toMatch(/^00000000-0000-0000-0000-/);
    expect(daemon.createdCommands).toEqual([[], [], []]);
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

  it("pending terminal attach does not allow terminal operations before attach response", async () => {
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
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000334");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    const attach = client.attachSession(sessionId, { timeoutMs: 300 });

    await expect(client.sendSessionData(sessionId, new TextEncoder().encode("too-early"))).rejects.toMatchObject({ code: "invalid_state" });
    await expect(client.sendSessionCursor(sessionId, { row: 1, col: 1, focused: true })).rejects.toMatchObject({ code: "invalid_state" });
    await expect(client.resizeSession(sessionId, { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })).rejects.toMatchObject({ code: "invalid_state" });
    await expect(attach).resolves.toMatchObject({ session_id: sessionId });
    await expect(client.sendSessionData(sessionId, new TextEncoder().encode("after-attach"))).resolves.toBeUndefined();
    client.close();
  });

  it("attachSession 的 abort signal 会中断挂起的 terminal attach", async () => {
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
      attachDelayMs: 200,
    });
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000336");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;
    const controller = new AbortController();

    const attach = client.attachSession(sessionId, {
      timeoutMs: 1000,
      signal: controller.signal,
    });

    controller.abort();

    await expect(attach).rejects.toMatchObject({ code: "connection_closed" });
    client.close();
  });

  it("packet dispatcher 按 request id 归属响应和错误，错误不会污染并发请求", async () => {
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
      pingDelayMs: 40,
      attachOutput: "termd-e2e-ready\n",
    });
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

    const pingPromise = client.sendPing();
    const closePromise = client.attachSession(missingSessionId, { timeoutMs: 500 });

    await expect(closePromise).rejects.toMatchObject({ code: "session_not_found" });
    await expect(pingPromise).resolves.toBeUndefined();
    client.close();

    const receivedPackets = (
      daemon as unknown as {
        receivedPackets?: Array<{ id?: string; kind: string; method?: string; payload: unknown }>;
      }
    ).receivedPackets ?? [];
    const closePacket = receivedPackets.find((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach");
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
    const output = await receiveUntilType(client, "attach_frame");
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
    expect(output.type).toBe("attach_frame");
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

  it("取消 terminal stream 不关闭当前 WebSocket，后续 RPC 继续可用", async () => {
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
    await receiveUntilType(client, "attach_frame");
    client.detachSession(attached.session_id);
    await new Promise((resolve) => setTimeout(resolve, 20));

    // 中文注释：DirectClient 内部取消 terminal stream 时，同一条 WebSocket 仍要承载状态/RPC。
    // App 层的 session 切换会额外关闭 DirectClient，以 WebSocket 生命周期清理旧 client context。
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

  it("关闭 session 后其他已 attach 连接也不能继续使用旧的 session 权限", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000334");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const firstClient = await connectDevice(device.device_id);
    await firstClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const firstList = await firstClient.listSessions();
    await firstClient.attachSessionPermission(firstList.sessions[0].session_id);
    const secondClient = await connectDevice(device.device_id);
    await secondClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const list = await firstClient.listSessions();
    const sessionId = list.sessions[0].session_id;
    await firstClient.attachSession(sessionId);
    await secondClient.attachSessionPermission(sessionId);

    await firstClient.closeSession(sessionId);
    // 中文注释：session scope 失效后，客户端会先尝试重新 attach 刷新 scope；
    // 若 session 已被远端真正删除，最终暴露的应是 session_not_found，而不是中间态 auth_failed。
    await expect(secondClient.listSessionFiles(sessionId)).rejects.toMatchObject({ code: "session_not_found" });
    await expect(secondClient.attachSession(sessionId)).rejects.toMatchObject({ code: "session_not_found" });

    firstClient.close();
    secondClient.close();
  });

  it("attach 挂起期间若 session 被其他连接关闭，迟到的 attach 返回 session_not_found", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000335",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachDelayMs: 80,
      attachOutput: "termd-e2e-ready\n",
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000336");
    const pairClient = await connectDevice(device.device_id);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const firstClient = await connectDevice(device.device_id);
    await firstClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const firstList = await firstClient.listSessions();
    await firstClient.attachSessionPermission(firstList.sessions[0].session_id);
    const secondClient = await connectDevice(device.device_id);
    await secondClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });

    const sessionId = firstList.sessions[0].session_id;
    const pendingAttach = secondClient.attachSession(sessionId, { timeoutMs: 500 });
    await waitUntil(() => daemon.attachRequests.some((request) => request.session_id === sessionId), "pending attach request");

    await firstClient.closeSession(sessionId);

    await expect(pendingAttach).rejects.toMatchObject({ code: "session_not_found" });
    await expect(secondClient.listSessionFiles(sessionId)).rejects.toMatchObject({ code: "session_not_found" });

    firstClient.close();
    secondClient.close();
  });

  it("HTTP closeSession 成功后会立刻清理本地 terminal stream 和 session 权限", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000340");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSession(sessionId);
    await receiveUntilType(client, "attach_frame");
    await client.closeSession(sessionId);

    await expect(client.sendSessionData(sessionId, new TextEncoder().encode("after-close"))).rejects.toMatchObject({
      code: "invalid_state",
    });
    await expect(client.listSessionFiles(sessionId)).rejects.toMatchObject({
      code: "session_not_found",
    });
    client.close();
  });

  it("收到远端 session_closed 事件后会清理本地 terminal stream 状态", async () => {
    const { client } = await authenticatedClient("00000000-0000-0000-0000-000000000344");
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;

    await client.attachSession(sessionId);
    await receiveUntilType(client, "attach_frame");
    daemon.sendSessionClosed(sessionId);
    await waitUntil(
      () => client.getSessionScope(sessionId) === undefined,
      "session scope cleared after remote close",
    );

    await expect(client.sendSessionData(sessionId, new TextEncoder().encode("after-remote-close"))).rejects.toMatchObject({
      code: "invalid_state",
    });
    expect(client.getSessionScope(sessionId)).toBeUndefined();

    client.close();
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
    await receiveUntilType(client, "attach_frame");
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
    const output = await receiveUntilType(client, "attach_frame");
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
    const output = await receiveUntilType(client, "attach_frame");
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

    expect(output.type).toBe("attach_frame");
    expect(binaryWireFrames.some((frame) => frame.direction === "in" && frame.byteLength > 0)).toBe(true);
    expect(binaryWireFrames.some((frame) => frame.direction === "out" && frame.byteLength > 0)).toBe(true);
    expect(
      binaryPackets.some(
        (packet) =>
          packet.direction === "in" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "attach_frame" &&
          packet.data_text === "stream-input",
      ),
    ).toBe(true);
    expect(
      binaryPackets.some(
        (packet) =>
          packet.direction === "out" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "attach_frame" &&
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
    await receiveUntilType(client, "attach_frame");
    daemon.pushTerminalFrame(attached.session_id, {
      kind: "output",
      session_id: attached.session_id,
      terminal_seq: 9,
      data_base64: "c2luZ2xlLWZyYW1lCg==",
    });

    const frame = await receiveUntilType(client, "attach_frame");
    client.close();
    expect(frame).toMatchObject({
      type: "attach_frame",
      payload: {
        session_id: attached.session_id,
        transport_seq: expect.any(Number),
        data_bytes: expect.any(Uint8Array),
      },
    });
    const decoded = decodeSupervisorTerminalServerFrame(
      (frame.payload as AttachFramePayload).data_bytes ?? new Uint8Array(),
    );

    expect(decoded).toMatchObject({
      type: "terminal_frame",
      frame: {
        kind: "output",
        terminal_seq: 9,
        data_bytes: expect.any(Uint8Array),
      },
    });
    if (decoded.type !== "terminal_frame" || decoded.frame.kind !== "output") {
      throw new Error("expected supervisor terminal_frame output");
    }
    expect(new TextDecoder().decode(decoded.frame.data_bytes)).toBe("single-frame\n");
    expect(
      daemon.binaryPacketLog.some(
        (packet) =>
          packet.direction === "out" &&
          packet.kind === "stream_chunk" &&
          packet.payload_type === "attach_frame",
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
    await receiveUntilType(client, "attach_frame");
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

    const first = await receiveUntilType(client, "attach_frame");
    const second = await receiveUntilType(client, "attach_frame");
    const firstTransportSeq = (first.payload as { transport_seq: number }).transport_seq;
    const secondTransportSeq = (second.payload as { transport_seq: number }).transport_seq;
    client.close();
    await new Promise((resolve) => setTimeout(resolve, 20));
    const firstDecoded = decodeSupervisorTerminalServerFrame(
      (first.payload as AttachFramePayload).data_bytes ?? new Uint8Array(),
    );
    const secondDecoded = decodeSupervisorTerminalServerFrame(
      (second.payload as AttachFramePayload).data_bytes ?? new Uint8Array(),
    );

    expect(first).toMatchObject({
      type: "attach_frame",
      payload: {
        transport_seq: firstTransportSeq,
        data_bytes: expect.any(Uint8Array),
      },
    });
    expect(second).toMatchObject({
      type: "attach_frame",
      payload: {
        transport_seq: secondTransportSeq,
        data_bytes: expect.any(Uint8Array),
      },
    });
    expect(secondTransportSeq).toBeGreaterThan(firstTransportSeq);
    expect(firstDecoded).toMatchObject({
      type: "terminal_frame",
      frame: {
        kind: "output",
        terminal_seq: 1,
        data_bytes: expect.any(Uint8Array),
      },
    });
    expect(secondDecoded).toMatchObject({
      type: "terminal_frame",
      frame: {
        kind: "output",
        terminal_seq: 2,
        data_bytes: expect.any(Uint8Array),
      },
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
    ).receivedPackets?.filter((packet) => packet.kind === "flow" && packet.stream_id === streamId) ?? [];
    expect(flows).toHaveLength(0);
  });

  it("SocketInbox 在积压消息后收到 close 时会先清空队列再暴露连接关闭", async () => {
    const socket = new EventTarget() as WebSocket;
    (socket as unknown as { readyState: number }).readyState = WebSocket.OPEN;
    const inbox = new __directClientTestInternals.SocketInbox(socket);

    socket.dispatchEvent(new MessageEvent("message", {
      data: JSON.stringify({ type: "hello", payload: { protocol_version: PROTOCOL_PACKET_VERSION } }),
    }));
    await new Promise((resolve) => setTimeout(resolve, 0));
    (socket as unknown as { readyState: number }).readyState = WebSocket.CLOSED;
    socket.dispatchEvent(new Event("close"));

    // 中文注释：复现 receive pump 处理 backlog 期间没有 read waiter 的窗口；
    // close 必须被记住，并且不能抢掉已经进入队列的消息。
    await expect(inbox.read()).resolves.toMatchObject({ envelope: { type: "hello" } });
    await expect(inbox.read()).rejects.toMatchObject({ code: "connection_closed" });
  });

  it.each([
    { transportEvent: "close" as const, expectedCode: "connection_closed" },
    { transportEvent: "error" as const, expectedCode: "connection_error" },
  ])("WebSocket $transportEvent 在 receive pump yield 窗口会关闭 pending RPC", async ({ transportEvent, expectedCode }) => {
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
      attachOutput: "termd-e2e-ready\n",
      pingDelayMs: 200,
    });
    type WsEmitter = WebSocket & { emit: (event: "close" | "error", ...args: unknown[]) => boolean };
    let capturedSocket: WsEmitter | undefined;
    const { device, server } = await pairedDevice(`00000000-0000-0000-0000-00000000033${transportEvent === "close" ? "5" : "6"}`);
    const client = await DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
      timeoutMs: 3000,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
      webSocketFactory: (url) => {
        const socket = new WebSocket(url) as WsEmitter;
        capturedSocket = socket;
        return socket;
      },
    });
    await client.authenticate(device, server);
    const list = await client.listSessions();
    const attached = await client.attachSession(list.sessions[0].session_id);
    await receiveUntilType(client, "attach_frame");
    const probeError = client.sendPing().then(
      () => undefined,
      (error: unknown) => error,
    );
    let resolveYieldReached!: () => void;
    const yieldReached = new Promise<void>((resolve) => {
      resolveYieldReached = resolve;
    });
    let injected = false;
    __directClientTestInternals.onReceivePumpYield = () => {
      if (injected) {
        return;
      }
      resolveYieldReached();
    };

    try {
      // 中文注释：数量固定在 receive pump 的 yield 阈值上，确保 close 注入时
      // 这批 terminal frame 已经全部被 pump 接收并排队，不再依赖额外的尾部消息。
      for (let index = 0; index < 64; index += 1) {
        daemon.pushTerminalFrame(attached.session_id, {
          kind: "output",
          session_id: attached.session_id,
          terminal_seq: index + 1,
          data_base64: "bGluZQo=",
        });
      }
      await settleWithin(yieldReached, 800, "receive pump yield");
      // 中文注释：在 pump 让出事件循环后、重新进入下一轮读取前注入 close/error，
      // 这样既不会打断前面的 backlog 推送，也能稳定覆盖 pending RPC 关闭路径。
      if (transportEvent === "close") {
        capturedSocket?.close();
      } else {
        capturedSocket?.emit("error", new Error("mock transport error"));
      }
      injected = true;
      await expect(settleWithin(probeError, 800, "ping probe")).resolves.toMatchObject({ code: expectedCode });
      await settleWithin(waitUntil(() => client.isClosed, "client close"), 800, "client close");
      expect(injected).toBe(true);
      expect(client.isClosed).toBe(true);
    } finally {
      __directClientTestInternals.onReceivePumpYield = undefined;
      client.close();
    }
  });

  it.each([
    { transportEvent: "close" as const, expectedCode: "connection_closed" },
    { transportEvent: "error" as const, expectedCode: "connection_error" },
  ])("WebSocket $transportEvent 会唤醒等待中的 binary file upload stream", async ({ transportEvent, expectedCode }) => {
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
      fileUploadProgressDelayMs: 120,
    });
    type WsEmitter = WebSocket & { emit: (event: "close" | "error", ...args: unknown[]) => boolean };
    let controlSocket: WsEmitter | undefined;
    const { device, server } = await pairedDevice(`00000000-0000-0000-0000-00000000034${transportEvent === "close" ? "5" : "6"}`);
    const connectWithCapturedSocket = () => DirectClient.connect(daemon.url, daemon.serverId, device.device_id, {
      timeoutMs: 3000,
      expectedDaemonPublicKey: daemon.daemonPublicKey,
      webSocketFactory: (url) => {
        const socket = new WebSocket(url) as WsEmitter;
        controlSocket = socket;
        return socket;
      },
    });
    const client = await connectWithCapturedSocket();
    await client.authenticate(device, server);
    const sessionOperationClient = await connectWithCapturedSocket();
    await sessionOperationClient.authenticate(device, server);
    const list = await sessionOperationClient.listSessions();
    const sessionId = list.sessions[0].session_id;
    // 中文注释：binary file stream fallback 必须在执行上传的那条连接上完成 attach 权限建立。
    (sessionOperationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedDevice = undefined;
    (sessionOperationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedServer = undefined;
    const uploadError = sessionOperationClient.uploadSessionFile(
      sessionId,
      `/tmp/waiter-${transportEvent}.bin`,
      new File([new Uint8Array([1, 2, 3])], `waiter-${transportEvent}.bin`),
      { timeoutMs: 1000 },
    ).then(
      () => undefined,
      (error: unknown) => error,
    );

    try {
      await waitUntil(() => daemon.sessionFileBinaryWrites.length > 0, "file upload chunk accepted");
      if (transportEvent === "close") {
        controlSocket?.emit("close", 1006, Buffer.from("mock close"));
      } else {
        controlSocket?.emit("error", new Error("mock transport error"));
      }
      await expect(settleWithin(uploadError, 800, "file upload close")).resolves.toMatchObject({ code: expectedCode });
      expect(sessionOperationClient.isClosed).toBe(true);
    } finally {
      sessionOperationClient.close();
      client.close();
    }
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
    const attached = await client.attachSessionPermission(list.sessions[0].session_id);
    const files = await client.listSessionFiles(attached.session_id);
    client.close();

    expect(attached.resize_owner).toBe(false);
    expect(files.session_id).toBe(attached.session_id);
    expect(daemon.attachRequests.at(-1)).toEqual({ session_id: attached.session_id, watch_updates: false });
  });

  it("未使用 HTTP E2EE 时仍可走 binary file stream 并回报提交进度", async () => {
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
    const sessionId = list.sessions[0].session_id;
    const operationClient = await connectDevice(device.device_id);
    await operationClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    const progress: number[] = [];
    const sentProgress: number[] = [];
    const file = new File([new Uint8Array([0, 1, 2, 3, 255])], "raw.bin");
    // 中文注释：兼容路径必须由执行上传的专用连接自己退回到 WebSocket binary stream。
    (operationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedDevice = undefined;
    (operationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedServer = undefined;

    try {
      await operationClient.uploadSessionFile(sessionId, "/tmp/raw.bin", file, {
        onProgress: (update) => progress.push(update.offset_bytes),
        onSentProgress: (sentBytes) => sentProgress.push(sentBytes),
      });
    } finally {
      operationClient.close();
      client.close();
    }

    expect(sentProgress.at(-1)).toBe(file.size);
    expect(progress.at(-1)).toBe(file.size);
    expect(daemon.sessionFileWrites).toHaveLength(0);
    expect(daemon.sessionFileDownloadChunkRequests).toHaveLength(0);
    expect(daemon.binaryPacketLog.some((entry) => entry.direction === "in" && entry.payload_type === "file_chunk")).toBe(true);
  });

  it("legacy RPC upload 不会把超过 RPC cap 的文件整包 base64 发送", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000331");
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
    const sessionId = list.sessions[0].session_id;
    const file = new File([new Uint8Array((1024 * 1024) + 1)], "too-large-legacy.bin");
    try {
      await expect((client as unknown as {
        uploadSessionFileLegacy: (
          sessionId: string,
          path: string,
          file: File,
          options?: Record<string, never>,
        ) => Promise<unknown>;
      }).uploadSessionFileLegacy(sessionId, "/tmp/too-large-legacy.bin", file)).rejects.toMatchObject({
        code: "file_too_large",
      });
    } finally {
      client.close();
    }

    expect(daemon.sessionFileWrites).toHaveLength(0);
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_write")).toBe(false);
  });

  it("HTTP 上传只在 daemon 确认后回报完成进度", async () => {
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
    const sessionId = list.sessions[0].session_id;
    const file = new File([new Uint8Array([1, 2, 3, 4])], "raw.bin");
    const progress: number[] = [];
    const sentProgress: number[] = [];
    const originalFetch = globalThis.fetch;
    let progressCallsBeforeResponse = -1;
    let sentProgressCallsBeforeResponse = -1;
    let uploadBodyBytes = 0;

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/raw.bin",
          upload_id: "mock-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        uploadBodyBytes = (await requestBodyBytes(init?.body)).byteLength;
        // 中文注释：请求体交给 fetch 时，文件还没有得到 daemon 响应确认。
        progressCallsBeforeResponse = progress.length;
        sentProgressCallsBeforeResponse = sentProgress.length;
        const committed = {
          session_id: sessionId,
          path: "/tmp/raw.bin",
          offset_bytes: file.size,
          size_bytes: file.size,
          eof: true,
          modified_at_ms: null,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return new Response(JSON.stringify({ code: "not_found", message: "not found" }), { status: 404 });
    }) as typeof fetch;

    try {
      await client.uploadSessionFile(sessionId, "/tmp/raw.bin", file, {
        onProgress: (update) => progress.push(update.offset_bytes),
        onSentProgress: (sentBytes) => sentProgress.push(sentBytes),
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(uploadBodyBytes).toBeGreaterThan(0);
    expect(progressCallsBeforeResponse).toBe(0);
    expect(sentProgressCallsBeforeResponse).toBeGreaterThan(0);
    expect(sentProgress.at(-1)).toBe(file.size);
    expect(progress).toEqual([file.size]);
  });

  it("HTTP 大文件上传使用 10MiB 分片且最多 2 个 POST 并发", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000332");
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
    const sessionId = list.sessions[0].session_id;
    const fileSize = (10 * 1024 * 1024 * 2) + 17;
    const file = {
      size: fileSize,
      slice: (start: number, end: number) =>
        // 中文注释：测试只关心 offset 分片和并发，不需要真的在 Vitest 里加解密 20MiB。
        // 用小标记块保留 Blob 行为，避免测试 worker 因大数组和 E2EE 副本放大而 OOM。
        new Blob([new Uint8Array([Math.floor(start / (10 * 1024 * 1024)), end - start > 1 ? 1 : 0])]),
    } as unknown as Blob;
    const originalFetch = globalThis.fetch;
    let uploadCalls = 0;
    let activeUploads = 0;
    let maxActiveUploads = 0;
    const received = new Set<number>();

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/large-chunked.bin",
          upload_id: "mock-large-chunked-upload",
          size_bytes: fileSize,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        uploadCalls += 1;
        activeUploads += 1;
        maxActiveUploads = Math.max(maxActiveUploads, activeUploads);
        const wire = await requestBodyBytes(init?.body);
        const frames = decodeHttpE2eeTestFrames(e2ee, wire);
        const meta = JSON.parse(new TextDecoder().decode(frames[0])) as SessionFileHttpUploadStreamPayload;
        expect(meta).toMatchObject({
          session_id: sessionId,
          path: "/tmp/large-chunked.bin",
          upload_id: "mock-large-chunked-upload",
          size_bytes: fileSize,
        });
        const requestBytes = new Uint8Array(frames.slice(1).reduce((sum, frame) => sum + frame.byteLength, 0));
        let offset = meta.offset_bytes;
        for (const frame of frames.slice(1)) {
          requestBytes.set(frame, offset - meta.offset_bytes);
          offset += frame.byteLength;
        }
        expect(Array.from(requestBytes)).toEqual([
          testLargeUploadChunkMarker(meta.offset_bytes),
          1,
        ]);
        received.add(meta.offset_bytes);
        await new Promise((resolve) => setTimeout(resolve, 20));
        activeUploads -= 1;
        const receivedBytes = [...received].reduce((sum, chunkOffset) => {
          if (chunkOffset >= 20 * 1024 * 1024) {
            return sum + 17;
          }
          return sum + 10 * 1024 * 1024;
        }, 0);
        const committed = {
          session_id: sessionId,
          path: "/tmp/large-chunked.bin",
          offset_bytes: receivedBytes,
          size_bytes: fileSize,
          eof: receivedBytes === fileSize,
          modified_at_ms: receivedBytes === fileSize ? null : undefined,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return originalFetch(input, init);
    }) as typeof fetch;

    try {
      const progress = await client.uploadSessionFile(sessionId, "/tmp/large-chunked.bin", file);
      expect(progress.offset_bytes).toBe(fileSize);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(uploadCalls).toBe(3);
    expect(maxActiveUploads).toBe(2);
    expect(daemon.sessionFileBinaryWrites).toHaveLength(0);
  });

  it("HTTP 上传 10MiB 业务分片内部会拆成小于 2MiB 的 E2EE frame", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000333");
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
    const sessionId = list.sessions[0].session_id;
    const payload = new Uint8Array((2 * 1024 * 1024) + 100);
    payload[0] = 7;
    payload[payload.length - 1] = 9;
    const file = new File([payload], "frame-cap.bin");
    const originalFetch = globalThis.fetch;
    let sawSplitUploadBody = false;

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/frame-cap.bin",
          upload_id: "mock-frame-cap-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        const wire = await requestBodyBytes(init?.body);
        const lengths = httpE2eeRawFrameLengths(wire);
        expect(lengths.every((len) => len <= 2 * 1024 * 1024)).toBe(true);
        const frames = decodeHttpE2eeTestFrames(e2ee, wire);
        const uploaded = concatBytes(...frames.slice(1));
        expect(uploaded.byteLength).toBe(file.size);
        expect(uploaded[0]).toBe(7);
        expect(uploaded[uploaded.length - 1]).toBe(9);
        sawSplitUploadBody = frames.length > 2;
        const committed = {
          session_id: sessionId,
          path: "/tmp/frame-cap.bin",
          offset_bytes: file.size,
          size_bytes: file.size,
          eof: true,
          modified_at_ms: null,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return originalFetch(input, init);
    }) as typeof fetch;

    try {
      const progress = await client.uploadSessionFile(sessionId, "/tmp/frame-cap.bin", file);
      expect(progress.eof).toBe(true);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(sawSplitUploadBody).toBe(true);
  });

  it("HTTP 并发上传最终 eof 进度不会被较晚返回的旧 non-eof 响应覆盖", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000334");
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
    const sessionId = list.sessions[0].session_id;
    const fileSize = 20 * 1024 * 1024;
    const file = {
      size: fileSize,
      slice: (start: number) => new Blob([new Uint8Array([testLargeUploadChunkMarker(start)])]),
    } as unknown as Blob;
    const originalFetch = globalThis.fetch;
    const progressOffsets: number[] = [];

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/progress-race.bin",
          upload_id: "mock-progress-race-upload",
          size_bytes: fileSize,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        const frames = decodeHttpE2eeTestFrames(e2ee, await requestBodyBytes(init?.body));
        const meta = JSON.parse(new TextDecoder().decode(frames[0])) as SessionFileHttpUploadStreamPayload;
        if (meta.offset_bytes === 0) {
          await new Promise((resolve) => setTimeout(resolve, 40));
        }
        const committed = {
          session_id: sessionId,
          path: "/tmp/progress-race.bin",
          offset_bytes: meta.offset_bytes === 0 ? 10 * 1024 * 1024 : fileSize,
          size_bytes: fileSize,
          eof: meta.offset_bytes !== 0,
          modified_at_ms: meta.offset_bytes !== 0 ? null : undefined,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return originalFetch(input, init);
    }) as typeof fetch;

    try {
      const progress = await client.uploadSessionFile(sessionId, "/tmp/progress-race.bin", file, {
        onProgress: (update) => progressOffsets.push(update.offset_bytes),
      });
      expect(progress.eof).toBe(true);
      expect(progress.offset_bytes).toBe(fileSize);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(progressOffsets).toEqual([fileSize]);
  });

  it("HTTP 上传已收到 eof 后忽略旧并发分片的取消错误", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000336");
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
    const sessionId = list.sessions[0].session_id;
    const fileSize = 20 * 1024 * 1024;
    const file = {
      size: fileSize,
      slice: (start: number) => new Blob([new Uint8Array([testLargeUploadChunkMarker(start)])]),
    } as unknown as Blob;
    const originalFetch = globalThis.fetch;
    const paths: string[] = [];

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      paths.push(url.pathname);
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/eof-wins.bin",
          upload_id: "mock-eof-wins-upload",
          size_bytes: fileSize,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        const frames = decodeHttpE2eeTestFrames(e2ee, await requestBodyBytes(init?.body));
        const meta = JSON.parse(new TextDecoder().decode(frames[0])) as SessionFileHttpUploadStreamPayload;
        if (meta.offset_bytes === 0) {
          await new Promise((resolve) => setTimeout(resolve, 30));
          throw new TypeError("stale chunk was cancelled");
        }
        const committed = {
          session_id: sessionId,
          path: "/tmp/eof-wins.bin",
          offset_bytes: fileSize,
          size_bytes: fileSize,
          eof: true,
          modified_at_ms: null,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return originalFetch(input, init);
    }) as typeof fetch;

    try {
      const progress = await client.uploadSessionFile(sessionId, "/tmp/eof-wins.bin", file);
      expect(progress.eof).toBe(true);
      expect(progress.offset_bytes).toBe(fileSize);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(paths).toEqual(["/api/files/upload/init", "/api/files/upload", "/api/files/upload"]);
  });

  it("HTTP 上传所有分片结束但没有 eof 时会请求 abort 清理", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000335");
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
    const sessionId = list.sessions[0].session_id;
    const file = new File([new Uint8Array([1, 2, 3])], "missing-eof.bin");
    const originalFetch = globalThis.fetch;
    const paths: string[] = [];

    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      paths.push(url.pathname);
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/missing-eof.bin",
          upload_id: "mock-missing-eof-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload/abort")) {
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify({ ok: true }))])));
      }
      if (url.pathname.endsWith("/api/files/upload")) {
        const committed = {
          session_id: sessionId,
          path: "/tmp/missing-eof.bin",
          offset_bytes: 0,
          size_bytes: file.size,
          eof: false,
          modified_at_ms: undefined,
        } satisfies SessionFileUploadProgressPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(committed))])));
      }
      return originalFetch(input, init);
    }) as typeof fetch;

    try {
      await expect(client.uploadSessionFile(sessionId, "/tmp/missing-eof.bin", file)).rejects.toMatchObject({
        code: "invalid_file_transfer",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(paths).toEqual(["/api/files/upload/init", "/api/files/upload", "/api/files/upload/abort"]);
  });

  it("HTTP 上传请求 TypeError 不回退到 WebSocket 文件流", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000322");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    let fetchCalls = 0;
    const paths: string[] = [];
    const file = new File([new Uint8Array([7, 8])], "fallback.bin");
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      fetchCalls += 1;
      const url = new URL(input instanceof Request ? input.url : String(input));
      paths.push(url.pathname);
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/fallback.bin",
          upload_id: "mock-fallback-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      if (url.pathname.endsWith("/api/files/upload/abort")) {
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify({ ok: true }))])));
      }
      throw new TypeError("ReadableStream request body is not supported");
    }) as typeof fetch;
    try {
      await expect(client.uploadSessionFile(sessionId, "/tmp/fallback.bin", file)).rejects.toThrow(TypeError);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(fetchCalls).toBe(3);
    expect(paths).toEqual(["/api/files/upload/init", "/api/files/upload", "/api/files/upload/abort"]);
    expect(daemon.sessionFileBinaryWrites).toHaveLength(0);
  });

  it("WebSocket 上传必须等待 daemon eof 确认", async () => {
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
      attachOutput: "termd-e2e-ready\n",
      fileUploadProgressOverrides: {
        "/tmp/bad-progress.bin": { eof: false },
      },
    });
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000329");
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
    const sessionId = list.sessions[0].session_id;
    const file = new File([new Uint8Array([9])], "bad-progress.bin");
    const operationClient = await connectDevice(device.device_id);
    await operationClient.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: daemon.url,
      paired_at_ms: 1710000000000,
    });
    (operationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedDevice = undefined;
    (operationClient as unknown as { authenticatedDevice?: unknown; authenticatedServer?: unknown }).authenticatedServer = undefined;
    try {
      await expect(operationClient.uploadSessionFile(sessionId, "/tmp/bad-progress.bin", file)).rejects.toMatchObject({
        code: "invalid_file_transfer",
      });
    } finally {
      operationClient.close();
      client.close();
    }
  });

  it("HTTP 上传初始化阶段网络 TypeError 不回退到 WebSocket 文件流", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000324");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async () => {
      throw new TypeError("Failed to fetch");
    }) as typeof fetch;
    try {
      await expect(
        client.uploadSessionFile(sessionId, "/tmp/no-fallback.bin", new File([new Uint8Array([1])], "no-fallback.bin")),
      ).rejects.toThrow(TypeError);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.sessionFileBinaryWrites).toHaveLength(0);
  });

  it("HTTP 上传初始化返回 relay 未实现状态时仅小文件回退 WebSocket 兼容路径", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000326");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    // 中文注释：termrelay 默认禁用 HTTP 文件隧道时返回 501；小文件必须能退回 WebSocket 兼容上传。
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async () =>
      new Response(null, { status: 501 })) as typeof fetch;
    try {
      await client.uploadSessionFile(sessionId, "/tmp/http-required.bin", new File([new Uint8Array([1])], "http-required.bin"));
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.sessionFileBinaryWrites).toHaveLength(1);
    expect(Array.from(daemon.sessionFileBinaryWrites[0].bytes)).toEqual([1]);
  });

  it("HTTP 上传不支持时大文件不会回退到 WebSocket 兼容路径", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000334");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async () =>
      new Response(null, { status: 426 })) as typeof fetch;
    const largeFile = {
      size: (16 * 1024 * 1024) + 1,
      slice: () => {
        throw new Error("large unsupported HTTP upload should not be read");
      },
    } as unknown as Blob;
    try {
      await expect(client.uploadSessionFile(sessionId, "/tmp/large-http-required.bin", largeFile)).rejects.toMatchObject({
        code: "file_too_large",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.sessionFileBinaryWrites).toHaveLength(0);
  });

  it("HTTP 上传 body 阶段 TypeError 不回退到 WebSocket 文件流", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000325");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    const file = new File([new Uint8Array([2])], "no-stream-fallback.bin");
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/no-stream-fallback.bin",
          upload_id: "mock-no-stream-fallback-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      throw new TypeError("Failed to fetch");
    }) as typeof fetch;
    try {
      await expect(client.uploadSessionFile(sessionId, "/tmp/no-stream-fallback.bin", file)).rejects.toThrow(TypeError);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.sessionFileBinaryWrites).toHaveLength(0);
  });

  it("HTTP 上传 body 端点返回不支持状态时仅小文件回退 WebSocket 兼容路径", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000327");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    const file = new File([new Uint8Array([3])], "http-required-stream.bin");
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      if (url.pathname.endsWith("/api/files/upload/init")) {
        const ready = {
          session_id: sessionId,
          path: "/tmp/http-required-stream.bin",
          upload_id: "mock-http-required-upload",
          size_bytes: file.size,
          offset_bytes: 0,
        } satisfies SessionFileHttpUploadReadyPayload;
        return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))])));
      }
      return new Response(null, { status: 426 });
    }) as typeof fetch;
    try {
      await client.uploadSessionFile(sessionId, "/tmp/http-required-stream.bin", file);
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.sessionFileBinaryWrites).toHaveLength(1);
    expect(Array.from(daemon.sessionFileBinaryWrites[0].bytes)).toEqual([3]);
  });

  it("HTTP 下载首个元数据响应帧使用短超时", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000321");
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
    const sessionId = list.sessions[0].session_id;
    const originalFetch = globalThis.fetch;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = ((_input, init) =>
      new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener("abort", () => {
          reject(new DOMException("Aborted", "AbortError"));
        }, { once: true });
      })) as typeof fetch;
    try {
      await expect(client.downloadSessionFile(sessionId, "/tmp/raw.bin", { timeoutMs: 20 })).rejects.toMatchObject({
        code: "response_timeout",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }
  });

  it("HTTP 下载响应头已返回但元数据首帧缺失时仍使用短超时", async () => {
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
    const sessionId = list.sessions[0].session_id;
    const originalFetch = globalThis.fetch;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = ((_input, init) => {
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          init?.signal?.addEventListener("abort", () => {
            controller.error(new DOMException("Aborted", "AbortError"));
          }, { once: true });
          setTimeout(() => {
            controller.error(new Error("metadata guard reached"));
          }, 100);
        },
      });
      return Promise.resolve(new Response(body, { status: 200 }));
    }) as typeof fetch;
    try {
      await expect(client.downloadSessionFile(sessionId, "/tmp/raw.bin", { timeoutMs: 20 })).rejects.toMatchObject({
        code: "response_timeout",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }
  });

  it("HTTP 文件传输 400 错误不会回退到 WebSocket 文件流", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000318");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);

    const originalFetch = globalThis.fetch;
    const originalSetTimeout = globalThis.setTimeout;
    const setTimeoutDelays: number[] = [];
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (_input, init) => {
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [
        encodeUtf8(JSON.stringify({ code: "invalid_file_transfer", message: "bad request" })),
      ])), {
        status: 400,
        headers: { "content-type": "application/octet-stream" },
      });
    }) as typeof fetch;
    (globalThis as unknown as { setTimeout: typeof setTimeout }).setTimeout = ((handler: TimerHandler, timeout?: number, ...args: unknown[]) => {
      // 中文注释：HTTP 下载允许首个元数据帧短超时，但不能给后续文件体注册整体超时。
      setTimeoutDelays.push(Number(timeout ?? 0));
      return originalSetTimeout(handler, timeout, ...args);
    }) as typeof setTimeout;
    try {
      await expect(client.downloadSessionFile(sessionId, "/tmp/raw.bin")).rejects.toMatchObject({
        code: "invalid_file_transfer",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      (globalThis as unknown as { setTimeout: typeof setTimeout }).setTimeout = originalSetTimeout;
      client.close();
    }

    expect(setTimeoutDelays).toEqual([3000]);
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_download")).toBe(false);
    expect(daemon.sessionFileDownloadChunkRequests).toHaveLength(0);
  });

  it("HTTP 下载端点不支持时不会回退到 WebSocket 文件流", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000328");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    try {
      for (const status of [426, 501]) {
        // 中文注释：下载不做 WebSocket fallback；501 来自默认禁用 HTTP tunnel 的 relay。
        (globalThis as unknown as { fetch: typeof fetch }).fetch = (async () =>
          new Response(null, { status })) as typeof fetch;
        await expect(client.downloadSessionFile(sessionId, `/tmp/raw-${status}.bin`)).rejects.toThrow("http_file_transfer_unsupported");
      }
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_download")).toBe(false);
    expect(daemon.sessionFileDownloadChunkRequests).toHaveLength(0);
  });

  it("HTTP 下载没有 ReadableStream body 时不会退回整包 arrayBuffer 缓冲", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000329");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);
    const originalFetch = globalThis.fetch;
    const arrayBuffer = vi.fn<() => Promise<ArrayBuffer>>();
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (_input, init) => {
      const headers = new Headers(init?.headers);
      const e2ee = httpE2eeSessionFromHeaders(headers);
      const ready = {
        session_id: sessionId,
        path: "/tmp/raw.bin",
        name: "raw.bin",
        size_bytes: 3,
        modified_at_ms: null,
      } satisfies SessionFileDownloadStreamReadyPayload;
      const encryptedBody = responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [encodeUtf8(JSON.stringify(ready))]));
      arrayBuffer.mockResolvedValue(encryptedBody);
      const response = new Response(null, { status: 200 });
      Object.defineProperty(response, "body", { value: null });
      Object.defineProperty(response, "arrayBuffer", { value: arrayBuffer });
      return response;
    }) as typeof fetch;
    try {
      await expect(
        client.downloadSessionFile(sessionId, "/tmp/raw.bin", { collectBytes: false }),
      ).rejects.toThrow("http_file_transfer_unsupported");
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(arrayBuffer).not.toHaveBeenCalled();
    expect(daemon.receivedPackets.some((packet) => packet.method === "session.file_download")).toBe(false);
  });

  it("HTTP E2EE 认证签名绑定 method、path 和 header 公钥", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000330");
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
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);

    const originalFetch = globalThis.fetch;
    let verified = false;
    let tamperedPathRejected = false;
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (_input, init) => {
      const headers = new Headers(init?.headers);
      const auth: HttpE2eeAuthPayload = {
        device_id: headers.get("x-termd-device-id") ?? "",
        e2ee_public_key: headers.get("x-termd-e2ee-public-key") ?? "",
        nonce: headers.get("x-termd-e2ee-nonce") ?? "",
        timestamp_ms: Number(headers.get("x-termd-e2ee-timestamp-ms") ?? "0"),
        method: "POST",
        path: "/api/files/download",
        signature: headers.get("x-termd-e2ee-signature") ?? "",
      };
      const publicKey = decodeEd25519PublicKey(device.device_public_key);
      const daemonIdentity = {
        server_id: daemon.serverId,
        daemon_public_key: daemon.daemonPublicKey,
      };
      verified = await verifyEd25519Signature(
        publicKey,
        httpE2eeSigningInputBytes(auth, daemonIdentity),
        auth.signature,
      );
      tamperedPathRejected = !(await verifyEd25519Signature(
        publicKey,
        httpE2eeSigningInputBytes({ ...auth, path: "/api/files/upload" }, daemonIdentity),
        auth.signature,
      ));

      const e2ee = httpE2eeSessionFromHeaders(headers);
      const ready = {
        session_id: sessionId,
        path: "/tmp/raw.bin",
        name: "raw.bin",
        size_bytes: 3,
        modified_at_ms: null,
      } satisfies SessionFileDownloadStreamReadyPayload;
      return new Response(responseBodyBytes(encodeHttpE2eeTestFrames(e2ee, [
        encodeUtf8(JSON.stringify(ready)),
        new Uint8Array([1, 2, 3]),
      ])));
    }) as typeof fetch;
    try {
      await client.downloadSessionFile(sessionId, "/tmp/raw.bin", {
        collectBytes: false,
        onChunk: () => undefined,
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    expect(verified).toBe(true);
    expect(tamperedPathRejected).toBe(true);
  });

  it("HTTP 文件传输保留 WebSocket 子路径前缀和 query", async () => {
    const prefixedUrl = daemon.url.replace("/ws", "/termd/ws?relay_token=abc");
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000319");
    const pairClient = await connectDevice(device.device_id, 3000, prefixedUrl);
    const accepted = await pairClient.pair("secret-token", device.device_public_key);
    pairClient.close();

    const client = await connectDevice(device.device_id, 3000, prefixedUrl);
    await client.authenticate(device, {
      server_id: accepted.server_id,
      daemon_public_key: accepted.daemon_public_key,
      url: prefixedUrl,
      paired_at_ms: 1710000000000,
    });
    const list = await client.listSessions();
    const sessionId = list.sessions[0].session_id;
    await client.attachSessionPermission(sessionId);

    const originalFetch = globalThis.fetch;
    let requestedUrl = "";
    (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input) => {
      requestedUrl = input instanceof Request ? input.url : String(input);
      return new Response(JSON.stringify({ code: "invalid_file_transfer", message: "bad request" }), {
        status: 400,
        headers: { "content-type": "application/json" },
      });
    }) as typeof fetch;
    try {
      await expect(client.downloadSessionFile(sessionId, "/tmp/raw.bin")).rejects.toMatchObject({
        code: "invalid_file_transfer",
      });
    } finally {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
      client.close();
    }

    const url = new URL(requestedUrl);
    expect(url.pathname).toBe("/termd/api/files/download");
    expect(url.searchParams.get("relay_token")).toBe("abc");
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
