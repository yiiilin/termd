import { describe, expect, it } from "vitest";
import { browserViewerPath, browserWebSocketUrl } from "../protocol/browser-client";

const browserId = "11111111-2222-4333-8444-555555555555";
const serverId = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

describe("browser transport URL helpers", () => {
  it("builds direct and relay RFB routes without putting credentials in the URL", () => {
    expect(browserWebSocketUrl("ws://127.0.0.1:8765/ws", browserId)).toBe(
      `ws://127.0.0.1:8765/ws/browser/${browserId}`,
    );
    expect(browserWebSocketUrl("wss://relay.example/ws?token=remove-me", browserId)).toBe(
      `wss://relay.example/ws/browser/${browserId}`,
    );
  });

  it("puts only routing identifiers in the independent viewer location", () => {
    const path = browserViewerPath(serverId, browserId);
    expect(path).toBe(`/browser/${browserId}?server_id=${serverId}`);
    expect(path).not.toContain("token");
  });
});
