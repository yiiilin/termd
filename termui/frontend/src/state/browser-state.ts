import { generateDeviceIdentity } from "../protocol/auth";
import type { BrowserState, DeviceState, PairAcceptPayload, PairedServerState } from "../protocol/types";

const DB_NAME = "termd-termui-web";
const DB_VERSION = 1;
const STORE_NAME = "state";
const STATE_KEY = "current";

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
    return state.device;
  }

  const device = await generateDeviceIdentity();
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
        }
      : undefined,
    pairedServers,
    defaultServerId: state.defaultServerId,
    defaultUrl,
  };
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
