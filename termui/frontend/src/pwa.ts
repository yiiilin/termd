export function registerTermdServiceWorker(): void {
  if (!("serviceWorker" in navigator)) {
    return;
  }

  // 清理历史全局 PWA worker，但保留每个 daemon 独立的 Web Push scope。
  void cleanupLegacyTermdServiceWorkers().catch(() => undefined);
}

async function cleanupLegacyTermdServiceWorkers(): Promise<void> {
  const serviceWorker = navigator.serviceWorker;
  const registrations = await serviceWorker.getRegistrations?.().catch(() => []);
  await Promise.all(
    (registrations ?? [])
      .filter((registration) => !isPushWorkerScope(registration.scope))
      .map((registration) => registration.unregister().catch(() => false)),
  );

  if ("caches" in globalThis) {
    const keys = await caches.keys().catch(() => []);
    await Promise.all(
      keys
        .filter((key) => key.startsWith("termd-"))
        .map((key) => caches.delete(key).catch(() => false)),
    );
  }
}

function isPushWorkerScope(scope: string): boolean {
  try {
    return new URL(scope).pathname.includes("/.termd-push/");
  } catch {
    return false;
  }
}
