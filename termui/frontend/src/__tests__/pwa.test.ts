import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerTermdServiceWorker } from "../pwa";

describe("PWA 外壳", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("注册 service worker 时使用静态根路径，便于 embedded Web UI 缓存资源", async () => {
    const update = vi.fn(() => Promise.resolve());
    const register = vi.fn(() => Promise.resolve({ update }));
    vi.stubGlobal("navigator", {
      serviceWorker: { register },
    });

    registerTermdServiceWorker();
    await Promise.resolve();

    expect(register).toHaveBeenCalledWith("/service-worker.js");
    expect(update).toHaveBeenCalledTimes(1);
  });

  it("manifest 声明可安装的 termd Web 应用", async () => {
    const raw = await readFile(resolve(process.cwd(), "public/manifest.webmanifest"), "utf8");
    const manifest = JSON.parse(raw) as {
      name?: string;
      short_name?: string;
      start_url?: string;
      display?: string;
      icons?: Array<{ src: string; purpose?: string }>;
    };

    expect(manifest.name).toBe("Termd");
    expect(manifest.short_name).toBe("Termd");
    expect(manifest.start_url).toBe("/");
    expect(manifest.display).toBe("standalone");
    expect(manifest.icons?.some((icon) => icon.src === "/icons/termd.svg" && icon.purpose?.includes("maskable"))).toBe(true);
  });

  it("service worker 对前端资源使用网络优先，避免 PWA 长期运行旧 JS", async () => {
    const source = await readFile(resolve(process.cwd(), "public/service-worker.js"), "utf8");

    expect(source).toContain('const CACHE_NAME = "termd-web-shell-v2"');
    expect(source).toContain('event.respondWith(networkFirst(request, request))');
    expect(source).not.toContain("event.respondWith(cacheFirst(request))");
  });
});
