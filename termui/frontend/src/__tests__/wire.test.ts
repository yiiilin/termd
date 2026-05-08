import { describe, expect, it, vi } from "vitest";
import { randomUuid } from "../protocol/wire";

describe("Web wire 随机标识", () => {
  it("randomUUID 不存在时使用 getRandomValues 生成 v4 UUID", () => {
    const originalCrypto = globalThis.crypto;
    const getRandomValues = vi.fn((target: Uint8Array) => {
      target.set([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0xff,
      ]);
      return target;
    });

    vi.stubGlobal("crypto", { getRandomValues });

    try {
      expect(randomUuid()).toBe("00112233-4455-4677-8899-aabbccddeeff");
      expect(getRandomValues).toHaveBeenCalledTimes(1);
    } finally {
      vi.stubGlobal("crypto", originalCrypto);
    }
  });

  it("没有安全随机源时给出明确错误", () => {
    const originalCrypto = globalThis.crypto;
    vi.stubGlobal("crypto", {});

    try {
      expect(() => randomUuid()).toThrow("web_crypto_unavailable");
    } finally {
      vi.stubGlobal("crypto", originalCrypto);
    }
  });
});
