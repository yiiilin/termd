import type {
  BrowserNotificationPreference,
  DeviceState,
  PairedServerState,
} from "./protocol/types";
import { V070Client } from "./protocol/v070-client";

export interface SerializedPushSubscription {
  endpoint: string;
  keys: {
    p256dh: string;
    auth: string;
  };
}

interface PushConfigResponse {
  server_id: string;
  application_server_key: string;
  subscribed: boolean;
}

const REMOTE_PUSH_CLEANUP_TIMEOUT_MS = 5_000;

export interface BrowserPushPreferenceSync {
  device: DeviceState;
  servers: readonly PairedServerState[];
  preference: BrowserNotificationPreference;
  locale: "zh-CN" | "en-US";
}

export function supportsBrowserPush(): boolean {
  return (
    globalThis.isSecureContext === true &&
    typeof navigator !== "undefined" &&
    "serviceWorker" in navigator &&
    typeof PushManager !== "undefined" &&
    typeof Notification !== "undefined"
  );
}

export function pushServiceWorkerScope(serverId: string, pageUrl = globalThis.location.href): string {
  const base = applicationBaseUrl(pageUrl);
  return new URL(`.termd-push/${encodeURIComponent(serverId)}/`, base).toString();
}

export async function subscribeBrowserPush(
  serverId: string,
  applicationServerKey: string,
): Promise<SerializedPushSubscription> {
  if (!supportsBrowserPush()) {
    throw new Error("browser_push_unavailable");
  }
  const base = applicationBaseUrl(globalThis.location.href);
  const scope = pushServiceWorkerScope(serverId, globalThis.location.href);
  const registration = await navigator.serviceWorker.register(
    new URL("service-worker.js", base).toString(),
    { scope },
  );
  await waitForActiveWorker(registration);

  const applicationServerKeyBytes = base64UrlToBytes(applicationServerKey);
  let subscription = await registration.pushManager.getSubscription();
  if (subscription && !subscriptionMatchesKey(subscription, applicationServerKeyBytes)) {
    await subscription.unsubscribe();
    subscription = null;
  }
  subscription ??= await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: applicationServerKeyBytes,
  });
  return serializeSubscription(subscription);
}

export async function unsubscribeBrowserPush(serverId: string): Promise<void> {
  if (typeof navigator === "undefined" || !("serviceWorker" in navigator)) {
    return;
  }
  const scope = pushServiceWorkerScope(serverId, globalThis.location.href);
  const container = navigator.serviceWorker;
  const registration = container.getRegistration
    ? await container.getRegistration(scope)
    : (await container.getRegistrations()).find((candidate) => candidate.scope === scope);
  if (!registration) {
    return;
  }
  const subscription = await registration.pushManager.getSubscription();
  await subscription?.unsubscribe();
  await registration.unregister();
}

export async function syncBrowserPushPreference(input: BrowserPushPreferenceSync): Promise<void> {
  const servers = input.servers.filter((server) => Boolean(server.device_certificate));
  if (
    input.preference !== "off" &&
    (!supportsBrowserPush() || Notification.permission !== "granted")
  ) {
    return;
  }
  await Promise.allSettled(servers.map((server) => syncServerPushPreference(server, input)));
}

async function syncServerPushPreference(
  server: PairedServerState,
  input: BrowserPushPreferenceSync,
): Promise<void> {
  let client: V070Client | undefined;
  if (input.preference === "off") {
    await removeBrowserPushForServer(server, input.device);
    return;
  }

  try {
    client = await V070Client.connect(server, input.device);
    const configResponse = await client.requestPush("/api/push/config", { method: "GET" });
    if (!configResponse.ok) {
      throw new Error("push_config_failed");
    }
    const config = await configResponse.json() as Partial<PushConfigResponse>;
    if (
      config.server_id !== server.server_id ||
      typeof config.application_server_key !== "string" ||
      !config.application_server_key
    ) {
      throw new Error("push_config_invalid");
    }
    const subscription = await subscribeBrowserPush(server.server_id, config.application_server_key);
    const response = await client.requestPush("/api/push/subscription", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        ...subscription,
        mode: input.preference === "mentions" ? "attention" : "all",
        locale: input.locale,
      }),
    });
    if (!response.ok) {
      throw new Error("push_subscription_update_failed");
    }
  } finally {
    client?.close();
  }
}

export async function removeBrowserPushForServer(
  server: PairedServerState,
  device?: DeviceState,
): Promise<void> {
  const endpoint = await browserPushEndpoint(server.server_id).catch(() => undefined);
  const remoteRemoved = device && server.device_certificate && endpoint
    ? await removeRemotePushSubscription(server, device, endpoint)
    : false;
  try {
    await unsubscribeBrowserPush(server.server_id);
  } catch (caught) {
    if (!remoteRemoved) {
      throw caught;
    }
    throw new Error("push_worker_cleanup_failed");
  }
}

async function removeRemotePushSubscription(
  server: PairedServerState,
  device: DeviceState,
  endpoint: string,
): Promise<boolean> {
  const controller = new AbortController();
  return settleRemoteRemoval(
    deleteRemotePushSubscription(server, device, endpoint, controller.signal),
    controller,
  );
}

async function deleteRemotePushSubscription(
  server: PairedServerState,
  device: DeviceState,
  endpoint: string,
  signal: AbortSignal,
): Promise<void> {
  const client = await V070Client.connect(server, device);
  try {
    const response = await client.requestPush("/api/push/subscription", {
      method: "DELETE",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ endpoint }),
      signal,
    });
    if (!response.ok) {
      throw new Error("push_subscription_delete_failed");
    }
  } finally {
    client.close();
  }
}

async function browserPushEndpoint(serverId: string): Promise<string | undefined> {
  if (typeof navigator === "undefined" || !("serviceWorker" in navigator)) {
    return undefined;
  }
  const scope = pushServiceWorkerScope(serverId, globalThis.location.href);
  const container = navigator.serviceWorker;
  const registration = container.getRegistration
    ? await container.getRegistration(scope)
    : (await container.getRegistrations()).find((candidate) => candidate.scope === scope);
  const subscription = await registration?.pushManager.getSubscription();
  return subscription?.endpoint || undefined;
}

async function settleRemoteRemoval(
  removal: Promise<void>,
  controller: AbortController,
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    let settled = false;
    const finish = (removed: boolean) => {
      if (settled) {
        return;
      }
      settled = true;
      globalThis.clearTimeout(timeout);
      resolve(removed);
    };
    const timeout = globalThis.setTimeout(() => {
      controller.abort();
      finish(false);
    }, REMOTE_PUSH_CLEANUP_TIMEOUT_MS);
    void removal.then(() => finish(true), () => finish(false));
  });
}

function applicationBaseUrl(pageUrl: string): URL {
  const page = new URL(pageUrl);
  page.search = "";
  page.hash = "";
  return new URL("./", page);
}

async function waitForActiveWorker(registration: ServiceWorkerRegistration): Promise<void> {
  if (registration.active) {
    return;
  }
  const worker = registration.installing ?? registration.waiting;
  if (!worker) {
    throw new Error("service_worker_not_active");
  }
  if (worker.state === "activated") {
    return;
  }
  await new Promise<void>((resolve, reject) => {
    let timeout: ReturnType<typeof globalThis.setTimeout> | undefined;
    const handleStateChange = () => {
      if (worker.state === "activated") {
        if (timeout !== undefined) {
          globalThis.clearTimeout(timeout);
        }
        worker.removeEventListener("statechange", handleStateChange);
        resolve();
      } else if (worker.state === "redundant") {
        if (timeout !== undefined) {
          globalThis.clearTimeout(timeout);
        }
        worker.removeEventListener("statechange", handleStateChange);
        reject(new Error("service_worker_activation_failed"));
      }
    };
    worker.addEventListener("statechange", handleStateChange);
    handleStateChange();
    if (worker.state === "activated" || worker.state === "redundant") {
      return;
    }
    timeout = globalThis.setTimeout(() => {
      worker.removeEventListener("statechange", handleStateChange);
      reject(new Error("service_worker_activation_timeout"));
    }, 10_000);
  });
}

function subscriptionMatchesKey(subscription: PushSubscription, expected: Uint8Array): boolean {
  const current = subscription.options?.applicationServerKey;
  if (!current) {
    return true;
  }
  const bytes = ArrayBuffer.isView(current)
    ? new Uint8Array(current.buffer, current.byteOffset, current.byteLength)
    : new Uint8Array(current);
  return bytes.length === expected.length && bytes.every((byte, index) => byte === expected[index]);
}

function serializeSubscription(subscription: PushSubscription): SerializedPushSubscription {
  const json = subscription.toJSON();
  const p256dh = json.keys?.p256dh ?? keyToBase64Url(subscription.getKey("p256dh"));
  const auth = json.keys?.auth ?? keyToBase64Url(subscription.getKey("auth"));
  if (!subscription.endpoint || !p256dh || !auth) {
    throw new Error("push_subscription_invalid");
  }
  return {
    endpoint: subscription.endpoint,
    keys: { p256dh, auth },
  };
}

function keyToBase64Url(value: ArrayBuffer | null): string | undefined {
  return value ? bytesToBase64Url(new Uint8Array(value)) : undefined;
}

function base64UrlToBytes(value: string): Uint8Array<ArrayBuffer> {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(value.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function bytesToBase64Url(value: Uint8Array): string {
  let binary = "";
  for (const byte of value) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
}
