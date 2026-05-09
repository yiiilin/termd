export function registerTermdServiceWorker(): void {
  if (!("serviceWorker" in navigator)) {
    return;
  }

  // Service worker 只缓存 Web UI 外壳；协议、WebSocket 和 pairing/auth 请求不走缓存。
  void navigator.serviceWorker.register("/service-worker.js").catch(() => undefined);
}
