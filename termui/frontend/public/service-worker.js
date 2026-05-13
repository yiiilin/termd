const CACHE_NAME = "termd-web-shell-v2";
const SHELL_URLS = ["/", "/manifest.webmanifest", "/icons/termd.svg"];
const NEVER_CACHE_PATHS = new Set(["/ws", "/healthz", "/local/pairing-token"]);

// 只把 Web UI 外壳作为离线兜底；正常在线访问必须优先走网络，
// 避免 PWA 或 Safari 长期使用旧 JS 导致终端交互行为和 daemon 不一致。
self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) => cache.addAll(SHELL_URLS))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== CACHE_NAME).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);

  if (request.method !== "GET" || url.origin !== self.location.origin || NEVER_CACHE_PATHS.has(url.pathname)) {
    return;
  }

  if (request.mode === "navigate") {
    event.respondWith(networkFirst(request, "/"));
    return;
  }

  if (url.pathname.startsWith("/assets/") || SHELL_URLS.includes(url.pathname)) {
    event.respondWith(networkFirst(request, request));
  }
});

async function networkFirst(request, cacheKey) {
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(CACHE_NAME);
      await cache.put(cacheKey, response.clone());
    }
    return response;
  } catch {
    return (await caches.match(cacheKey)) || Response.error();
  }
}
