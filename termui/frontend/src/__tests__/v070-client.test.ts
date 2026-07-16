import { afterEach, describe, expect, it, vi } from "vitest";
import { V070Client } from "../protocol/v070-client";
import { generateDeviceIdentity } from "../protocol/auth";
import { ProtocolClientError } from "../protocol/errors";
import {
  decodeSupervisorTerminalClientFrame,
  decodeSupervisorTerminalServerFrame,
  encodeSupervisorTerminalServerFrame,
} from "../protocol/supervisor-terminal";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("V070Client", () => {
  it("closes only the terminal socket attached to the session being closed", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const request = vi.fn(async (path: string) => ({ session_id: path.includes("session-a") ? "session-a" : "session-b" }));
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
      request,
    );
    const pending = client.listSessions();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [{ session_id: "session-a" }], clients: [], daemon: {} } },
    }));
    await expect(pending).resolves.toMatchObject({ sessions: [{ session_id: "session-a" }] });
    const attached = client.attachSession("session-a");
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: {
        session_id: "session-a",
        role: "operator",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    }));
    await attached;

    await client.closeSession("session-b");
    expect(transport.closeTerminal).not.toHaveBeenCalled();
    await client.sendSessionData("session-a", new TextEncoder().encode("still-attached"));

    await client.closeSession("session-a");
    expect(transport.closeTerminal).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledTimes(2);
    expect(request).toHaveBeenNthCalledWith(1, "/api/control/session/session-b/close", {});
    expect(request).toHaveBeenNthCalledWith(2, "/api/control/session/session-a/close", {});
  });

  it("uploads raw chunks and downloads raw bytes through v0.7 file routes", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const http = vi.fn(async (path: string, init: RequestInit = {}) => {
      if (path === "/api/files/uploads") {
        return new Response(JSON.stringify({ upload_id: "upload-a", size_bytes: 6, offset_bytes: 0 }), { status: 201 });
      }
      if (path === "/api/files/uploads/upload-a/chunks") {
        return new Response(JSON.stringify({ session_id: "session-a", path: "/tmp/a", offset_bytes: 6, size_bytes: 6, eof: true }), { status: 200 });
      }
      if (path === "/api/files/uploads/upload-a/commit") {
        return new Response(JSON.stringify({ session_id: "session-a", path: "/tmp/a", offset_bytes: 6, size_bytes: 6, eof: true }), { status: 200 });
      }
      if (path === "/api/files/downloads") {
        return new Response(JSON.stringify({ download_id: "download-a", path: "/tmp/a", size_bytes: 6 }), { status: 201 });
      }
      if (path === "/api/files/downloads/download-a") {
        return new Response(new TextEncoder().encode("abcdef"), { status: 200 });
      }
      throw new Error(`unexpected request: ${path} ${init.method}`);
    });
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
      vi.fn(),
      http,
    );

    await expect(client.uploadSessionFile("session-a", "/tmp/a", new Blob(["abcdef"]))).resolves.toMatchObject({ eof: true });
    const downloaded = await client.downloadSessionFile("session-a", "/tmp/a");
    expect(downloaded).toMatchObject({
      path: "/tmp/a",
      size_bytes: 6,
    });
    expect(Array.from(downloaded.bytes)).toEqual(Array.from(new TextEncoder().encode("abcdef")));

    expect(http).toHaveBeenCalledWith(
      "/api/files/uploads/upload-a/chunks",
      expect.objectContaining({ method: "PUT", headers: expect.objectContaining({ "content-range": "bytes 0-5/6" }) }),
    );
  });

  it("resynchronizes metadata instead of applying a revision gap", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );

    const initial = client.listSessions();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [{ session_id: "session-a" }], clients: [], daemon: {} } },
    }));
    await expect(initial).resolves.toMatchObject({ sessions: [{ session_id: "session-a" }] });

    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 3, state: { sessions: [{ session_id: "stale-gap" }], clients: [], daemon: {} } },
    }));
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(1);
    let settled = false;
    const resynced = client.listSessions().finally(() => { settled = true; });
    await Promise.resolve();
    expect(settled).toBe(false);

    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [{ session_id: "session-b" }], clients: [], daemon: {} } },
    }));
    await expect(resynced).resolves.toMatchObject({ sessions: [{ session_id: "session-b" }] });
  });

  it("delivers revisioned metadata through a dedicated listener", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const states: Array<{ revision: number; state: any; deliveryKind: "snapshot" | "update" }> = [];
    const unsubscribe = client.watchMetadata((revision, state, deliveryKind) => {
      states.push({ revision, state, deliveryKind });
    });

    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: {
        revision: 7,
        state: {
          sessions: [
            {
              session_id: "session-codex",
              activity: { kind: "ai", agent: "codex", state: "running", changed_at_ms: 123 },
            },
            {
              session_id: "session-claude",
              activity: { kind: "ai", agent: "claude_code", state: "attention", changed_at_ms: 124 },
            },
            {
              session_id: "session-opencode",
              activity: { kind: "ai", agent: "opencode", state: "completed", changed_at_ms: 125 },
            },
            {
              session_id: "session-zcode",
              activity: { kind: "ai", agent: "zcode", state: "idle", changed_at_ms: 126 },
            },
          ],
          clients: [{ device_id: "device-a" }],
          daemon: { cpu_percent: 1 },
        },
      },
    }));
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 8, state: { sessions: [], clients: [{ device_id: "device-b" }], daemon: { cpu_percent: 2 } } },
    }));
    const cachedDeliveries: Array<{ revision: number; deliveryKind: "snapshot" | "update" }> = [];
    const unsubscribeCached = client.watchMetadata((revision, _state, deliveryKind) => {
      cachedDeliveries.push({ revision, deliveryKind });
    });
    unsubscribeCached();
    unsubscribe();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 9, state: { sessions: [], clients: [], daemon: {} } },
    }));

    expect(states).toEqual([
      {
        revision: 7,
        deliveryKind: "snapshot",
        state: expect.objectContaining({
          clients: [{ device_id: "device-a" }],
          sessions: [
            {
              session_id: "session-codex",
              activity: { kind: "ai", agent: "codex", state: "running", changed_at_ms: 123 },
            },
            {
              session_id: "session-claude",
              activity: { kind: "ai", agent: "claude_code", state: "attention", changed_at_ms: 124 },
            },
            {
              session_id: "session-opencode",
              activity: { kind: "ai", agent: "opencode", state: "completed", changed_at_ms: 125 },
            },
            {
              session_id: "session-zcode",
              activity: { kind: "ai", agent: "zcode", state: "idle", changed_at_ms: 126 },
            },
          ],
        }),
      },
      {
        revision: 8,
        deliveryKind: "update",
        state: expect.objectContaining({ clients: [{ device_id: "device-b" }] }),
      },
    ]);
    expect(cachedDeliveries).toEqual([{ revision: 8, deliveryKind: "snapshot" }]);
  });

  it("measures RTT from an echoed metadata ping timestamp instead of snapshot rtt_ms", async () => {
    const sentAtMs = 1_710_000_000_000;
    const nowSpy = vi.spyOn(Date, "now").mockReturnValue(sentAtMs);
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      sendMetadata: vi.fn(),
      sendTerminal: vi.fn(),
      closeTerminal: vi.fn(),
      close: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const metadataReady = client.subscribeMetadata();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [], clients: [], daemon: {}, rtt_ms: 1 } },
    }));
    await metadataReady;

    const measured = client.measureLatency();
    const concurrentMeasured = client.measureLatency();
    await Promise.resolve();
    await Promise.resolve();
    expect(transport.sendMetadata).toHaveBeenCalledTimes(1);
    expect(transport.sendMetadata).toHaveBeenCalledWith(JSON.stringify({
      type: "metadata.ping",
      payload: { timestamp_ms: sentAtMs },
    }));

    let measuredSettled = false;
    void measured.finally(() => { measuredSettled = true; });
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.pong",
      payload: { timestamp_ms: sentAtMs - 1 },
    }));
    await Promise.resolve();
    expect(measuredSettled).toBe(false);

    nowSpy.mockReturnValue(sentAtMs + 42);
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.pong",
      payload: { timestamp_ms: sentAtMs },
    }));
    await expect(measured).resolves.toBe(42);
    await expect(concurrentMeasured).resolves.toBe(42);
    client.close();
  });

  it("uses terminal activity as a liveness acknowledgement without consuming the frame", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      sendTerminal: vi.fn(),
      closeTerminal: vi.fn(),
      close: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const attached = client.attachSession("session-a");
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: {
        session_id: "session-a",
        role: "operator",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    }));
    await attached;

    const liveness = client.probeTerminalLiveness(
      "session-a",
      { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      100,
    );
    await Promise.resolve();
    expect(transport.sendTerminal).toHaveBeenCalledTimes(1);
    const sentFrame = decodeSupervisorTerminalClientFrame(
      transport.sendTerminal.mock.calls[0]?.[0] as Uint8Array,
    );
    expect(sentFrame).toMatchObject({
      type: "resize",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    });

    const outputFrame = encodeSupervisorTerminalServerFrame({
      type: "terminal_frame",
      session_id: "session-a",
      frame: {
        kind: "output",
        session_id: "session-a",
        terminal_seq: 1,
        data_bytes: new TextEncoder().encode("probe-alive"),
      },
    });
    transport.onTerminal?.(outputFrame.buffer.slice(
      outputFrame.byteOffset,
      outputFrame.byteOffset + outputFrame.byteLength,
    ));
    await expect(liveness).resolves.toBeUndefined();

    const received = await client.receiveInner();
    expect(received.type).toBe("attach_frame");
    const receivedFrame = decodeSupervisorTerminalServerFrame(
      (received.payload as { data_bytes: Uint8Array }).data_bytes,
    );
    expect(receivedFrame).toMatchObject({
      type: "terminal_frame",
      session_id: "session-a",
      frame: { kind: "output", terminal_seq: 1 },
    });
    client.close();
  });

  it("rejects a terminal liveness probe on timeout and close", async () => {
    vi.useFakeTimers();
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const makeAttachedClient = async () => {
      const transport = {
        onMetadata: undefined as ((data: unknown) => void) | undefined,
        onTerminal: undefined as ((data: unknown) => void) | undefined,
        connectMetadata: vi.fn(async () => undefined),
        reconnectMetadata: vi.fn(async () => undefined),
        openTerminal: vi.fn(async () => undefined),
        sendTerminal: vi.fn(),
        closeTerminal: vi.fn(),
        close: vi.fn(),
      };
      const client = new V070Client(
        {
          server_id: "00000000-0000-0000-0000-000000000070",
          daemon_public_key: "ed25519-v1:daemon",
          url: "wss://relay.example/ws",
          paired_at_ms: 1,
          device_certificate: "device.certificate.signature",
        },
        device,
        transport,
      );
      const attached = client.attachSession("session-a");
      transport.onTerminal?.(JSON.stringify({
        type: "terminal.attached",
        payload: {
          session_id: "session-a",
          role: "operator",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      }));
      await attached;
      return { client, transport };
    };

    const timedOut = await makeAttachedClient();
    const timeoutProbe = timedOut.client.probeTerminalLiveness(
      "session-a",
      { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      5,
    );
    const timeoutOutcome = timeoutProbe.catch((error) => error);
    await vi.advanceTimersByTimeAsync(5);
    expect(await timeoutOutcome).toMatchObject({ code: "response_timeout" });
    timedOut.client.close();

    const closed = await makeAttachedClient();
    const closeProbe = closed.client.probeTerminalLiveness(
      "session-a",
      { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      50,
    );
    const closeOutcome = closeProbe.catch((error) => error);
    closed.client.close();
    expect(await closeOutcome).toMatchObject({ code: "connection_closed" });
  });

  it("cleans pending RTT measurements on timeout, close, metadata close, and reconnect", async () => {
    vi.useFakeTimers();
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const openMeasurement = async () => {
      const transport = {
        onMetadata: undefined as ((data: unknown) => void) | undefined,
        onTerminal: undefined as ((data: unknown) => void) | undefined,
        onMetadataClose: undefined as (() => void) | undefined,
        onTerminalClose: undefined as (() => void) | undefined,
        connectMetadata: vi.fn(async () => undefined),
        reconnectMetadata: vi.fn(async () => undefined),
        openTerminal: vi.fn(async () => undefined),
        sendMetadata: vi.fn(),
        sendTerminal: vi.fn(),
        closeTerminal: vi.fn(),
        close: vi.fn(),
      };
      const client = new V070Client(
        {
          server_id: "00000000-0000-0000-0000-000000000070",
          daemon_public_key: "ed25519-v1:daemon",
          url: "wss://relay.example/ws",
          paired_at_ms: 1,
          device_certificate: "device.certificate.signature",
        },
        device,
        transport,
        undefined,
        undefined,
        5,
      );
      const metadataReady = client.subscribeMetadata();
      transport.onMetadata?.(JSON.stringify({
        type: "metadata.snapshot",
        payload: { revision: 1, state: { sessions: [], clients: [], daemon: {} } },
      }));
      await metadataReady;
      const measured = client.measureLatency();
      await Promise.resolve();
      return { client, measured, transport };
    };

    const timedOut = await openMeasurement();
    const timeoutOutcome = timedOut.measured.catch((error) => error);
    await vi.advanceTimersByTimeAsync(5);
    expect(await timeoutOutcome).toMatchObject({ code: "response_timeout" });
    timedOut.client.close();

    const closed = await openMeasurement();
    const closeOutcome = closed.measured.catch((error) => error);
    closed.client.close();
    expect(await closeOutcome).toMatchObject({ code: "connection_closed" });

    const metadataClosed = await openMeasurement();
    const metadataCloseOutcome = metadataClosed.measured.catch((error) => error);
    metadataClosed.transport.onMetadataClose?.();
    expect(await metadataCloseOutcome).toMatchObject({ code: "connection_closed" });
    metadataClosed.client.close();

    const reconnecting = await openMeasurement();
    const reconnectOutcome = reconnecting.measured.catch((error) => error);
    reconnecting.transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 3, state: { sessions: [], clients: [], daemon: {} } },
    }));
    expect(await reconnectOutcome).toMatchObject({ code: "stale_connection" });
    reconnecting.client.close();
  });

  it("queues supervisor binary snapshots received after terminal attach", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const attached = client.attachSession("session-a");
    transport.onTerminal?.(JSON.stringify({
      type: "terminal.attached",
      payload: { session_id: "session-a", role: "operator", state: "running", size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 } },
    }));
    await attached;
    transport.onTerminal?.(encodeSupervisorTerminalServerFrame({
      type: "attach_sync",
      session_id: "session-a",
      base_seq: 0,
      snapshot: {
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        process_id: 7,
        retained_output_bytes: new TextEncoder().encode("ready\n"),
      },
      frames: [],
    }));

    const envelope = await client.receiveInner();
    const frame = decodeSupervisorTerminalServerFrame((envelope.payload as any).data_bytes);
    expect(frame).toMatchObject({
      type: "attach_sync",
      session_id: "session-a",
    });
    expect(new TextDecoder().decode(frame.type === "attach_sync" ? frame.snapshot.retained_output_bytes : new Uint8Array())).toBe("ready\n");
  });

  it("accepts browser Blob terminal frames", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const attached = client.attachSession("session-a");
    transport.onTerminal?.(JSON.stringify({ type: "terminal.attached", payload: { session_id: "session-a" } }));
    await attached;
    const bytes = encodeSupervisorTerminalServerFrame({
      type: "attach_sync",
      session_id: "session-a",
      base_seq: 0,
      snapshot: { size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }, process_id: 7, retained_output_bytes: new Uint8Array() },
      frames: [],
    });
    let received = false;
    void client.receiveInner().then(() => { received = true; });
    const blobBytes = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
    transport.onTerminal?.(new Blob([blobBytes]));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(received).toBe(true);
  });

  it("does not let a late terminal open failure reject the newer attach", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    let rejectFirst!: (error: unknown) => void;
    let opens = 0;
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(() => {
        opens += 1;
        if (opens === 1) return new Promise<undefined>((_resolve, reject) => { rejectFirst = reject; });
        return Promise.resolve(undefined);
      }),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );

    const first = client.attachSession("session-a");
    const firstRejected = expect(first).rejects.toMatchObject({ code: "stale_connection" });
    const second = client.attachSession("session-b");
    rejectFirst(new ProtocolClientError("stale_connection", "old terminal was superseded"));
    await Promise.resolve();
    transport.onTerminal?.(JSON.stringify({ type: "terminal.attached", payload: { session_id: "session-b" } }));

    await firstRejected;
    await expect(second).resolves.toMatchObject({ session_id: "session-b" });
    await client.sendSessionData("session-b", new TextEncoder().encode("input-b"));
    expect(transport.sendTerminal).toHaveBeenCalledTimes(1);
  });

  it("rejects a pending attach on terminal close, clears early frames, and allows retry", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const staleFrame = encodeSupervisorTerminalServerFrame({
      type: "attach_sync",
      session_id: "session-a",
      base_seq: 0,
      snapshot: {
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        process_id: 1,
        retained_output_bytes: new TextEncoder().encode("stale-a"),
      },
      frames: [],
    });

    const first = client.attachSession("session-a");
    const firstOutcome = Promise.race([
      first.then(() => "resolved", (error: unknown) => (error as { code?: string }).code),
      new Promise<string>((resolve) => setTimeout(() => resolve("pending"), 0)),
    ]);
    transport.onTerminal?.(staleFrame);
    transport.onTerminalClose?.();
    expect(await firstOutcome).toBe("connection_closed");

    const second = client.attachSession("session-b");
    transport.onTerminal?.(JSON.stringify({ type: "terminal.attached", payload: { session_id: "session-b" } }));
    await expect(second).resolves.toMatchObject({ session_id: "session-b" });
    transport.onTerminal?.(encodeSupervisorTerminalServerFrame({
      type: "attach_sync",
      session_id: "session-b",
      base_seq: 0,
      snapshot: {
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        process_id: 2,
        retained_output_bytes: new TextEncoder().encode("current-b"),
      },
      frames: [],
    }));
    const envelope = await client.receiveInner();
    const frame = decodeSupervisorTerminalServerFrame((envelope.payload as any).data_bytes);
    expect(frame).toMatchObject({ type: "attach_sync", session_id: "session-b" });
  });

  it("rejects pending terminal and metadata operations when the client closes", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const terminal = client.createSession([], { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 });
    const metadata = client.listSessions();
    const settleCode = (promise: Promise<unknown>) => Promise.race([
      promise.then(() => "resolved", (error: unknown) => (error as { code?: string }).code),
      new Promise<string>((resolve) => setTimeout(() => resolve("pending"), 0)),
    ]);
    const terminalOutcome = settleCode(terminal);
    const metadataOutcome = settleCode(metadata);
    await Promise.resolve();
    await Promise.resolve();
    client.close();

    expect(await terminalOutcome).toBe("connection_closed");
    expect(await metadataOutcome).toBe("connection_closed");
  });

  it("retries metadata reconnect after an established socket closes until a snapshot arrives", async () => {
    vi.useFakeTimers();
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    let reconnects = 0;
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => {
        reconnects += 1;
        if (reconnects === 1) throw new Error("first reconnect failed");
      }),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const states: Array<{ revision: number; deliveryKind: "snapshot" | "update" }> = [];
    client.watchMetadata((revision, _state, deliveryKind) => states.push({ revision, deliveryKind }));
    const initial = client.listSessions();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [{ session_id: "session-a" }] } },
    }));
    await initial;
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 2, state: { sessions: [{ session_id: "session-a" }] } },
    }));

    transport.onMetadataClose?.();
    await Promise.resolve();
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(100);
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(2);
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 3, state: { sessions: [{ session_id: "session-b" }] } },
    }));

    await expect(client.listSessions()).resolves.toMatchObject({ sessions: [{ session_id: "session-b" }] });
    expect(states).toEqual([
      { revision: 1, deliveryKind: "snapshot" },
      { revision: 2, deliveryKind: "update" },
      { revision: 3, deliveryKind: "snapshot" },
    ]);
    client.close();
  });

  it("rejects a pre-snapshot metadata waiter and recovers on the next reconnect", async () => {
    vi.useFakeTimers();
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    let reconnects = 0;
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => {
        reconnects += 1;
        if (reconnects === 1) throw new Error("first reconnect failed");
      }),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );

    const initial = client.listSessions();
    await Promise.resolve();
    await Promise.resolve();
    transport.onMetadataClose?.();
    await expect(initial).rejects.toMatchObject({ code: "connection_closed" });
    await Promise.resolve();
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(100);
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(2);
    const retry = client.listSessions();
    await Promise.resolve();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [{ session_id: "session-b" }] } },
    }));
    await expect(retry).resolves.toMatchObject({ sessions: [{ session_id: "session-b" }] });
    client.close();
  });

  it("cancels a scheduled metadata reconnect when the client closes", async () => {
    vi.useFakeTimers();
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => { throw new Error("reconnect failed"); }),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
    );
    const initial = client.listSessions();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 1, state: { sessions: [] } },
    }));
    await initial;
    transport.onMetadataClose?.();
    await Promise.resolve();
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(1);

    client.close();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(transport.reconnectMetadata).toHaveBeenCalledTimes(1);
  });

  it("times out ordinary JSON control without timing out raw upload and download bodies", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const transport = {
      onMetadata: undefined as ((data: unknown) => void) | undefined,
      onTerminal: undefined as ((data: unknown) => void) | undefined,
      onMetadataClose: undefined as (() => void) | undefined,
      onTerminalClose: undefined as (() => void) | undefined,
      connectMetadata: vi.fn(async () => undefined),
      reconnectMetadata: vi.fn(async () => undefined),
      openTerminal: vi.fn(async () => undefined),
      closeTerminal: vi.fn(),
      close: vi.fn(),
      sendTerminal: vi.fn(),
    };
    const rawSignals: Array<AbortSignal | null | undefined> = [];
    vi.stubGlobal("fetch", vi.fn(async (input: string | URL | Request, init: RequestInit = {}) => {
      const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
      if (url.pathname.endsWith("/api/auth/challenge")) {
        return Response.json({ challenge: "challenge-a" });
      }
      if (url.pathname.endsWith("/api/auth/access-token")) {
        return Response.json({
          access_token: "access.claims.signature",
          expires_at_ms: Date.now() + 60_000,
          refresh_at_ms: Date.now() + 50_000,
        });
      }
      if (url.pathname.endsWith("/api/control/session/session-a/rename")) {
        return new Promise<Response>((_resolve, reject) => {
          init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
        });
      }
      if (url.pathname.endsWith("/api/files/uploads") && init.method === "POST") {
        return Response.json({ upload_id: "upload-a", size_bytes: 6, offset_bytes: 0 }, { status: 201 });
      }
      if (url.pathname.endsWith("/api/files/uploads/upload-a/chunks") && init.method === "PUT") {
        rawSignals.push(init.signal);
        await new Promise((resolve) => setTimeout(resolve, 30));
        return Response.json({ session_id: "session-a", path: "/tmp/a", offset_bytes: 6, size_bytes: 6, eof: true });
      }
      if (url.pathname.endsWith("/api/files/uploads/upload-a/commit") && init.method === "POST") {
        return Response.json({ session_id: "session-a", path: "/tmp/a", offset_bytes: 6, size_bytes: 6, eof: true });
      }
      if (url.pathname.endsWith("/api/files/downloads") && init.method === "POST") {
        return Response.json({ download_id: "download-a", path: "/tmp/a", size_bytes: 6 }, { status: 201 });
      }
      if (url.pathname.endsWith("/api/files/downloads/download-a") && init.method === "GET") {
        rawSignals.push(init.signal);
        await new Promise((resolve) => setTimeout(resolve, 30));
        return new Response(new TextEncoder().encode("abcdef"));
      }
      throw new Error(`unexpected request: ${url.pathname} ${init.method}`);
    }));
    const client = new V070Client(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:daemon",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
      transport,
      undefined,
      undefined,
      5,
    );

    await expect(client.renameSession("session-a", "renamed")).rejects.toMatchObject({ code: "response_timeout" });
    const uploadController = new AbortController();
    const downloadController = new AbortController();
    await expect(client.uploadSessionFile("session-a", "/tmp/a", new Blob(["abcdef"]), {
      signal: uploadController.signal,
    })).resolves.toMatchObject({ eof: true });
    await expect(client.downloadSessionFile("session-a", "/tmp/a", {
      signal: downloadController.signal,
    })).resolves.toMatchObject({ size_bytes: 6 });
    expect(rawSignals).toEqual([uploadController.signal, downloadController.signal]);
    expect(rawSignals.every((signal) => !signal?.aborted)).toBe(true);
    client.close();
  });
});
