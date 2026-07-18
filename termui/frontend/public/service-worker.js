self.addEventListener("install", (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener("push", (event) => {
  event.waitUntil(handlePush(event));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(handleNotificationClick(event.notification.data));
});

async function handlePush(event) {
  const scope = scopedDaemon();
  const payload = readPushPayload(event.data, scope?.serverId);
  if (!scope || !payload) {
    return;
  }
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  if (windows.some((client) => client.visibilityState === "visible" && isApplicationClient(client.url, scope.base))) {
    return;
  }
  await self.registration.showNotification("Termd", {
    body: payload.body,
    tag: `termd-session-activity-${payload.server_id}-${payload.session_id}`,
    icon: new URL("icons/termd.svg", scope.base).toString(),
    silent: true,
    data: {
      server_id: payload.server_id,
      session_id: payload.session_id,
    },
  });
}

async function handleNotificationClick(data) {
  const scope = scopedDaemon();
  if (!scope || !isNotificationData(data) || data.server_id !== scope.serverId) {
    return;
  }
  const target = new URL(scope.base);
  target.searchParams.set("termd_server_id", data.server_id);
  target.searchParams.set("termd_session_id", data.session_id);
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  const existing = windows.find((client) => isApplicationClient(client.url, scope.base));
  if (existing) {
    if (typeof existing.navigate === "function") {
      await existing.navigate(target.toString());
    }
    if (typeof existing.focus === "function") {
      await existing.focus();
    }
    return;
  }
  await self.clients.openWindow(target.toString());
}

function scopedDaemon() {
  const scope = new URL(self.registration.scope);
  const marker = "/.termd-push/";
  const markerIndex = scope.pathname.lastIndexOf(marker);
  if (markerIndex < 0) {
    return undefined;
  }
  const serverId = decodeURIComponent(scope.pathname.slice(markerIndex + marker.length).split("/")[0] || "");
  if (!isUuid(serverId)) {
    return undefined;
  }
  scope.pathname = `${scope.pathname.slice(0, markerIndex)}/`;
  scope.search = "";
  scope.hash = "";
  return { serverId, base: scope.toString() };
}

function readPushPayload(data, scopedServerId) {
  if (!data || !scopedServerId) {
    return undefined;
  }
  let payload;
  try {
    payload = data.json();
  } catch {
    return undefined;
  }
  if (
    payload?.version !== 1 ||
    payload.server_id !== scopedServerId ||
    !isUuid(payload.session_id) ||
    typeof payload.body !== "string" ||
    payload.body.length === 0 ||
    payload.body.length > 500
  ) {
    return undefined;
  }
  return payload;
}

function isNotificationData(data) {
  return Boolean(data && isUuid(data.server_id) && isUuid(data.session_id));
}

function isApplicationClient(rawUrl, baseUrl) {
  try {
    const client = new URL(rawUrl);
    const base = new URL(baseUrl);
    return client.origin === base.origin && client.pathname.startsWith(base.pathname) && !client.pathname.includes("/.termd-push/");
  } catch {
    return false;
  }
}

function isUuid(value) {
  return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
}
