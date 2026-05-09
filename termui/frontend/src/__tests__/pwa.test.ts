import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerTermdServiceWorker } from "../pwa";

describe("PWA 外壳", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("注册 service worker 时使用静态根路径，便于 embedded Web UI 缓存资源", () => {
    const register = vi.fn(() => Promise.resolve({}));
    vi.stubGlobal("navigator", {
      serviceWorker: { register },
    });

    registerTermdServiceWorker();

    expect(register).toHaveBeenCalledWith("/service-worker.js");
  });

  it("manifest 声明可安装的 termd Web 应用", async () => {
    const raw = await readFile(resolve(process.cwd(), "public/manifest.webmanifest"), "utf8");
    const manifest = JSON.parse(raw) as {
      name?: string;
      start_url?: string;
      display?: string;
      icons?: Array<{ src: string; purpose?: string }>;
    };

    expect(manifest.name).toBe("termd");
    expect(manifest.start_url).toBe("/");
    expect(manifest.display).toBe("standalone");
    expect(manifest.icons?.some((icon) => icon.src === "/icons/termd.svg" && icon.purpose?.includes("maskable"))).toBe(true);
  });
});
