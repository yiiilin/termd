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
  return state ?? { pairedServers: [] };
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
  const server: PairedServerState = {
    server_id: accepted.server_id,
    daemon_public_key: accepted.daemon_public_key,
    url,
    paired_at_ms: Date.now(),
  };
  const pairedServers = [
    ...state.pairedServers.filter((existing) => existing.server_id !== server.server_id),
    server,
  ];
  const next = normalizeState({
    ...state,
    pairedServers,
    defaultServerId: accepted.server_id,
    defaultUrl: url,
  });
  await saveBrowserState(next);
  return next;
}

export async function recordServerUrl(serverId: string, url: string): Promise<BrowserState> {
  const cleanUrl = url.trim();
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
  return {
    device: state.device
      ? {
          device_id: state.device.device_id,
          device_public_key: state.device.device_public_key,
          device_signing_key_secret: state.device.device_signing_key_secret,
        }
      : undefined,
    pairedServers: (state.pairedServers ?? []).map((server) => ({
      server_id: server.server_id,
      daemon_public_key: server.daemon_public_key,
      url: server.url,
      paired_at_ms: server.paired_at_ms,
    })),
    defaultServerId: state.defaultServerId,
    defaultUrl: state.defaultUrl,
  };
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
