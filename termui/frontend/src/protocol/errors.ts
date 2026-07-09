import type { ErrorPayload, SafeError } from "./types";

const SECRET_PATTERNS = [/token/i, /secret/i, /private/i, /signature/i, /ciphertext/i, /authorization/i, /bearer/i];
const FALLBACK_ERROR_CODE = "protocol_error";
const FALLBACK_ERROR_MESSAGE = "protocol operation failed";

export class ProtocolClientError extends Error implements SafeError {
  public readonly code: string;

  constructor(code: string, message: string) {
    super(safeMessage(message));
    this.name = "ProtocolClientError";
    this.code = safeCode(code);
  }
}

export function protocolError(payload: ErrorPayload): ProtocolClientError {
  return new ProtocolClientError(payload.code, payload.message);
}

export function safeCode(code: string): string {
  // error code 会直接进入 UI；即使 daemon/relay 返回异常 code，也不能让敏感字段穿透。
  if (!code || SECRET_PATTERNS.some((pattern) => pattern.test(code))) {
    return FALLBACK_ERROR_CODE;
  }
  return code;
}

export function safeMessage(message: string): string {
  if (SECRET_PATTERNS.some((pattern) => pattern.test(message))) {
    return FALLBACK_ERROR_MESSAGE;
  }
  return message || FALLBACK_ERROR_MESSAGE;
}

export function toSafeError(error: unknown): SafeError {
  if (error instanceof ProtocolClientError) {
    return { code: error.code, message: error.message };
  }
  if (error instanceof Error) {
    return { code: "client_error", message: safeMessage(error.message) };
  }
  return { code: "client_error", message: "protocol operation failed" };
}
