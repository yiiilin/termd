import { generateDeviceIdentity } from "../protocol/auth";
import type {
  BrowserLanguagePreference,
  BrowserPreferences,
  BrowserState,
  BrowserThemePreference,
  DeviceState,
  PairAcceptPayload,
  PairedServerState,
} from "../protocol/types";

const DB_NAME = "termd-termui-web";
const DB_VERSION = 1;
const STORE_NAME = "state";
const STATE_KEY = "current";

export const DEFAULT_BROWSER_PREFERENCES: BrowserPreferences = {
  language: "auto",
  theme: "system",
  notifications: "off",
  mobileShortcuts: [],
};

export async function loadBrowserState(): Promise<BrowserState> {
  const db = await openStateDb();
  const state = await requestToPromise<BrowserState | undefined>(
    db.transaction(STORE_NAME, "readonly").objectStore(STORE_NAME).get(STATE_KEY),
  );
  db.close();
  return normalizeState(state ?? { pairedServers: [] });
}

export async function saveBrowserState(state: BrowserState): Promise<void> {
  const db = await openStateDb();
  await requestToPromise(
    db.transaction(STORE_NAME, "readwrite").objectStore(STORE_NAME).put(normalizeState(state), STATE_KEY),
  );
  db.close();
}

export async function ensureDevice(): Promise<DeviceState> {
  const state = await loadBrowserState();
  if (state.device) {
    if (!normalizeDeviceName(state.device.name)) {
      const device = { ...state.device, name: defaultDeviceName() };
      await saveBrowserState({ ...state, device });
      return device;
    }
    return state.device;
  }

  const device = { ...(await generateDeviceIdentity()), name: defaultDeviceName() };
  await saveBrowserState({ ...state, device });
  return device;
}

export async function recordPairing(accepted: PairAcceptPayload, url: string): Promise<BrowserState> {
  const state = await loadBrowserState();
  const normalizedUrl = normalizeRouteWsUrl(url, accepted.server_id);
  const existing = state.pairedServers.find((server) => server.server_id === accepted.server_id);
  const server: PairedServerState = {
    server_id: accepted.server_id,
    daemon_public_key: accepted.daemon_public_key,
    url: normalizedUrl,
    paired_at_ms: Date.now(),
    ...(existing?.name ? { name: existing.name } : {}),
  };
  const pairedServers = [
    ...state.pairedServers.filter((existing) => existing.server_id !== server.server_id),
    server,
  ];
  const next = normalizeState({
    ...state,
    pairedServers,
    defaultServerId: accepted.server_id,
    defaultUrl: normalizedUrl,
  });
  await saveBrowserState(next);
  return next;
}

export async function renameDaemon(serverId: string, name: string): Promise<BrowserState> {
  const cleanName = normalizeDaemonName(name);
  const state = await loadBrowserState();
  const next = normalizeState({
    ...state,
    pairedServers: state.pairedServers.map((server) =>
      server.server_id === serverId
        ? {
            ...server,
            name: cleanName,
          }
        : server,
    ),
  });
  await saveBrowserState(next);
  return next;
}

export async function forgetDaemon(serverId: string): Promise<BrowserState> {
  const state = await loadBrowserState();
  const pairedServers = state.pairedServers.filter((server) => server.server_id !== serverId);
  const defaultServer =
    pairedServers.find((server) => server.server_id === state.defaultServerId) ?? pairedServers.at(0);
  const next = normalizeState({
    ...state,
    pairedServers,
    defaultServerId: defaultServer?.server_id,
    defaultUrl: defaultServer?.url,
  });
  await saveBrowserState(next);
  return next;
}

export async function recordServerUrl(serverId: string, url: string): Promise<BrowserState> {
  const cleanUrl = normalizeRouteWsUrl(url, serverId);
  const state = await loadBrowserState();
  const next = normalizeState({
    ...state,
    pairedServers: state.pairedServers.map((server) =>
      server.server_id === serverId ? { ...server, url: cleanUrl } : server,
    ),
    defaultServerId: serverId,
    defaultUrl: cleanUrl,
  });
  await saveBrowserState(next);
  return next;
}

export async function saveBrowserPreferences(preferences: BrowserPreferences): Promise<BrowserState> {
  const state = await loadBrowserState();
  const next = normalizeState({
    ...state,
    preferences,
  });
  await saveBrowserState(next);
  return next;
}

export async function selectDefaultServer(serverId: string): Promise<BrowserState> {
  const state = await loadBrowserState();
  const server = state.pairedServers.find((candidate) => candidate.server_id === serverId);
  if (!server) {
    return state;
  }

  const next = normalizeState({
    ...state,
    defaultServerId: server.server_id,
    defaultUrl: server.url,
  });
  await saveBrowserState(next);
  return next;
}

export async function clearBrowserState(): Promise<void> {
  const db = await openStateDb();
  await requestToPromise(db.transaction(STORE_NAME, "readwrite").objectStore(STORE_NAME).clear());
  db.close();
}

export function defaultServer(state: BrowserState): PairedServerState | undefined {
  return (
    state.pairedServers.find((server) => server.server_id === state.defaultServerId) ?? state.pairedServers.at(0)
  );
}

function normalizeState(state: BrowserState): BrowserState {
  // IndexedDB 只保存设备身份和 daemon 公开身份；
  // session 文件树位置属于 daemon 共享状态，不能在单个浏览器里各存一份。
  // 这里显式按字段白名单重建对象，避免旧 schema 或污染对象把敏感字段重新写回。
  const pairedServers = (state.pairedServers ?? []).map((server) => {
    const name = normalizeDaemonName(server.name);
    return {
      server_id: server.server_id,
      daemon_public_key: server.daemon_public_key,
      url: normalizeRouteWsUrl(server.url, server.server_id),
      paired_at_ms: server.paired_at_ms,
      ...(name ? { name } : {}),
    };
  });
  const defaultUrlServerId = state.defaultServerId ?? pairedServers.at(0)?.server_id;
  const defaultUrl = state.defaultUrl
    ? normalizeRouteWsUrl(state.defaultUrl, defaultUrlServerId)
    : state.defaultUrl;

  return {
    device: state.device
      ? {
          device_id: state.device.device_id,
          device_public_key: state.device.device_public_key,
          device_signing_key_secret: state.device.device_signing_key_secret,
          ...(normalizeDeviceName(state.device.name) ? { name: normalizeDeviceName(state.device.name) } : {}),
        }
      : undefined,
    pairedServers,
    defaultServerId: state.defaultServerId,
    defaultUrl,
    preferences: normalizePreferences(state.preferences),
  };
}

function normalizePreferences(value: unknown): BrowserPreferences {
  const source = isObjectRecord(value) ? value : {};
  const language = normalizeLanguagePreference(source.language);
  const theme = normalizeThemePreference(source.theme);
  const notifications = normalizeNotificationPreference(source.notifications);
  const mobileShortcuts = normalizeMobileShortcuts(source.mobileShortcuts);
  return {
    language,
    theme,
    notifications,
    mobileShortcuts,
  };
}

function normalizeLanguagePreference(value: unknown): BrowserLanguagePreference {
  if (value === "zh-CN" || value === "en-US" || value === "auto") {
    return value;
  }
  return DEFAULT_BROWSER_PREFERENCES.language;
}

function normalizeThemePreference(value: unknown): BrowserThemePreference {
  if (value === "dark" || value === "light" || value === "system") {
    return value;
  }
  return DEFAULT_BROWSER_PREFERENCES.theme;
}

function normalizeNotificationPreference(value: unknown): BrowserPreferences["notifications"] {
  if (value === "off" || value === "mentions" || value === "all") {
    return value;
  }
  return DEFAULT_BROWSER_PREFERENCES.notifications;
}

function normalizeMobileShortcuts(value: unknown): BrowserPreferences["mobileShortcuts"] {
  if (!Array.isArray(value)) {
    return [];
  }

  return value
    .map((shortcut) => {
      if (!isObjectRecord(shortcut)) {
        return undefined;
      }
      const label = typeof shortcut.label === "string" ? shortcut.label.trim() : "";
      const data = typeof shortcut.data === "string" ? shortcut.data : "";
      if (!label || !data || label.length > 12 || data.length > 64 || [...data].some((character) => character === "\0")) {
        return undefined;
      }
      // 移动快捷键允许控制字符，但不允许空标签或过长数据，避免 IndexedDB 偏好被异常值污染。
      return { label, data };
    })
    .filter((shortcut): shortcut is NonNullable<typeof shortcut> => Boolean(shortcut))
    .slice(0, 12);
}

function isObjectRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export function normalizeRouteWsUrl(rawUrl: string, serverId?: string): string {
  const cleanUrl = rawUrl.trim();
  try {
    const parsed = new URL(cleanUrl);
    if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
      return cleanUrl;
    }

    const normalizedPath = parsed.pathname.replace(/\/+$/, "") || "/";
    if (serverId) {
      const legacySuffix = `/${serverId}/client`;
      const legacyBase = normalizedPath.endsWith(legacySuffix)
        ? normalizedPath.slice(0, -legacySuffix.length)
        : undefined;
      if (legacyBase?.endsWith("/ws")) {
        parsed.pathname = legacyBase;
        return parsed.toString();
      }
    }

    if (normalizedPath === "/") {
      parsed.pathname = "/ws";
      return parsed.toString();
    }

    if (normalizedPath.endsWith("/ws")) {
      parsed.pathname = normalizedPath;
      return parsed.toString();
    }
    return cleanUrl;
  } catch {
    return cleanUrl;
  }
}

function normalizeDaemonName(value: unknown): string | undefined {
  if (typeof value !== "string") {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed || undefined;
}

function normalizeDeviceName(value: unknown): string | undefined {
  if (typeof value !== "string") {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed || undefined;
}

function defaultDeviceName(): string {
  const platform =
    typeof navigator === "undefined"
      ? undefined
      : // userAgentData 在部分浏览器存在；没有时回退到 platform，避免引入额外依赖。
        ((navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform ||
          navigator.platform);
  const cleanPlatform = platform?.trim();
  const suffix = Math.floor(1000 + Math.random() * 9000);
  return cleanPlatform ? `Web client ${suffix} on ${cleanPlatform}` : `Web client ${suffix}`;
}

function openStateDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE_NAME)) {
        db.createObjectStore(STORE_NAME);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("indexeddb_open_failed"));
  });
}

function requestToPromise<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("indexeddb_request_failed"));
  });
}
