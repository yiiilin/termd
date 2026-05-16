import { describe, expect, it } from "vitest";
import { createTranslator, translateSafeErrorMessage } from "../i18n";

describe("i18n", () => {
  it("按当前语言翻译常见安全错误消息，同时保留调用方展示错误码", () => {
    const zh = createTranslator("zh-CN");
    const en = createTranslator("en-US");

    expect(translateSafeErrorMessage({ code: "missing_pairing", message: "device is not paired" }, zh)).toBe(
      "设备尚未配对",
    );
    expect(
      translateSafeErrorMessage(
        { code: "pairing_payload_server_mismatch", message: "pairing payload does not match the connected daemon" },
        zh,
      ),
    ).toBe("配对内容与当前连接的守护进程不匹配");
    expect(
      translateSafeErrorMessage({ code: "file_too_large", message: "file is too large to edit in browser" }, zh),
    ).toBe("文件过大，无法在浏览器中编辑");
    expect(
      translateSafeErrorMessage(
        { code: "file_too_large", message: "browser streaming download is unavailable for this file" },
        zh,
      ),
    ).toBe("当前浏览器无法下载这个大文件");
    expect(
      translateSafeErrorMessage(
        { code: "pairing_server_unknown", message: "pairing requires a known daemon server id" },
        en,
      ),
    ).toBe("pairing requires a known daemon server id");
  });
});
