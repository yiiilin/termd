import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  pushServiceWorkerScope,
  removeBrowserPushForServer,
  subscribeBrowserPush,
  syncBrowserPushPreference,
  unsubscribeBrowserPush,
} from "../push-notifications";
import type { DeviceState, PairedServerState } from "../protocol/types";
import { V070Client } from "../protocol/v070-client";

const SERVER_ID = "92ae2d30-ea7f-4bb7-90a6-56d4a70a5890";
const SECOND_SERVER_ID = "f065aee3-5f1e-46a7-83cd-59bfb64b9989";
const SESSION_ID = "f7ad50c5-8b33-41ae-a95e-7aab7afb5e7f";
const APPLICATION_SERVER_KEY =
  "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";

describe("浏览器 Push 生命周期", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("按 server id 注册独立 scope 并序列化 subscription", async () => {
    const subscription = {
      endpoint: "https://push.example.test/subscription",
      options: { applicationServerKey: undefined },
      toJSON: () => ({
        endpoint: "https://push.example.test/subscription",
        keys: {
          p256dh: APPLICATION_SERVER_KEY,
          auth: "BTBZMqHH6r4Tts7J_aSIgg",
        },
      }),
    };
    const subscribe = vi.fn(() => Promise.resolve(subscription));
    const registration = {
      scope: pushServiceWorkerScope(SERVER_ID),
      active: { state: "activated" },
      pushManager: {
        getSubscription: vi.fn(() => Promise.resolve(null)),
        subscribe,
      },
    };
    const register = vi.fn(() => Promise.resolve(registration));
    vi.stubGlobal("navigator", { serviceWorker: { register } });
    vi.stubGlobal("PushManager", class {});
    vi.stubGlobal("Notification", class {});
    vi.stubGlobal("isSecureContext", true);

    const serialized = await subscribeBrowserPush(SERVER_ID, APPLICATION_SERVER_KEY);

    expect(register).toHaveBeenCalledWith(
      "http://localhost:3000/service-worker.js",
      { scope: `http://localhost:3000/.termd-push/${SERVER_ID}/` },
    );
    expect(subscribe).toHaveBeenCalledWith({
      userVisibleOnly: true,
      applicationServerKey: expect.any(Uint8Array),
    });
    expect(serialized).toEqual({
      endpoint: "https://push.example.test/subscription",
      keys: {
        p256dh: APPLICATION_SERVER_KEY,
        auth: "BTBZMqHH6r4Tts7J_aSIgg",
      },
    });
  });

  it("只退订目标 daemon 的 registration", async () => {
    const unsubscribe = vi.fn(() => Promise.resolve(true));
    const unregister = vi.fn(() => Promise.resolve(true));
    const registration = {
      pushManager: { getSubscription: vi.fn(() => Promise.resolve({ unsubscribe })) },
      unregister,
    };
    const getRegistration = vi.fn(() => Promise.resolve(registration));
    vi.stubGlobal("navigator", { serviceWorker: { getRegistration } });

    await unsubscribeBrowserPush(SERVER_ID);

    expect(getRegistration).toHaveBeenCalledWith(
      `http://localhost:3000/.termd-push/${SERVER_ID}/`,
    );
    expect(unsubscribe).toHaveBeenCalledTimes(1);
    expect(unregister).toHaveBeenCalledTimes(1);
  });

  it("worker 在监听注册期间激活时不会错误等待超时", async () => {
    const worker: {
      state: ServiceWorkerState;
      addEventListener: ReturnType<typeof vi.fn>;
      removeEventListener: ReturnType<typeof vi.fn>;
    } = {
      state: "installing" as ServiceWorkerState,
      addEventListener: vi.fn(() => {
        worker.state = "activated";
      }),
      removeEventListener: vi.fn(),
    };
    const subscription = {
      endpoint: "https://push.example.test/subscription",
      options: { applicationServerKey: undefined },
      toJSON: () => ({
        endpoint: "https://push.example.test/subscription",
        keys: { p256dh: APPLICATION_SERVER_KEY, auth: "BTBZMqHH6r4Tts7J_aSIgg" },
      }),
    };
    const registration = {
      active: null,
      installing: worker,
      waiting: null,
      pushManager: {
        getSubscription: vi.fn(async () => subscription),
      },
    };
    vi.stubGlobal("navigator", {
      serviceWorker: { register: vi.fn(async () => registration) },
    });
    vi.stubGlobal("PushManager", class {});
    vi.stubGlobal("Notification", class {});
    vi.stubGlobal("isSecureContext", true);

    await expect(subscribeBrowserPush(SERVER_ID, APPLICATION_SERVER_KEY)).resolves.toMatchObject({
      endpoint: "https://push.example.test/subscription",
    });
    expect(worker.addEventListener).toHaveBeenCalledTimes(1);
    expect(worker.removeEventListener).toHaveBeenCalledTimes(1);
  });

  it("把全局 mentions 偏好同步到所有带设备证书的 daemon", async () => {
    const subscription = {
      endpoint: "https://push.example.test/subscription",
      options: { applicationServerKey: undefined },
      toJSON: () => ({
        endpoint: "https://push.example.test/subscription",
        keys: { p256dh: APPLICATION_SERVER_KEY, auth: "BTBZMqHH6r4Tts7J_aSIgg" },
      }),
    };
    const register = vi.fn((_: string, options: RegistrationOptions) => Promise.resolve({
      scope: options.scope,
      active: { state: "activated" },
      pushManager: {
        getSubscription: vi.fn(() => Promise.resolve(null)),
        subscribe: vi.fn(() => Promise.resolve(subscription)),
      },
    }));
    vi.stubGlobal("navigator", { serviceWorker: { register } });
    vi.stubGlobal("PushManager", class {});
    vi.stubGlobal("Notification", class { static permission = "granted"; });
    vi.stubGlobal("isSecureContext", true);

    const requests = new Map<string, Array<{ path: string; init?: RequestInit }>>();
    const connect = vi.spyOn(V070Client, "connect").mockImplementation(async (server) => ({
      serverId: server.server_id,
      requestPush: vi.fn(async (path: string, init?: RequestInit) => {
        const calls = requests.get(server.server_id) ?? [];
        calls.push({ path, init });
        requests.set(server.server_id, calls);
        return path === "/api/push/config"
          ? Response.json({ server_id: server.server_id, application_server_key: APPLICATION_SERVER_KEY, subscribed: false })
          : new Response(null, { status: 204 });
      }),
      close: vi.fn(),
    } as unknown as V070Client));

    await syncBrowserPushPreference({
      device: testDevice(),
      servers: [testServer(SERVER_ID), testServer(SECOND_SERVER_ID), testServer(SESSION_ID, false)],
      preference: "mentions",
      locale: "zh-CN",
    });

    expect(connect).toHaveBeenCalledTimes(2);
    for (const serverId of [SERVER_ID, SECOND_SERVER_ID]) {
      const calls = requests.get(serverId);
      expect(calls?.map((call) => [call.path, call.init?.method])).toEqual([
        ["/api/push/config", "GET"],
        ["/api/push/subscription", "PUT"],
      ]);
      expect(JSON.parse(String(calls?.[1]?.init?.body))).toMatchObject({
        endpoint: "https://push.example.test/subscription",
        mode: "attention",
        locale: "zh-CN",
      });
    }
    expect(register).toHaveBeenCalledTimes(2);
  });

  it("关闭偏好时先删除 daemon 订阅，再清理本地 registration", async () => {
    const operations: string[] = [];
    const unregister = vi.fn(async () => {
      operations.push("unregister");
      return true;
    });
    vi.stubGlobal("navigator", {
      serviceWorker: {
        getRegistration: vi.fn(async () => ({
          pushManager: {
            getSubscription: vi.fn(async () => ({
              endpoint: "https://push.example.test/subscription",
              unsubscribe: vi.fn(async () => {
                operations.push("unsubscribe");
                return true;
              }),
            })),
          },
          unregister,
        })),
      },
    });
    vi.spyOn(V070Client, "connect").mockResolvedValue({
      serverId: SERVER_ID,
      requestPush: vi.fn(async (_path: string, init?: RequestInit) => {
        operations.push("delete");
        expect(JSON.parse(String(init?.body))).toEqual({
          endpoint: "https://push.example.test/subscription",
        });
        return new Response(null, { status: 204 });
      }),
      close: vi.fn(),
    } as unknown as V070Client);

    await syncBrowserPushPreference({
      device: testDevice(),
      servers: [testServer(SERVER_ID)],
      preference: "off",
      locale: "en-US",
    });

    expect(operations).toEqual(["delete", "unsubscribe", "unregister"]);
  });

  it("忘记离线 daemon 时仍通过本地 unsubscribe 使旧 endpoint 失效", async () => {
    const operations: string[] = [];
    vi.stubGlobal("navigator", {
      serviceWorker: {
        getRegistration: vi.fn(async () => ({
          pushManager: {
            getSubscription: vi.fn(async () => ({
              endpoint: "https://push.example.test/subscription",
              unsubscribe: vi.fn(async () => {
                operations.push("unsubscribe");
                return true;
              }),
            })),
          },
          unregister: vi.fn(async () => {
            operations.push("unregister");
            return true;
          }),
        })),
      },
    });
    vi.spyOn(V070Client, "connect").mockResolvedValue({
      requestPush: vi.fn(async () => {
        operations.push("delete");
        throw new Error("offline");
      }),
      close: vi.fn(),
    } as unknown as V070Client);

    await removeBrowserPushForServer(testServer(SERVER_ID), testDevice());

    expect(operations).toEqual(["delete", "unsubscribe", "unregister"]);
  });

  it("忘记 daemon 超时会取消旧 DELETE，避免越过重新配对边界", async () => {
    vi.useFakeTimers();
    const operations: string[] = [];
    let deleteSignal: AbortSignal | undefined;
    vi.stubGlobal("navigator", {
      serviceWorker: {
        getRegistration: vi.fn(async () => ({
          pushManager: {
            getSubscription: vi.fn(async () => ({
              endpoint: "https://push.example.test/subscription",
              unsubscribe: vi.fn(async () => {
                operations.push("unsubscribe");
                return true;
              }),
            })),
          },
          unregister: vi.fn(async () => {
            operations.push("unregister");
            return true;
          }),
        })),
      },
    });
    vi.spyOn(V070Client, "connect").mockResolvedValue({
      requestPush: vi.fn(async (_path: string, init?: RequestInit) => {
        deleteSignal = init?.signal ?? undefined;
        expect(JSON.parse(String(init?.body))).toEqual({
          endpoint: "https://push.example.test/subscription",
        });
        await new Promise<void>((resolve, reject) => {
          const lateDelete = globalThis.setTimeout(() => {
            operations.push("late-delete");
            resolve();
          }, 6_000);
          const abort = () => {
            globalThis.clearTimeout(lateDelete);
            operations.push("delete-aborted");
            reject(new Error("aborted"));
          };
          if (deleteSignal?.aborted) {
            abort();
          } else {
            deleteSignal?.addEventListener("abort", abort, { once: true });
          }
        });
        return new Response(null, { status: 204 });
      }),
      close: vi.fn(),
    } as unknown as V070Client);

    const cleanup = removeBrowserPushForServer(testServer(SERVER_ID), testDevice());
    await vi.advanceTimersByTimeAsync(5_000);
    await cleanup;
    operations.push("new-put");
    await vi.advanceTimersByTimeAsync(1_000);

    expect(deleteSignal?.aborted).toBe(true);
    expect(operations).toEqual([
      "delete-aborted",
      "unsubscribe",
      "unregister",
      "new-put",
    ]);
  });
});

describe("实际 service worker 事件", () => {
  it("页面可见时抑制 Push，隐藏时显示通知", async () => {
    const worker = await loadWorker(
      `https://termd.test/termd/.termd-push/${SERVER_ID}/`,
    );
    worker.self.clients.matchAll.mockResolvedValueOnce([
      { url: "https://termd.test/termd/", visibilityState: "visible" },
    ]);
    await dispatchWaitUntil(worker.listeners.push, pushEventPayload());
    expect(worker.self.registration.showNotification).not.toHaveBeenCalled();

    worker.self.clients.matchAll.mockResolvedValueOnce([
      { url: "https://termd.test/termd/", visibilityState: "hidden" },
    ]);
    await dispatchWaitUntil(worker.listeners.push, pushEventPayload());
    expect(worker.self.registration.showNotification).toHaveBeenCalledWith(
      "Termd",
      expect.objectContaining({
        body: "Release shell：Codex 已完成",
        tag: `termd-session-activity-${SERVER_ID}-${SESSION_ID}`,
        data: { server_id: SERVER_ID, session_id: SESSION_ID },
      }),
    );
  });

  it("点击通知复用应用窗口并导航到可信 daemon/session query", async () => {
    const worker = await loadWorker(
      `https://termd.test/termd/.termd-push/${SERVER_ID}/`,
    );
    const navigate = vi.fn(() => Promise.resolve());
    const focus = vi.fn(() => Promise.resolve());
    worker.self.clients.matchAll.mockResolvedValue([
      {
        url: "https://termd.test/termd/?old=1",
        visibilityState: "hidden",
        navigate,
        focus,
      },
    ]);
    const close = vi.fn();

    await dispatchWaitUntil(worker.listeners.notificationclick, {
      notification: {
        data: { server_id: SERVER_ID, session_id: SESSION_ID },
        close,
      },
    });

    expect(close).toHaveBeenCalledTimes(1);
    expect(navigate).toHaveBeenCalledWith(
      `https://termd.test/termd/?termd_server_id=${SERVER_ID}&termd_session_id=${SESSION_ID}`,
    );
    expect(focus).toHaveBeenCalledTimes(1);
    expect(worker.self.clients.openWindow).not.toHaveBeenCalled();
  });

  it("拒绝非法 payload，并在没有应用窗口时打开可信目标", async () => {
    const worker = await loadWorker(
      `https://termd.test/termd/.termd-push/${SERVER_ID}/`,
    );
    await dispatchWaitUntil(worker.listeners.push, {
      data: {
        json: () => ({
          version: 1,
          server_id: SECOND_SERVER_ID,
          session_id: SESSION_ID,
          body: "wrong daemon",
        }),
      },
    });
    expect(worker.self.clients.matchAll).not.toHaveBeenCalled();
    expect(worker.self.registration.showNotification).not.toHaveBeenCalled();

    worker.self.clients.matchAll.mockResolvedValue([]);
    await dispatchWaitUntil(worker.listeners.notificationclick, {
      notification: {
        data: { server_id: SERVER_ID, session_id: SESSION_ID },
        close: vi.fn(),
      },
    });

    expect(worker.self.clients.openWindow).toHaveBeenCalledWith(
      `https://termd.test/termd/?termd_server_id=${SERVER_ID}&termd_session_id=${SESSION_ID}`,
    );
  });
});

interface WorkerHarness {
  listeners: Record<string, (event: any) => void>;
  self: {
    registration: { scope: string; showNotification: ReturnType<typeof vi.fn> };
    clients: { matchAll: ReturnType<typeof vi.fn>; openWindow: ReturnType<typeof vi.fn> };
  };
}

async function loadWorker(scope: string): Promise<WorkerHarness> {
  const source = await readFile(resolve(process.cwd(), "public/service-worker.js"), "utf8");
  const listeners: Record<string, (event: any) => void> = {};
  const workerSelf = {
    registration: { scope, showNotification: vi.fn(() => Promise.resolve()) },
    clients: { matchAll: vi.fn(), openWindow: vi.fn(() => Promise.resolve()) },
    skipWaiting: vi.fn(() => Promise.resolve()),
    addEventListener: (type: string, handler: (event: any) => void) => {
      listeners[type] = handler;
    },
  };
  new Function("self", source)(workerSelf);
  return { listeners, self: workerSelf };
}

function pushEventPayload(): Record<string, unknown> {
  return {
    data: {
      json: () => ({
        version: 1,
        server_id: SERVER_ID,
        session_id: SESSION_ID,
        title: "Termd",
        body: "Release shell：Codex 已完成",
      }),
    },
  };
}

async function dispatchWaitUntil(
  handler: ((event: any) => void) | undefined,
  event: Record<string, unknown>,
): Promise<void> {
  let pending = Promise.resolve();
  handler?.({
    ...event,
    waitUntil: (promise: Promise<unknown>) => {
      pending = promise.then(() => undefined);
    },
  });
  await pending;
}

function testDevice(): DeviceState {
  return {
    device_id: "14af51fb-9e68-4dbc-b589-bb4f8382ce38",
    device_public_key: "ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    device_signing_key_secret: "test-secret",
  };
}

function testServer(serverId: string, withCertificate = true): PairedServerState {
  return {
    server_id: serverId,
    daemon_public_key: "ed25519-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    url: "wss://relay.example/ws",
    paired_at_ms: 1,
    ...(withCertificate ? { device_certificate: `certificate.${serverId}` } : {}),
  };
}
