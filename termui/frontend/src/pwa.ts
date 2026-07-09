export function registerTermdServiceWorker(): void {
  if (!("serviceWorker" in navigator)) {
    return;
  }

  // 旧版本曾注册 PWA shell cache；新版前端启动时主动清理，避免用户浏览器继续运行
  // 与当前 daemon/relay 协议不匹配的旧 bundle。IndexedDB 配对状态不在 Cache API 里。
  void cleanupTermdServiceWorkers();
}

async function cleanupTermdServiceWorkers(): Promise<void> {
  const serviceWorker = navigator.serviceWorker;

  // 中文注释：register() 本身会安装/更新这份清理型 SW；这里随后就要注销历史注册，
  // 不能再强制 update()，否则 Chromium 可能在 update/unregister 竞态中抛 pageerror。
  const registration = await serviceWorker.register("./service-worker.js").catch(() => undefined);

  const registrations = await serviceWorker.getRegistrations?.().catch(() => undefined);
  const candidates = registrations ?? (registration ? [registration] : []);
  await Promise.all(candidates.map((candidate) => candidate.unregister().catch(() => false)));

  if ("caches" in globalThis) {
    const keys = await caches.keys().catch(() => []);
    await Promise.all(
      keys
        .filter((key) => key.startsWith("termd-"))
        .map((key) => caches.delete(key).catch(() => false)),
    );
  }
}
