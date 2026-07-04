import type { PairingQrPayload } from "./types";
import { base64ToBytes, decodeUtf8 } from "./wire";

const PAIRING_QR_TYPE = "termd_pairing_qr";
const PAIRING_INVITE_PREFIXES = ["termd-pair:v2:", "termd-pair:v1:"];
const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function parsePairingQrPayload(raw: string): PairingQrPayload | undefined {
  const trimmed = raw.trim();
  const payloadText = decodePastedPairingPayload(trimmed);
  if (!payloadText) {
    return undefined;
  }

  try {
    const parsed = JSON.parse(payloadText) as Partial<PairingQrPayload> & { type?: string };
    if (
      parsed.type !== PAIRING_QR_TYPE ||
      (parsed.version !== 1 && parsed.version !== 2) ||
      typeof parsed.token !== "string" ||
      parsed.token.trim().length === 0 ||
      typeof parsed.server_id !== "string" ||
      !UUID_PATTERN.test(parsed.server_id) ||
      (parsed.daemon_public_key !== undefined &&
        (typeof parsed.daemon_public_key !== "string" ||
          !parsed.daemon_public_key.startsWith("ed25519-v1:"))) ||
      typeof parsed.expires_at_ms !== "number" ||
      (parsed.ws_url !== undefined &&
        (typeof parsed.ws_url !== "string" || !isSupportedWsUrl(parsed.ws_url)))
    ) {
      return undefined;
    }
    if (parsed.version === 1 && !parsed.daemon_public_key) {
      return undefined;
    }

    const payload: PairingQrPayload = {
      type: PAIRING_QR_TYPE,
      version: parsed.version,
      token: parsed.token,
      server_id: parsed.server_id,
      expires_at_ms: parsed.expires_at_ms,
    };

    if (parsed.daemon_public_key) {
      payload.daemon_public_key = parsed.daemon_public_key;
    }
    if (parsed.ws_url) {
      // ws_url 仅兼容旧邀请码；真正连接入口由调用方结合 server_id 规范化到统一 /ws。
      payload.ws_url = parsed.ws_url;
    }

    return payload;
  } catch {
    return undefined;
  }
}

function decodePastedPairingPayload(trimmed: string): string | undefined {
  for (const prefix of PAIRING_INVITE_PREFIXES) {
    if (trimmed.startsWith(prefix)) {
      return decodeInviteCode(trimmed.slice(prefix.length));
    }
  }

  // 兼容旧版 termd pair --qr 输出的明文 JSON；新版本默认会输出单行邀请码。
  if (trimmed.startsWith("{")) {
    return trimmed;
  }

  return undefined;
}

function decodeInviteCode(encoded: string): string | undefined {
  if (!encoded || !/^[A-Za-z0-9_-]+$/.test(encoded)) {
    return undefined;
  }

  const base64 = encoded.replaceAll("-", "+").replaceAll("_", "/");
  const paddingLength = (4 - (base64.length % 4)) % 4;

  try {
    return decodeUtf8(base64ToBytes(`${base64}${"=".repeat(paddingLength)}`));
  } catch {
    return undefined;
  }
}

function isSupportedWsUrl(value: string): boolean {
  return (value.startsWith("ws://") || value.startsWith("wss://")) && !/\s/.test(value) && !value.includes("#");
}
