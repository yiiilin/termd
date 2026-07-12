import { afterEach, describe, expect, it, vi } from "vitest";
import { AccessTokenManager } from "../protocol/access-token";
import { generateDeviceIdentity } from "../protocol/auth";

describe("AccessTokenManager v0.7", () => {
  it("deduplicates challenge exchange and refreshes sixty seconds before expiry", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ challenge: "challenge-a" }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({
        access_token: "header.claims.signature",
        expires_at_ms: 400_000,
        refresh_at_ms: 340_000,
      }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const manager = new AccessTokenManager(
      {
        server_id: "00000000-0000-0000-0000-000000000070",
        daemon_public_key: "ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
        device_certificate: "device.certificate.signature",
      },
      device,
    );

    const [first, second] = await Promise.all([manager.get(100_000), manager.get(100_000)]);
    expect(first).toBe("header.claims.signature");
    expect(second).toBe(first);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(fetchMock.mock.calls[0]?.[0]).toBe("https://relay.example/api/auth/challenge");
  });

  it("proactively refreshes at refresh_at_ms and notifies WebSocket owners", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(100_000);
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ challenge: "challenge-a" }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({
        access_token: "token-a",
        expires_at_ms: 400_000,
        refresh_at_ms: 340_000,
      }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ challenge: "challenge-b" }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({
        access_token: "token-b",
        expires_at_ms: 700_000,
        refresh_at_ms: 640_000,
      }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const manager = new AccessTokenManager({
      server_id: "00000000-0000-0000-0000-000000000070",
      daemon_public_key: "ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
      url: "wss://relay.example/ws",
      paired_at_ms: 1,
      device_certificate: "device.certificate.signature",
    }, device);
    const refreshed = vi.fn();
    manager.onRefresh(refreshed);

    await expect(manager.get()).resolves.toBe("token-a");
    await vi.advanceTimersByTimeAsync(240_000);

    expect(fetchMock).toHaveBeenCalledTimes(4);
    expect(refreshed).toHaveBeenCalledWith("token-b");
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
