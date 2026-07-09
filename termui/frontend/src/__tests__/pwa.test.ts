import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerTermdServiceWorker } from "../pwa";

describe("PWA 外壳", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("注册清理型 service worker 并注销旧 PWA 缓存", async () => {
    const update = vi.fn(() => Promise.resolve());
    const unregister = vi.fn(() => Promise.resolve(true));
    const register = vi.fn(() => Promise.resolve({ update, unregister }));
    const getRegistrations = vi.fn(() => Promise.resolve([{ unregister }]));
    const keys = vi.fn(() => Promise.resolve(["termd-web-shell-v1", "other-cache"]));
    const deleteCache = vi.fn(() => Promise.resolve(true));
    vi.stubGlobal("navigator", {
      serviceWorker: { register, getRegistrations },
    });
    vi.stubGlobal("caches", {
      keys,
      delete: deleteCache,
    });

    registerTermdServiceWorker();
    await vi.waitFor(() => expect(unregister).toHaveBeenCalledTimes(1));

    expect(register).toHaveBeenCalledWith("./service-worker.js");
    expect(update).not.toHaveBeenCalled();
    expect(getRegistrations).toHaveBeenCalledTimes(1);
    expect(deleteCache).toHaveBeenCalledWith("termd-web-shell-v1");
    expect(deleteCache).not.toHaveBeenCalledWith("other-cache");
  });

  it("清理型 service worker 不强制 update，避免注销竞态污染页面错误", async () => {
    // 中文注释：Chromium 可能在清理型 SW 立即 unregister 的竞态中让 update() reject。
    // register() 已足够触发清理脚本，后续注销流程不应再主动 update。
    const update = vi.fn(() => Promise.reject(new Error("invalid state")));
    const unregister = vi.fn(() => Promise.resolve(true));
    const register = vi.fn(() => Promise.resolve({ update, unregister }));
    const getRegistrations = vi.fn(() => Promise.resolve([{ unregister }]));
    vi.stubGlobal("navigator", {
      serviceWorker: { register, getRegistrations },
    });

    registerTermdServiceWorker();
    await vi.waitFor(() => expect(unregister).toHaveBeenCalledTimes(1));

    expect(update).not.toHaveBeenCalled();
    expect(getRegistrations).toHaveBeenCalledTimes(1);
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

  it("service worker 只负责清理历史缓存，不再拦截前端资源", async () => {
    const source = await readFile(resolve(process.cwd(), "public/service-worker.js"), "utf8");

    expect(source).toContain('const CACHE_PREFIX = "termd-"');
    expect(source).toContain("self.registration.unregister()");
    expect(source).not.toContain("clients.claim()");
    expect(source).not.toContain("event.respondWith(");
    expect(source).not.toContain("event.respondWith(cacheFirst(request))");
  });
});
