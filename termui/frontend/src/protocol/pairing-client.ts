import { DirectClient, ProtocolClientError } from "./direct-client";
import type { UUID } from "./types";

export async function connectPairingClient(
  candidateUrls: string[],
  routeServerId: UUID,
  deviceId: UUID,
  daemonPublicKey: string,
  timeoutMs: number,
): Promise<{ client: DirectClient; effectiveUrl: string }> {
  if (!routeServerId) {
    throw new ProtocolClientError("pairing_server_unknown", "pairing requires a known daemon server id");
  }
  let lastError: unknown;
  for (const candidateUrl of candidateUrls) {
    try {
      const client = await DirectClient.connect(candidateUrl, routeServerId, deviceId, {
        expectedDaemonPublicKey: daemonPublicKey,
        timeoutMs,
      });
      if (client.serverId !== routeServerId) {
        client.close();
        lastError = new ProtocolClientError(
          "pairing_payload_server_mismatch",
          "pairing payload does not match the connected daemon",
        );
        continue;
      }
      return { client, effectiveUrl: candidateUrl };
    } catch (caught) {
      lastError = caught;
    }
  }

  throw normalizePairingRouteError(lastError) ??
    new ProtocolClientError("empty_pairing_candidates", "no pairing URL candidates");
}

function normalizePairingRouteError(error: unknown): unknown {
  if (
    error instanceof ProtocolClientError &&
    (error.code === "invalid_route_prelude" || error.code === "route_server_mismatch")
  ) {
    return new ProtocolClientError(
      "pairing_payload_server_mismatch",
      "pairing payload does not match the connected daemon",
    );
  }
  return error;
}
