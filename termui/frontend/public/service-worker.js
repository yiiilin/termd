const CACHE_PREFIX = "termd-";

// 终端前端和 daemon/relay 协议版本强相关；旧 PWA 缓存继续运行旧 bundle 时，
// 会把浏览器卡在旧 WebSocket/attach 流程里。这里保留 service worker 文件只用于
// 接管历史注册并清理缓存，随后立即注销，让所有资源都回到服务端 no-store 策略。
self.addEventListener("install", (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key.startsWith(CACHE_PREFIX)).map((key) => caches.delete(key))))
      .then(() => self.registration.unregister()),
  );
});
