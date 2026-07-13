import { describe, expect, it, vi } from "vitest";
import { WorkspaceTransport } from "../protocol/workspace-transport";

class FakeSocket {
  static instances: FakeSocket[] = [];
  static autoOpen = true;
  readyState = 0;
  onopen?: () => void;
  onmessage?: (event: { data: unknown }) => void;
  onclose?: () => void;
  onerror?: () => void;
  sent: unknown[] = [];

  constructor(public readonly url: string, public readonly protocols: string[]) {
    FakeSocket.instances.push(this);
    if (FakeSocket.autoOpen) queueMicrotask(() => this.open());
  }

  open() { this.readyState = 1; this.onopen?.(); }
  receive(value: unknown) { this.onmessage?.({ data: value }); }
  send(value: unknown) { this.sent.push(value); }
  close() { this.readyState = 3; this.onclose?.(); }
}

describe("WorkspaceTransport v0.7", () => {
  it("keeps exactly one metadata socket and one replaceable terminal socket", async () => {
    FakeSocket.instances = [];
    FakeSocket.autoOpen = true;
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

  it("keeps a late terminal open from replacing the newer terminal", async () => {
    FakeSocket.instances = [];
    FakeSocket.autoOpen = false;
    let releaseFirstToken!: (token: string) => void;
    let tokenRequests = 0;
    const transport = new WorkspaceTransport(
      "wss://relay.example/ws?server_id=server-a",
      {
        get: vi.fn(() => {
          tokenRequests += 1;
          if (tokenRequests === 1) {
            return new Promise<string>((resolve) => { releaseFirstToken = resolve; });
          }
          return Promise.resolve("new.header.signature");
        }),
      },
    );
    const received = vi.fn();
    transport.onTerminal = received;

    const first = transport.openTerminal({ type: "terminal.attach", payload: { session_id: "session-a" } });
    await Promise.resolve();
    const second = transport.openTerminal({ type: "terminal.attach", payload: { session_id: "session-b" } });
    await Promise.resolve();
    const secondSocket = FakeSocket.instances[0];
    secondSocket.open();
    await expect(second).resolves.toBe(secondSocket);

    const firstRejected = expect(first).rejects.toMatchObject({ code: "stale_connection" });
    releaseFirstToken("old.header.signature");
    await Promise.resolve();
    const lateFirstSocket = FakeSocket.instances[1];
    if (lateFirstSocket.readyState !== 3) lateFirstSocket.open();
    await firstRejected;

    lateFirstSocket.receive("late-a");
    secondSocket.receive("current-b");
    transport.sendTerminal("input-b");
    expect(received).toHaveBeenCalledTimes(1);
    expect(received).toHaveBeenCalledWith("current-b");
    expect(lateFirstSocket.readyState).toBe(3);
    expect(secondSocket.sent).toEqual([
      JSON.stringify({ type: "terminal.attach", payload: { session_id: "session-b" } }),
      "input-b",
    ]);
  });

  it("closes and rejects a terminal socket that is still opening", async () => {
    FakeSocket.instances = [];
    FakeSocket.autoOpen = false;
    const transport = new WorkspaceTransport(
      "wss://relay.example/ws?server_id=server-a",
      { get: vi.fn(async () => "header.claims.signature") },
    );

    const pending = transport.openTerminal({ type: "terminal.attach", payload: { session_id: "session-a" } });
    await Promise.resolve();
    const openingSocket = FakeSocket.instances[0];
    transport.closeTerminal();

    expect(openingSocket.readyState).toBe(3);
    await expect(pending).rejects.toMatchObject({ code: "stale_connection" });
  });
});
