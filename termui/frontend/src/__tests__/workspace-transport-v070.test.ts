import { describe, expect, it, vi } from "vitest";
import { WorkspaceTransport } from "../protocol/workspace-transport";

class FakeSocket {
  static instances: FakeSocket[] = [];
  readyState = 0;
  onopen?: () => void;
  onmessage?: (event: { data: unknown }) => void;
  onclose?: () => void;
  sent: unknown[] = [];

  constructor(public readonly url: string, public readonly protocols: string[]) {
    FakeSocket.instances.push(this);
    queueMicrotask(() => {
      this.readyState = 1;
      this.onopen?.();
    });
  }

  send(value: unknown) { this.sent.push(value); }
  close() { this.readyState = 3; this.onclose?.(); }
}

describe("WorkspaceTransport v0.7", () => {
  it("keeps exactly one metadata socket and one replaceable terminal socket", async () => {
    FakeSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeSocket);
    const transport = new WorkspaceTransport(
      "wss://relay.example/ws?server_id=server-a",
      { get: vi.fn(async () => "header.claims.signature") },
    );

    await Promise.all([
      transport.connectMetadata(),
      transport.connectMetadata(),
      transport.connectMetadata(),
    ]);
    expect(FakeSocket.instances).toHaveLength(1);
    await transport.openTerminal({ type: "terminal.attach", payload: { session_id: "session-a" } });
    expect(FakeSocket.instances.filter((socket) => socket.readyState !== 3)).toHaveLength(2);

    await transport.openTerminal({ type: "terminal.create", payload: { command: [], size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 } } });
    expect(FakeSocket.instances.filter((socket) => socket.readyState !== 3)).toHaveLength(2);
    expect(FakeSocket.instances.at(-1)?.url).toBe("wss://relay.example/ws/terminal");
    expect(FakeSocket.instances.at(-1)?.protocols).toEqual(["termd.v0.7", "header.claims.signature"]);
  });
});
