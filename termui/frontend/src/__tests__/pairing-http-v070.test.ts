import { afterEach, describe, expect, it, vi } from "vitest";
import { generateDeviceIdentity } from "../protocol/auth";
import { migrateDeviceCertificate, pairDeviceOverHttp } from "../protocol/pairing-client";

afterEach(() => vi.unstubAllGlobals());

describe("HTTP pairing v0.7", () => {
  it("persists the daemon-signed device certificate", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify({
      server_id: "00000000-0000-0000-0000-000000000070",
      device_id: device.device_id,
      device_certificate: "device.certificate.signature",
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const result = await pairDeviceOverHttp(
      ["wss://relay.example/ws"],
      "00000000-0000-0000-0000-000000000070",
      "ed25519-v1:daemon",
      "pair.ticket.signature",
      device,
    );
    expect(result.accepted.device_certificate).toBe("device.certificate.signature");
    expect(fetchMock.mock.calls[0]?.[0]).toBe("https://relay.example/api/auth/pair");
    expect((fetchMock.mock.calls[0]?.[1] as RequestInit).headers).toMatchObject({
      authorization: "TermdPair pair.ticket.signature",
      "x-termd-server-id": "00000000-0000-0000-0000-000000000070",
    });
  });

  it("migrates a legacy paired device using its persisted device key", async () => {
    const device = await generateDeviceIdentity("00000000-0000-0000-0000-000000000071");
    const server = {
      server_id: "00000000-0000-0000-0000-000000000070",
      daemon_public_key: "ed25519-v1:daemon",
      url: "wss://relay.example/ws",
      paired_at_ms: 1,
    };
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ challenge: "migration-challenge" }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({
        device_certificate: "migrated.device.certificate",
      }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(migrateDeviceCertificate(server, device)).resolves.toBe("migrated.device.certificate");
    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      "https://relay.example/api/auth/device-certificate/migrate/challenge",
      "https://relay.example/api/auth/device-certificate/migrate",
    ]);
    const proof = JSON.parse((fetchMock.mock.calls[1]?.[1] as RequestInit).body as string);
    expect(proof).toMatchObject({
      device_id: device.device_id,
      challenge: "migration-challenge",
    });
    expect(proof.signature).toMatch(/^ed25519-v1:/);
  });
});
