import type { PairingQrPayload } from "./types";

const PAIRING_QR_TYPE = "termd_pairing_qr";
const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function parsePairingQrPayload(raw: string): PairingQrPayload | undefined {
  const trimmed = raw.trim();
  if (!trimmed.startsWith("{")) {
    return undefined;
  }

  try {
    const parsed = JSON.parse(trimmed) as Partial<PairingQrPayload> & { type?: string };
    if (
      parsed.type !== PAIRING_QR_TYPE ||
      parsed.version !== 1 ||
      typeof parsed.ws_url !== "string" ||
      !isSupportedWsUrl(parsed.ws_url) ||
      typeof parsed.token !== "string" ||
      parsed.token.trim().length === 0 ||
      typeof parsed.server_id !== "string" ||
      !UUID_PATTERN.test(parsed.server_id) ||
      typeof parsed.expires_at_ms !== "number"
    ) {
      return undefined;
    }

    return {
      type: PAIRING_QR_TYPE,
      version: 1,
      ws_url: parsed.ws_url,
      token: parsed.token,
      server_id: parsed.server_id,
      expires_at_ms: parsed.expires_at_ms,
    };
  } catch {
    return undefined;
  }
}

function isSupportedWsUrl(value: string): boolean {
  return (value.startsWith("ws://") || value.startsWith("wss://")) && !/\s/.test(value) && !value.includes("#");
}
