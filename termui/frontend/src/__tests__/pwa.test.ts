import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerTermdServiceWorker } from "../pwa";

describe("PWA 外壳", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("清理旧 service worker 和 PWA 缓存但保留 scoped Push worker", async () => {
    const unregisterLegacy = vi.fn(() => Promise.resolve(true));
    const unregisterPush = vi.fn(() => Promise.resolve(true));
    const getRegistrations = vi.fn(() => Promise.resolve([
      { scope: "https://termd.test/", unregister: unregisterLegacy },
      { scope: "https://termd.test/.termd-push/server-1/", unregister: unregisterPush },
    ]));
    const keys = vi.fn(() => Promise.resolve(["termd-web-shell-v1", "other-cache"]));
    const deleteCache = vi.fn(() => Promise.resolve(true));
    vi.stubGlobal("navigator", {
      serviceWorker: { getRegistrations },
    });
    vi.stubGlobal("caches", {
      keys,
      delete: deleteCache,
    });

    registerTermdServiceWorker();
    await vi.waitFor(() => expect(unregisterLegacy).toHaveBeenCalledTimes(1));

    expect(unregisterPush).not.toHaveBeenCalled();
    expect(getRegistrations).toHaveBeenCalledTimes(1);
    expect(deleteCache).toHaveBeenCalledWith("termd-web-shell-v1");
    expect(deleteCache).not.toHaveBeenCalledWith("other-cache");
  });

  it("service worker 查询失败不会影响应用启动", async () => {
    const getRegistrations = vi.fn(() => Promise.reject(new Error("unavailable")));
    vi.stubGlobal("navigator", {
      serviceWorker: { getRegistrations },
    });

    registerTermdServiceWorker();
    await vi.waitFor(() => expect(getRegistrations).toHaveBeenCalledTimes(1));
  });

  it("manifest 声明可安装的 termd Web 应用", async () => {
    const raw = await readFile(resolve(process.cwd(), "public/manifest.webmanifest"), "utf8");
    const manifest = JSON.parse(raw) as {
      name?: string;
      short_name?: string;
      start_url?: string;
      display?: string;
      scope?: string;
      icons?: Array<{ src: string; purpose?: string }>;
    };

    expect(manifest.name).toBe("Termd");
    expect(manifest.short_name).toBe("Termd");
    expect(manifest.start_url).toBe("./");
    expect(manifest.scope).toBe("./");
    expect(manifest.display).toBe("standalone");
    expect(manifest.icons?.some((icon) => icon.src === "./icons/termd.svg" && icon.purpose?.includes("maskable"))).toBe(true);
  });

  it("service worker 只处理 Push，不缓存或拦截前端资源", async () => {
    const source = await readFile(resolve(process.cwd(), "public/service-worker.js"), "utf8");

    expect(source).toContain('self.addEventListener("push"');
    expect(source).toContain('self.addEventListener("notificationclick"');
    expect(source).toContain("self.registration.showNotification");
    expect(source).not.toContain('self.addEventListener("fetch"');
    expect(source).not.toContain("self.registration.unregister()");
    expect(source).not.toContain("caches.");
    expect(source).not.toContain("event.respondWith(");
  });
});
