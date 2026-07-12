import { describe, expect, it, vi } from "vitest";
import { V070Client } from "../protocol/v070-client";
import { generateDeviceIdentity } from "../protocol/auth";
import { decodeSupervisorTerminalServerFrame, encodeSupervisorTerminalServerFrame } from "../protocol/supervisor-terminal";

describe("V070Client", () => {
  it("uses pushed metadata and closes a session with one JSON request", async () => {
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
    const request = vi.fn(async () => ({ session_id: "session-a", state: "closed" }));
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
    await client.closeSession("session-a");
    expect(transport.closeTerminal).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledWith("/api/control/session/session-a/close", {});
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
    const states: Array<{ revision: number; state: any }> = [];
    const unsubscribe = client.watchMetadata((revision, state) => states.push({ revision, state }));

    transport.onMetadata?.(JSON.stringify({
      type: "metadata.snapshot",
      payload: { revision: 7, state: { sessions: [], clients: [{ device_id: "device-a" }], daemon: { cpu_percent: 1 } } },
    }));
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 8, state: { sessions: [], clients: [{ device_id: "device-b" }], daemon: { cpu_percent: 2 } } },
    }));
    unsubscribe();
    transport.onMetadata?.(JSON.stringify({
      type: "metadata.update",
      payload: { revision: 9, state: { sessions: [], clients: [], daemon: {} } },
    }));

    expect(states).toEqual([
      { revision: 7, state: expect.objectContaining({ clients: [{ device_id: "device-a" }] }) },
      { revision: 8, state: expect.objectContaining({ clients: [{ device_id: "device-b" }] }) },
    ]);
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
});
