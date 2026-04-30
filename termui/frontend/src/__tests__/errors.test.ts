import { describe, expect, it } from "vitest";
import { protocolError, toSafeError } from "../protocol/errors";

describe("SafeError 脱敏", () => {
  it("脱敏 error code 和 message 中的 token、私钥、签名、密文和终端明文", () => {
    const error = protocolError({
      code: "token_private_signature_ciphertext",
      message:
        "token=secret-token server_private_key=private-value signature=sig ciphertext_base64=abc terminal-secret",
    });

    expect(error.code).toBe("protocol_error");
    expect(error.message).toBe("protocol operation failed");
  });

  it("普通 Error 进入 UI 状态前也会脱敏", () => {
    const safe = toSafeError(new Error("terminal-secret signature=sig private key"));

    expect(safe).toEqual({
      code: "client_error",
      message: "protocol operation failed",
    });
  });
});
