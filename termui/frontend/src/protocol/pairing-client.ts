import { ProtocolClientError } from "./errors";
import { applicationHttpUrl } from "./access-token";
import { authPayloadForChallenge, signAuthPayload } from "./auth";
import type { DeviceState, PairAcceptPayload, PairedServerState, UUID } from "./types";

export async function pairDeviceOverHttp(
  candidateUrls: string[],
  routeServerId: UUID,
  daemonPublicKey: string,
  pairingToken: string,
  device: DeviceState,
): Promise<{ accepted: PairAcceptPayload; effectiveUrl: string }> {
  let lastError: unknown;
  for (const candidateUrl of candidateUrls) {
    try {
      const response = await fetch(applicationHttpUrl(candidateUrl, "/api/auth/pair"), {
        method: "POST",
        headers: {
          authorization: `TermdPair ${pairingToken}`,
          "content-type": "application/json",
          "x-termd-server-id": routeServerId,
        },
        body: JSON.stringify({ device_id: device.device_id, device_public_key: device.device_public_key }),
      });
      const body = await response.json() as {
        server_id?: UUID;
        device_id?: UUID;
        device_certificate?: string;
        error?: { code?: string; message?: string };
      };
      if (!response.ok || body.server_id !== routeServerId || !body.device_certificate) {
        throw new ProtocolClientError(body.error?.code ?? "pairing_failed", body.error?.message ?? "pairing failed");
      }
      return {
        accepted: {
          server_id: body.server_id,
          daemon_public_key: daemonPublicKey,
          device_id: body.device_id ?? device.device_id,
          expires_at_ms: Number.MAX_SAFE_INTEGER,
          device_certificate: body.device_certificate,
        },
        effectiveUrl: candidateUrl,
      };
    } catch (caught) {
      lastError = caught;
    }
  }
  throw lastError ?? new ProtocolClientError("empty_pairing_candidates", "no pairing URL candidates");
}

export async function migrateDeviceCertificate(
  server: PairedServerState,
  device: DeviceState,
): Promise<string> {
  const headers = {
    "content-type": "application/json",
    "x-termd-server-id": server.server_id,
  };
  const challengeResponse = await fetch(
    applicationHttpUrl(server.url, "/api/auth/device-certificate/migrate/challenge"),
    { method: "POST", headers, body: JSON.stringify({ device_id: device.device_id }) },
  );
  const challengeBody = await challengeResponse.json() as {
    challenge?: string;
    error?: { code?: string; message?: string };
  };
  if (!challengeResponse.ok || !challengeBody.challenge) {
    throw new ProtocolClientError(
      challengeBody.error?.code ?? "device_migration_not_allowed",
      challengeBody.error?.message ?? "device credential migration is not allowed",
    );
  }
  const proof = await signAuthPayload(
    authPayloadForChallenge(device.device_id, challengeBody.challenge),
    server,
    device.device_signing_key_secret,
  );
  const response = await fetch(applicationHttpUrl(server.url, "/api/auth/device-certificate/migrate"), {
    method: "POST",
    headers,
    body: JSON.stringify(proof),
  });
  const body = await response.json() as {
    device_certificate?: string;
    error?: { code?: string; message?: string };
  };
  if (!response.ok || !body.device_certificate) {
    throw new ProtocolClientError(
      body.error?.code ?? "device_migration_proof_invalid",
      body.error?.message ?? "device credential migration failed",
    );
  }
  return body.device_certificate;
}
