import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, {
  APP_VERSION,
  APP_CONNECTION_TIMEOUT_MS,
  aiActivityTransitions,
  browserReachableWsUrl,
  isExclusiveMetadataClient,
  DAEMON_LATENCY_POLL_INTERVAL_MS,
  defaultWsUrlFromPage,
  knownServerWsUrlCandidates,
  latencyLevelClass,
  metadataAiActivityTransitions,
  networkRateFromSamples,
  notifyAiActivityTransitions,
  pairingWsUrlCandidates,
} from "../App";
import { decodeSupervisorTerminalClientFrame } from "../protocol/supervisor-terminal";
import type {
  AttachFramePayload,
  SessionFileDownloadStreamReadyPayload,
  SessionFileHttpUploadStreamPayload,
  SessionFileHttpUploadReadyPayload,
  SessionFileUploadProgressPayload,
  SessionFilesResultPayload,
  SessionGitResultPayload,
  UUID,
} from "../protocol/types";
import type { ProtocolPacket } from "../test/legacy-protocol-stubs";
import { concatBytes, encodeUtf8, sessionDataFromBase64 } from "../protocol/wire";
import { ProtocolClientError } from "../protocol/errors";
import { V070Client } from "../protocol/v070-client";
import { displayUrlWithoutQueryOrFragment } from "../protocol/url";
import { clearBrowserState, loadBrowserState } from "../state/browser-state";
import { MockDaemon } from "../test/mock-daemon";
import { fallbackSessionDisplayName } from "../session-names";
import { resetFileEditorDialogMonacoCacheForTests } from "../components/FileEditorDialog";
import { SessionFilesPanel } from "../components/SessionFilesPanel";
import { createTranslator } from "../i18n";

const DEFAULT_SESSION_ID = "00000000-0000-0000-0000-000000000401";
const DEFAULT_SESSION_NAME = fallbackSessionDisplayName(DEFAULT_SESSION_ID);

describe("metadata effect client ownership", () => {
  it("only treats the current unshared metadata client as effect-owned", () => {
    const client = { close: vi.fn() } as unknown as V070Client;
    const replacement = { close: vi.fn() } as unknown as V070Client;

    expect(isExclusiveMetadataClient(client, client, undefined, undefined)).toBe(true);
    expect(isExclusiveMetadataClient(client, replacement, undefined, undefined)).toBe(false);
    expect(isExclusiveMetadataClient(client, client, client, undefined)).toBe(false);
    expect(isExclusiveMetadataClient(client, client, undefined, client)).toBe(false);
  });
});

describe("AI activity notifications", () => {
  const size = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
  const activity = (state: "idle" | "running" | "attention" | "completed", changedAt: number) => ({
    kind: "ai" as const,
    agent: "codex" as const,
    state,
    changed_at_ms: changedAt,
  });

  it("hydrates the first metadata frame and only emits running to non-running transitions", () => {
    const runningSessions = [
      { session_id: "one", name: "One", state: "running" as const, size, activity: activity("running", 1) },
      { session_id: "two", name: "Two", state: "running" as const, size, activity: activity("running", 1) },
      { session_id: "three", name: "Three", state: "running" as const, size, activity: activity("running", 1) },
      { session_id: "removed", state: "running" as const, size, activity: activity("running", 1) },
    ];
    const settledSessions = [
      { ...runningSessions[0], activity: activity("completed", 2) },
      { ...runningSessions[1], activity: activity("attention", 2) },
      { ...runningSessions[2], activity: activity("idle", 2) },
      { session_id: "new", state: "running" as const, size, activity: activity("completed", 2) },
    ];

    expect(metadataAiActivityTransitions("snapshot", undefined, settledSessions)).toEqual([]);
    expect(metadataAiActivityTransitions("snapshot", runningSessions, settledSessions)).toEqual([]);
    expect(metadataAiActivityTransitions("update", runningSessions, settledSessions).map(({ session, activity }) => [
      session.session_id,
      activity.state,
    ])).toEqual([
      ["one", "completed"],
      ["two", "attention"],
      ["three", "idle"],
    ]);
    expect(metadataAiActivityTransitions("update", settledSessions, settledSessions)).toEqual([]);
  });

  it("uses independent per-session tags without the background-output throttle", () => {
    const originalNotification = Object.getOwnPropertyDescriptor(globalThis, "Notification");
    const calls: Array<{ title: string; options?: NotificationOptions }> = [];
    class TestNotification {
      static permission = "granted";
      constructor(title: string, options?: NotificationOptions) {
        calls.push({ title, options });
      }
    }
    Object.defineProperty(globalThis, "Notification", {
      configurable: true,
      value: TestNotification,
    });
    try {
      const previous = [
        { session_id: "one", name: "Build", state: "running" as const, size, activity: activity("running", 1) },
        { session_id: "two", name: "Review", state: "running" as const, size, activity: activity("running", 1) },
      ];
      const next = [
        { ...previous[0], activity: activity("completed", 2) },
        { ...previous[1], activity: activity("attention", 2) },
      ];

      notifyAiActivityTransitions(
        aiActivityTransitions(previous, next),
        { language: "en-US", theme: "light", notifications: "mentions", mobileShortcuts: [] },
        createTranslator("en-US"),
      );

      expect(calls).toEqual([
        {
          title: "Termd",
          options: {
            body: "Build: Codex finished",
            tag: "termd-session-activity-one",
            silent: true,
          },
        },
        {
          title: "Termd",
          options: {
            body: "Review: Codex needs attention",
            tag: "termd-session-activity-two",
            silent: true,
          },
        },
      ]);

      notifyAiActivityTransitions(
        aiActivityTransitions(previous, next),
        { language: "en-US", theme: "light", notifications: "off", mobileShortcuts: [] },
        createTranslator("en-US"),
      );
      TestNotification.permission = "denied";
      notifyAiActivityTransitions(
        aiActivityTransitions(previous, next),
        { language: "en-US", theme: "light", notifications: "all", mobileShortcuts: [] },
        createTranslator("en-US"),
      );
      expect(calls).toHaveLength(2);
    } finally {
      if (originalNotification) {
        Object.defineProperty(globalThis, "Notification", originalNotification);
      } else {
        Reflect.deleteProperty(globalThis, "Notification");
      }
    }
  });
});

const qrScannerMock = vi.hoisted(() => ({
  destroy: vi.fn(),
  hasCamera: vi.fn<() => Promise<boolean>>(() => Promise.resolve(true)),
  onDecode: undefined as ((result: { data: string }) => void) | undefined,
  options: undefined as
    | {
        calculateScanRegion?: (video: HTMLVideoElement) => {
          x: number;
          y: number;
          width: number;
          height: number;
          downScaledWidth: number;
          downScaledHeight: number;
        };
      }
    | undefined,
  scanImage: vi.fn<() => Promise<{ data: string; cornerPoints: [] }>>(),
  start: vi.fn<() => Promise<void>>(() => Promise.resolve()),
  stop: vi.fn(),
}));

vi.mock("qr-scanner", () => {
  class MockQrScanner {
    static NO_QR_CODE_FOUND = "No QR code found";
    static hasCamera = qrScannerMock.hasCamera;
    static scanImage = qrScannerMock.scanImage;

    constructor(_video: HTMLVideoElement, onDecode: (result: { data: string }) => void, options?: typeof qrScannerMock.options) {
      qrScannerMock.onDecode = onDecode;
      qrScannerMock.options = options;
    }

    start = qrScannerMock.start;
    stop = qrScannerMock.stop;
    destroy = qrScannerMock.destroy;
  }

  return { default: MockQrScanner };
});

async function setConnectionUrl(user: ReturnType<typeof userEvent.setup>, url: string): Promise<void> {
  if (!screen.queryByLabelText("WS URL")) {
    await user.click(await screen.findByRole("button", { name: "Edit address" }));
  }
  const input = await screen.findByLabelText("WS URL");
  await user.clear(input);
  await user.type(input, url);
}

function setViewportWidth(width: number): void {
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    value: width,
    writable: true,
  });
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: undefined,
    writable: true,
  });
  window.dispatchEvent(new Event("resize"));
}

function setTouchCapability(enabled: boolean): void {
  Object.defineProperty(navigator, "maxTouchPoints", {
    configurable: true,
    value: enabled ? 5 : 0,
    writable: true,
  });
  Object.defineProperty(window, "ontouchstart", {
    configurable: true,
    value: enabled ? () => undefined : undefined,
    writable: true,
  });
}

function setMobileVisualViewport(layoutHeight: number, visualHeight: number, offsetTop = 0): void {
  Object.defineProperty(window, "innerHeight", {
    configurable: true,
    value: layoutHeight,
    writable: true,
  });
  Object.defineProperty(window, "visualViewport", {
    configurable: true,
    value: {
      height: visualHeight,
      offsetTop,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    },
    writable: true,
  });
  window.dispatchEvent(new Event("resize"));
}

function encodeHttpE2eeTestFrames(frames: Uint8Array[]): Uint8Array {
  return concatBytes(
    ...frames.map((frame) => {
      const wire = new Uint8Array(4 + frame.byteLength);
      new DataView(wire.buffer, wire.byteOffset, 4).setUint32(0, frame.byteLength, false);
      wire.set(frame, 4);
      return wire;
    }),
  );
}

function decodeHttpE2eeTestFrames(wire: Uint8Array): Uint8Array[] {
  const frames: Uint8Array[] = [];
  let offset = 0;
  while (offset < wire.byteLength) {
    if (wire.byteLength - offset < 4) {
      throw new Error("truncated HTTP test frame length");
    }
    const len = new DataView(wire.buffer, wire.byteOffset + offset, 4).getUint32(0, false);
    offset += 4;
    if (len === 0 || wire.byteLength - offset < len) {
      throw new Error("invalid HTTP test frame body");
    }
    frames.push(wire.slice(offset, offset + len));
    offset += len;
  }
  return frames;
}

function responseBodyBytes(bytes: Uint8Array): ArrayBuffer {
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
}

async function requestBodyBytes(body: BodyInit | null | undefined): Promise<Uint8Array> {
  if (!body) {
    throw new Error("missing request body");
  }
  if (body instanceof ReadableStream) {
    throw new Error("upload body must not be a ReadableStream");
  }
  if (body instanceof Blob || Object.prototype.toString.call(body) === "[object Blob]") {
    const blob = body as Blob;
    if (typeof blob.arrayBuffer === "function") {
      return new Uint8Array(await blob.arrayBuffer());
    }
    return await new Promise<Uint8Array>((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error ?? new Error("failed to read blob"));
      reader.onload = () => resolve(new Uint8Array(reader.result as ArrayBuffer));
      reader.readAsArrayBuffer(blob);
    });
  }
  if (body instanceof ArrayBuffer || Object.prototype.toString.call(body) === "[object ArrayBuffer]") {
    return new Uint8Array(body as ArrayBuffer);
  }
  if (ArrayBuffer.isView(body)) {
    const view = body as ArrayBufferView;
    return new Uint8Array(view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength));
  }
  return encodeUtf8(String(body));
}

function installHttpControlFailureOnceMock(
  pathSuffix: string,
  responseInit: { status: number; body?: string } = { status: 502, body: "bad gateway" },
): () => void {
  const originalFetch = globalThis.fetch;
  let pending = true;
  (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
    const requestUrl = new URL(input instanceof Request ? input.url : String(input));
    if (pending && requestUrl.pathname.endsWith(pathSuffix)) {
      pending = false;
      return new Response(responseInit.body ?? "bad gateway", {
        status: responseInit.status,
        headers: { "content-type": "text/plain" },
      });
    }
    return originalFetch(input, init);
  }) as typeof fetch;
  return () => {
    (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
  };
}

interface HttpUploadMockRecord {
  session_id: UUID;
  path: string;
  bytes: Uint8Array;
}

interface TestDiagnosticEvent {
  name: string;
  fields?: Record<string, unknown>;
}

function testDiagnostics(): { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TestDiagnosticEvent[] } {
  return globalThis as { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TestDiagnosticEvent[] };
}

function clearTermdDiagnosticsForTest(): void {
  const scope = testDiagnostics();
  delete scope.__TERMD_TRACE__;
  delete scope.__TERMD_DIAG_EVENTS__;
}

function installHttpUploadOnceMock(
  daemon: MockDaemon,
  sessionId: UUID,
  uploadPath: string,
  file: File,
): { restore: () => void; uploads: HttpUploadMockRecord[] } {
  const originalFetch = globalThis.fetch;
  const uploads: HttpUploadMockRecord[] = [];
  const uploadId = "mock-app-upload";
  (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname.endsWith("/api/files/uploads") && init?.method === "POST") {
      return Response.json({ upload_id: uploadId, session_id: sessionId, path: uploadPath, size_bytes: file.size });
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/chunks`) && init?.method === "PUT") {
      const bytes = await requestBodyBytes(init.body);
      uploads.push({ session_id: sessionId, path: uploadPath, bytes });
      const progress = {
        session_id: sessionId,
        path: uploadPath,
        offset_bytes: bytes.byteLength,
        size_bytes: file.size,
        eof: bytes.byteLength === file.size,
        modified_at_ms: null,
      } satisfies SessionFileUploadProgressPayload;
      return Response.json(progress);
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/commit`)) {
      return Response.json({ session_id: sessionId, path: uploadPath, size_bytes: file.size, modified_at_ms: null });
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/abort`)) {
      return Response.json({ upload_id: uploadId, aborted: true });
    }
    return originalFetch(input, init);
  }) as typeof fetch;
  return {
    restore: () => {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
    },
    uploads,
  };
}

function installDelayedHttpUploadInitMock(
  daemon: MockDaemon,
  sessionId: UUID,
  uploadPath: string,
  file: File,
): { restore: () => void; releaseInit: () => void; uploads: HttpUploadMockRecord[] } {
  const originalFetch = globalThis.fetch;
  const uploads: HttpUploadMockRecord[] = [];
  const uploadId = "mock-app-delayed-upload";
  let releaseInit: (() => void) | undefined;
  const initReleased = new Promise<void>((resolve) => {
    releaseInit = resolve;
  });
  (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname.endsWith("/api/files/uploads") && init?.method === "POST") {
      await initReleased;
      return Response.json({ upload_id: uploadId, session_id: sessionId, path: uploadPath, size_bytes: file.size });
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/chunks`) && init?.method === "PUT") {
      const bytes = await requestBodyBytes(init.body);
      uploads.push({ session_id: sessionId, path: uploadPath, bytes });
      const progress = {
        session_id: sessionId,
        path: uploadPath,
        offset_bytes: bytes.byteLength,
        size_bytes: file.size,
        eof: bytes.byteLength === file.size,
        modified_at_ms: null,
      } satisfies SessionFileUploadProgressPayload;
      return Response.json(progress);
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/commit`)) {
      return Response.json({ session_id: sessionId, path: uploadPath, size_bytes: file.size, modified_at_ms: null });
    }
    if (url.pathname.endsWith(`/api/files/uploads/${uploadId}/abort`)) {
      return Response.json({ upload_id: uploadId, aborted: true });
    }
    return originalFetch(input, init);
  }) as typeof fetch;
  return {
    restore: () => {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
    },
    releaseInit: () => releaseInit?.(),
    uploads,
  };
}

function installDelayedHttpUploadInitFailure(): { restore: () => void; failInit: () => void } {
  const originalFetch = globalThis.fetch;
  let failInit: (() => void) | undefined;
  const initFailed = new Promise<void>((resolve) => {
    failInit = resolve;
  });
  (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname.endsWith("/api/files/uploads") && init?.method === "POST") {
      await initFailed;
      throw new TypeError("upload init failed");
    }
    return originalFetch(input, init);
  }) as typeof fetch;
  return {
    restore: () => {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
    },
    failInit: () => failInit?.(),
  };
}

function installHttpDownloadMock(
  daemon: MockDaemon,
  sessionId: UUID,
  filePath: string,
  name: string,
  bytes: Uint8Array,
): { restore: () => void; calls: () => number } {
  const originalFetch = globalThis.fetch;
  let calls = 0;
  const downloadId = "mock-app-download";
  (globalThis as unknown as { fetch: typeof fetch }).fetch = (async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname.endsWith("/api/files/downloads") && init?.method === "POST") {
      calls += 1;
      return Response.json({
        download_id: downloadId,
        session_id: sessionId,
        path: filePath,
        name,
        size_bytes: bytes.byteLength,
        modified_at_ms: null,
      });
    }
    if (url.pathname.endsWith(`/api/files/downloads/${downloadId}`) && (!init?.method || init.method === "GET")) {
      return new Response(bytes.slice());
    }
    return originalFetch(input, init);
  }) as typeof fetch;
  return {
    restore: () => {
      (globalThis as unknown as { fetch: typeof fetch }).fetch = originalFetch;
    },
    calls: () => calls,
  };
}

function installMutableMobileVisualViewport(layoutHeight: number, visualHeight: number, offsetTop = 0) {
  let metrics = { layoutHeight, visualHeight, offsetTop };
  const target = new EventTarget();
  const viewport = {
    get height() {
      return metrics.visualHeight;
    },
    get offsetTop() {
      return metrics.offsetTop;
    },
    get width() {
      return window.innerWidth;
    },
    get offsetLeft() {
      return 0;
    },
    get pageLeft() {
      return 0;
    },
    get pageTop() {
      return metrics.offsetTop;
    },
    get scale() {
      return 1;
    },
    addEventListener: target.addEventListener.bind(target),
    removeEventListener: target.removeEventListener.bind(target),
    dispatchEvent: target.dispatchEvent.bind(target),
  } as unknown as VisualViewport;
  Object.defineProperty(window, "innerHeight", {
    configurable: true,
    get: () => metrics.layoutHeight,
  });
  Object.defineProperty(window, "visualViewport", {
    configurable: true,
    value: viewport,
    writable: true,
  });

  return {
    setMetrics(nextLayoutHeight: number, nextVisualHeight: number, nextOffsetTop = 0) {
      metrics = {
        layoutHeight: nextLayoutHeight,
        visualHeight: nextVisualHeight,
        offsetTop: nextOffsetTop,
      };
      // 部分移动浏览器只派发 visualViewport 事件，不会同步派发 window.resize。
      target.dispatchEvent(new Event("resize"));
    },
  };
}

let mockedDocumentVisibilityState: DocumentVisibilityState = "visible";
let mockedDocumentHasFocus = true;

function setDocumentVisibility(state: DocumentVisibilityState): void {
  mockedDocumentVisibilityState = state;
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => mockedDocumentVisibilityState,
  });
  Object.defineProperty(document, "hidden", {
    configurable: true,
    get: () => mockedDocumentVisibilityState === "hidden",
  });
  document.dispatchEvent(new Event("visibilitychange"));
}

function setDocumentHasFocus(focused: boolean): void {
  mockedDocumentHasFocus = focused;
  Object.defineProperty(document, "hasFocus", {
    configurable: true,
    value: () => mockedDocumentHasFocus,
  });
}

function restoreDocumentVisibility(): void {
  mockedDocumentVisibilityState = "visible";
  mockedDocumentHasFocus = true;
  Reflect.deleteProperty(document, "visibilityState");
  Reflect.deleteProperty(document, "hidden");
  Reflect.deleteProperty(document, "hasFocus");
}

function dispatchMobileTextInput(
  target: HTMLTextAreaElement,
  data: string,
  options: { isComposing?: boolean } = {},
): InputEvent {
  const event = new InputEvent("beforeinput", {
    bubbles: true,
    cancelable: true,
    data,
    inputType: "insertText",
  });
  if (options.isComposing !== undefined) {
    Object.defineProperty(event, "isComposing", {
      configurable: true,
      value: options.isComposing,
    });
  }
  target.dispatchEvent(event);
  return event;
}

function dispatchMobilePasteInput(target: HTMLTextAreaElement, data: string): InputEvent {
  const event = new InputEvent("beforeinput", {
    bubbles: true,
    cancelable: true,
    data,
    inputType: "insertFromPaste",
  });
  target.dispatchEvent(event);
  return event;
}

function dispatchMobileClipboardPaste(target: HTMLTextAreaElement, data: string): Event {
  const event = new Event("paste", {
    bubbles: true,
    cancelable: true,
  });
  Object.defineProperty(event, "clipboardData", {
    configurable: true,
    value: {
      getData: (format: string) => (format === "text" || format === "text/plain" ? data : ""),
    },
  });
  target.dispatchEvent(event);
  return event;
}

function fireTouchPointer(
  target: HTMLElement,
  type: "pointerdown" | "pointermove" | "pointerup" | "pointercancel",
  options: { pointerId: number; clientX: number; clientY: number },
): void {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: options.pointerId },
    pointerType: { value: "touch" },
    button: { value: 0 },
    clientX: { value: options.clientX },
    clientY: { value: options.clientY },
  });
  fireEvent(target, event);
}

function pairingInviteCode(
  daemon: MockDaemon,
  token = "secret-token",
  options: { serverId?: string; wsUrl?: string } = {},
): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 1,
    ...(options.wsUrl === undefined ? {} : { ws_url: options.wsUrl }),
    token,
    server_id: options.serverId ?? daemon.serverId,
    daemon_public_key: daemon.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}

function expectPairingTokenOnlyInRelayAdmission(daemon: MockDaemon, token = "secret-token"): void {
  expect(daemon.pairingTokens).toEqual([token]);
  expect(daemon.outerWireText()).not.toContain(token);
}

async function pairWithInvite(
  user: ReturnType<typeof userEvent.setup>,
  daemon: MockDaemon,
  token = "secret-token",
): Promise<void> {
  await setConnectionUrl(user, daemon.url);
  fireEvent.change(screen.getByLabelText("Pairing token"), {
    target: { value: pairingInviteCode(daemon, token) },
  });
  await user.click(screen.getByRole("button", { name: "Pair" }));
}

async function expectDaemonUrlInAdmin(user: ReturnType<typeof userEvent.setup>, url: string): Promise<void> {
  if (!screen.queryByLabelText("daemon admin")) {
    await user.click(screen.getByRole("button", { name: "Daemons" }));
  }
  const admin = await screen.findByLabelText("daemon admin");
  const displayUrl = displayUrlWithoutQueryOrFragment(url);
  await waitFor(() => expect(within(admin).getAllByText(displayUrl).length).toBeGreaterThan(0));
}

async function waitForWorkspaceSession(name?: string, options: { timeout?: number } = {}): Promise<void> {
  await waitForWorkspaceReady();
  const mobileTitle = document.querySelector<HTMLButtonElement>("button.toolbar-title-button");
  if (mobileTitle) {
    if (name === "No session") {
      await waitFor(() => expect(mobileTitle).toHaveTextContent("No session"), options);
      return;
    }
    if (name) {
      await waitFor(() => expect(mobileTitle).toHaveTextContent(name), options);
      return;
    }
    await waitFor(() => expect(mobileTitle).not.toHaveTextContent("No session"), options);
    return;
  }

  const sessionList = screen.getByRole("region", { name: "sessions" });
  if (name) {
    if (name === "No session") {
      await waitFor(() => expect(within(sessionList).getByText("No sessions")).toBeVisible(), options);
      return;
    }
    await waitFor(() => expect(within(sessionList).queryAllByText(name).length).toBeGreaterThan(0), options);
    return;
  }
  await waitFor(() => {
    expect(within(sessionList).queryAllByRole("button", { name: /^Open / }).length).toBeGreaterThan(0);
  }, options);
}

async function waitForWorkspaceReady(): Promise<void> {
  await screen.findByTestId("terminal-pane");
}

function expectTerminalAndMetadataConnectionBudget(daemon: MockDaemon): void {
  // 中文注释：metadata push 引入第二条 E2EE WebSocket；这些断言只关心 terminal
  // 切换/超时后没有遗留旧 terminal transport，因此允许当前 terminal + metadata sidecar。
  expect(daemon.activeConnectionCount()).toBeGreaterThan(0);
  expect(daemon.activeConnectionCount()).toBeLessThanOrEqual(2);
}

async function clickSessionCard(
  user: ReturnType<typeof userEvent.setup>,
  name?: string,
  container: HTMLElement | Document = document.body,
): Promise<void> {
  const scope = container instanceof HTMLElement && !container.isConnected ? document.body : container;
  if (name) {
    const escapedName = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    await user.click(
      await within(scope as HTMLElement).findByRole("button", {
        name: new RegExp(`^Open ${escapedName}(?:, new output)?$`),
      }),
    );
    return;
  }
  const sessionButtons = await within(scope as HTMLElement).findAllByRole("button", { name: /^Open / });
  await user.click(sessionButtons[0]);
}

async function exerciseSupervisorBackedWebLifecycle(
  user: ReturnType<typeof userEvent.setup>,
  daemon: MockDaemon,
  input: {
    sessionId: UUID;
    readyText: string;
    cwd: string;
    fileName: string;
    inputText: string;
    postReconnectText: string;
  },
): Promise<void> {
  const sessionListRequests = () =>
    daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list");
  const terminalCreateStreams = () =>
    daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.create");
  const terminalAttachStreams = () =>
    daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach");

  await pairWithInvite(user, daemon);
  await waitForWorkspaceSession("No session");
  await waitFor(() => expect(sessionListRequests().length).toBeGreaterThan(0));

  await user.click(screen.getByRole("button", { name: "New session" }));

  const sessionName = fallbackSessionDisplayName(input.sessionId);
  await waitForWorkspaceSession(sessionName);
  await screen.findByText(new RegExp(input.readyText.trim()));
  expect(daemon.createdCommands).toEqual([[]]);
  expect(terminalCreateStreams()).toHaveLength(1);
  // 中文注释：supervisor-backed `terminal.create` 本身已经建立 watched terminal stream；
  // Web 端不能在 create 后再补一条 attach，否则 relay 排队时会形成重复终端流。
  expect(terminalAttachStreams()).toHaveLength(0);

  const panel = await screen.findByLabelText("session files");
  await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue(input.cwd));
  await within(panel).findByText(input.fileName);
  expect(daemon.sessionFileRequests).toContainEqual({ session_id: input.sessionId });

  const terminalInput = await waitFor(() => {
    const element = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(element).not.toBeNull();
    return element!;
  });
  await waitFor(() => expect(document.activeElement).toBe(terminalInput));
  expect(document.activeElement).not.toBe(terminalHost());

  terminalInput.value = input.inputText;
  fireEvent.input(terminalInput);

  await waitFor(() => expect(daemon.sessionDataMessages).toContain(input.inputText));
  expect(daemon.outerWireText()).not.toContain(input.inputText);

  const resizeCountBefore = daemon.sessionResizes.length;
  (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
    rows: 33,
    cols: 111,
  };
  terminalInput.focus();
  fireEvent(window, new Event("resize"));
  await waitFor(() =>
    expect(daemon.sessionResizes.slice(resizeCountBefore)).toContainEqual({
      session_id: input.sessionId,
      size: { rows: 33, cols: 111, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
    }),
  );

  daemon.dropConnections();
  await waitFor(
    () => expect(daemon.attachedSessions).toContain(input.sessionId),
    { timeout: 2200 },
  );

  daemon.pushTerminalFrameBatch(input.sessionId, [
    {
      kind: "snapshot",
      session_id: input.sessionId,
      base_seq: 0,
      terminal_seq: 1,
      size: { rows: 33, cols: 111, pixel_width: 0, pixel_height: 0 },
      data_base64: Buffer.from(input.postReconnectText, "utf8").toString("base64"),
    },
  ]);

  await screen.findByText(new RegExp(input.postReconnectText.trim()));
  expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
}

function visibleSessionNames(): string[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".session-row strong"))
    .map((element) => element.textContent?.trim() ?? "")
    .filter(Boolean);
}

function cssRuleBody(css: string, selector: string): string {
  // 中文注释：这里验证 selector 自己的声明块，避免只搜到零散属性却漏掉全局 button 居中回归。
  const selectorStart = css.indexOf(`${selector} {`);
  expect(selectorStart).toBeGreaterThanOrEqual(0);
  const bodyStart = css.indexOf("{", selectorStart);
  const bodyEnd = css.indexOf("}", bodyStart);
  expect(bodyStart).toBeGreaterThanOrEqual(0);
  expect(bodyEnd).toBeGreaterThan(bodyStart);
  return css.slice(bodyStart + 1, bodyEnd);
}

function selectedSessionName(): string | undefined {
  return document.querySelector<HTMLElement>(".session-row.selected strong")?.textContent?.trim();
}

function terminalHost(): HTMLElement | null {
  return document.querySelector<HTMLElement>(".terminal-host");
}

function terminalText(): string {
  // 中文注释：真实 xterm.js 直接使用 .terminal-host 作为 renderer element；
  // jsdom mock 会把可断言文本镜像到这个宿主节点，避免测试依赖不存在的内部 wrapper。
  return terminalHost()?.textContent ?? "";
}

function resetTerminalStats(): { writes: number; refreshes: number; writtenBytes: number } {
  const scope = globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number; refreshes: number; writtenBytes: number } };
  scope.__TERMD_TEST_TERMINAL_STATS__ = { writes: 0, refreshes: 0, writtenBytes: 0 };
  return scope.__TERMD_TEST_TERMINAL_STATS__;
}

async function waitForTerminalStatsToSettle(idleMs = 60, sampleMs = 20): Promise<void> {
  const scope = globalThis as { __TERMD_TEST_TERMINAL_STATS__?: { writes: number; refreshes: number; writtenBytes: number } };
  let stableMs = 0;
  let previous = JSON.stringify(scope.__TERMD_TEST_TERMINAL_STATS__ ?? {});
  while (stableMs < idleMs) {
    await new Promise((resolve) => window.setTimeout(resolve, sampleMs));
    const current = JSON.stringify(scope.__TERMD_TEST_TERMINAL_STATS__ ?? {});
    if (current === previous) {
      stableMs += sampleMs;
      continue;
    }
    previous = current;
    stableMs = 0;
  }
}

function triggerTerminalSelection(text: string): void {
  const scope = globalThis as { __TERMD_TEST_TERMINAL__?: { select: (text: string) => void } };
  expect(scope.__TERMD_TEST_TERMINAL__).toBeDefined();
  // 测试 mock 只暴露选择完成事件，避免测试直接依赖 xterm 内部 DOM 结构。
  scope.__TERMD_TEST_TERMINAL__!.select(text);
}

function mockTerminalLayout(input: {
  viewportWidth: number;
  viewportHeight: number;
  frameWidth: number;
  frameHeight: number;
  scrollHeight?: number;
}) {
  const clientWidthSpy = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? input.viewportWidth : 0;
  });
  const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? input.viewportHeight : 0;
  });
  const offsetWidthSpy = vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-frame") ? input.frameWidth : 0;
  });
  const offsetHeightSpy = vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-frame") ? input.frameHeight : 0;
  });
  const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-scrollport") ? (input.scrollHeight ?? input.frameHeight) : 0;
  });

  return () => {
    clientWidthSpy.mockRestore();
    clientHeightSpy.mockRestore();
    offsetWidthSpy.mockRestore();
    offsetHeightSpy.mockRestore();
    scrollHeightSpy.mockRestore();
  };
}

describe("termui web 工作台", () => {
  let daemon: MockDaemon;

  it("普通前端操作默认等待 5 秒，避免 relay 输出排队时过早报超时", () => {
    expect(APP_CONNECTION_TIMEOUT_MS).toBe(5000);
  });

  it("0 字节文件传输只有完成确认后才显示 100%", () => {
    const panelProps = {
      attachedSessionId: DEFAULT_SESSION_ID,
      activeTab: "files" as const,
      files: { session_id: DEFAULT_SESSION_ID, path: "/tmp", entries: [] },
      loading: false,
      gitLoading: false,
      followTerminalCwd: true,
      onTabChange: vi.fn(),
      onOpenDirectory: vi.fn(),
      onOpenFile: vi.fn(),
      onOpenGitFile: vi.fn(),
      onOpenGitDiff: vi.fn(),
      onGitAction: vi.fn(),
      onGoToPath: vi.fn(),
      onRefresh: vi.fn(),
      onRefreshGit: vi.fn(),
      onFollowTerminalCwdChange: vi.fn(),
      onUpload: vi.fn(),
      onDownload: vi.fn(),
      onDelete: vi.fn(),
      onHide: vi.fn(),
    };
    const { rerender } = render(
      <SessionFilesPanel
        {...panelProps}
        uploadProgress={{ name: "empty.txt", offsetBytes: 0, sizeBytes: 0, phase: "sending", completed: false }}
      />,
    );

    expect(
      screen.getByRole("status", { name: "Uploading empty.txt" }).querySelector<HTMLElement>(".files-transfer-bar-fill")
        ?.style.getPropertyValue("--files-transfer-progress"),
    ).toBe("0%");

    rerender(
      <SessionFilesPanel
        {...panelProps}
        uploadProgress={{ name: "empty.txt", offsetBytes: 0, sizeBytes: 0, phase: "confirmed", completed: true }}
      />,
    );
    expect(
      screen.getByRole("status", { name: "Uploading empty.txt" }).querySelector<HTMLElement>(".files-transfer-bar-fill")
        ?.style.getPropertyValue("--files-transfer-progress"),
    ).toBe("100%");

    rerender(
      <SessionFilesPanel
        {...panelProps}
        uploadProgress={{ name: "sent.bin", offsetBytes: 4, sizeBytes: 4, phase: "committing", completed: false }}
      />,
    );
    expect(screen.getByRole("status", { name: "Saving sent.bin" })).toBeInTheDocument();
    expect(
      screen.getByRole("status", { name: "Saving sent.bin" }).querySelector<HTMLElement>(".files-transfer-bar-fill")
        ?.style.getPropertyValue("--files-transfer-progress"),
    ).toBe("99%");
  });

  it("文件 path 输入编辑时不会被 cwd polling 覆盖", async () => {
    const user = userEvent.setup();
    const onGoToPath = vi.fn();
    const panelProps = {
      attachedSessionId: DEFAULT_SESSION_ID,
      activeTab: "files" as const,
      files: { session_id: DEFAULT_SESSION_ID, path: "/home/me", entries: [] },
      loading: false,
      gitLoading: false,
      followTerminalCwd: true,
      onTabChange: vi.fn(),
      onOpenDirectory: vi.fn(),
      onOpenFile: vi.fn(),
      onOpenGitFile: vi.fn(),
      onOpenGitDiff: vi.fn(),
      onGitAction: vi.fn(),
      onGoToPath,
      onRefresh: vi.fn(),
      onRefreshGit: vi.fn(),
      onFollowTerminalCwdChange: vi.fn(),
      onUpload: vi.fn(),
      onDownload: vi.fn(),
      onDelete: vi.fn(),
      onHide: vi.fn(),
    };
    const { rerender } = render(<SessionFilesPanel {...panelProps} />);

    const pathInput = screen.getByLabelText("Current directory");
    await user.clear(pathInput);
    await user.type(pathInput, "/home/me/project");

    rerender(
      <SessionFilesPanel
        {...panelProps}
        files={{ session_id: DEFAULT_SESSION_ID, path: "/tmp/work", entries: [] }}
      />,
    );

    expect(screen.getByLabelText("Current directory")).toHaveValue("/home/me/project");
    await user.click(screen.getByRole("button", { name: "Go" }));
    expect(onGoToPath).toHaveBeenCalledWith("/home/me/project");
  });

  beforeEach(async () => {
    // app 集成测试运行在 jsdom 中；这里固定使用 fallback 编辑器，Monaco 的生产加载由构建验证覆盖。
    (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__ = true;
    resetFileEditorDialogMonacoCacheForTests();
    await clearBrowserState();
    setViewportWidth(1366);
    setTouchCapability(false);
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 768,
      writable: true,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: undefined,
      writable: true,
    });
    qrScannerMock.destroy.mockClear();
    qrScannerMock.hasCamera.mockReset();
    qrScannerMock.hasCamera.mockResolvedValue(true);
    qrScannerMock.onDecode = undefined;
    qrScannerMock.options = undefined;
    qrScannerMock.scanImage.mockReset();
    qrScannerMock.start.mockReset();
    qrScannerMock.start.mockResolvedValue();
    qrScannerMock.stop.mockClear();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000401",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
    });
  });

  afterEach(async () => {
    restoreDocumentVisibility();
    resetFileEditorDialogMonacoCacheForTests();
    clearTermdDiagnosticsForTest();
    delete (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__;
    cleanup();
    await daemon.stop();
  });

  it("pairing 后清空 token，刷新 session list，并默认 attach 第一个 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitForWorkspaceSession();
    expect(screen.getByRole("button", { name: "Daemons" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit connection" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Manage daemons" })).toBeNull();
    expect(screen.queryByLabelText("Daemon")).toBeNull();
    await waitForWorkspaceSession();
    expect(document.body.textContent).not.toContain("00000000-0000-0000-0000-000000000401");
    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();

    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() =>
      expect(daemon.attachedSessions).toEqual([
        "00000000-0000-0000-0000-000000000401",
      ]),
    );
    expect(daemon.v070MetadataConnections).toBeGreaterThan(0);
    expect(daemon.v070TerminalConnections).toBeGreaterThan(0);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("在工作台和守护进程管理页显示当前构建版本", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    expect(screen.getByText(`v${APP_VERSION}`)).toHaveClass("app-version");
    await user.click(screen.getByRole("button", { name: "Daemons" }));
    await screen.findByLabelText("daemon admin");
    expect(screen.getByText(`v${APP_VERSION}`)).toHaveClass("app-version");
  });

  it("daemon.clients 超时不阻断 session list", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      dropDaemonClients: true,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitFor(() => expect(visibleSessionNames()).toEqual([DEFAULT_SESSION_NAME]));
    await new Promise((resolve) =>
      window.setTimeout(resolve, APP_CONNECTION_TIMEOUT_MS + 700),
    );

    expect(screen.queryByLabelText("Connection error")).toBeNull();
    expect(document.body.textContent).not.toContain("response_timeout");
  }, 15_000);

  it("metadata WebSocket ready 后不再请求 daemon.status HTTP", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.v070MetadataConnections).toBeGreaterThan(0));

    await new Promise((resolve) => window.setTimeout(resolve, DAEMON_LATENCY_POLL_INTERVAL_MS * 2 + 500));

    expect(
      daemon.receivedHttpRequests.some((request) => request.path === "/api/control/daemon/status"),
    ).toBe(false);
    expect(daemon.v070MetadataConnections).toBe(1);
  });

  it("daemon.status 旁路超时不污染已 attach 的终端", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "attached-ready\n",
      daemonStatusDelayMs: APP_CONNECTION_TIMEOUT_MS + 300,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/attached-ready/);
    await waitFor(() => expect(daemon.daemonStatusRequests).toBeGreaterThan(0));
    await new Promise((resolve) =>
      window.setTimeout(resolve, APP_CONNECTION_TIMEOUT_MS + 700),
    );

    expect(screen.queryByLabelText("Connection error")).toBeNull();
    expect(document.body.textContent).not.toContain("response_timeout");
    expect(document.body.textContent).not.toContain("operation timed out");
  }, 15_000);

  it("已 attach 后 terminal 走 terminal stream，普通 RPC 改走 HTTP 控制面", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() =>
      expect(daemon.receivedHttpRequests.some((request) => request.path === `/api/control/session/${DEFAULT_SESSION_ID}/files`)).toBe(true),
    );
    await waitFor(() =>
      expect(daemon.receivedHttpRequests.some((request) => request.path === `/api/control/session/${DEFAULT_SESSION_ID}/git`)).toBe(true),
    );
    expect(daemon.v070MetadataConnections).toBeGreaterThan(0);
    expect(daemon.v070TerminalConnections).toBeGreaterThan(0);
    expect(
      daemon.receivedHttpRequests.some((request) => /\/(?:attach|list|clients)(?:\/|$)/u.test(request.path)),
    ).toBe(false);
  });

  it("页面 hidden 时保持 metadata 和终端流，visible 后不重新 attach", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    setDocumentVisibility("hidden");
    daemon.pushSessionData(DEFAULT_SESSION_ID, "hidden-live-output\n");
    await screen.findByText(/hidden-live-output/);
    await new Promise((resolve) => window.setTimeout(resolve, DAEMON_LATENCY_POLL_INTERVAL_MS + 250));

    expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]);
    expect(daemon.v070MetadataConnections).toBe(1);

    const resizeCountBeforeResume = daemon.sessionResizes.length;
    setDocumentVisibility("visible");
    await waitFor(() => expect(daemon.sessionResizes.length).toBeGreaterThan(resizeCountBeforeResume));
    // 中文注释：hidden/visible 只是页面可见性变化，不能主动重建 terminal WebSocket；
    // 否则会触发 snapshot 重绘，并让后台已经持续接收的输出被重复恢复。
    expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path === "/api/control/daemon/status"),
    ).toBe(false);
    expect(screen.getByText(/hidden-live-output/)).toBeInTheDocument();
  });

  it("移动端仅靠 visibility 恢复也会探测半开 terminal WebSocket 且 focus 不重复 attach", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    setDocumentVisibility("hidden");
    daemon.suspendTerminalConnectionsWithoutClose();
    setDocumentVisibility("visible");
    fireEvent.focus(window);

    await waitFor(
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    await new Promise((resolve) => window.setTimeout(resolve, 300));
    expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]);
    expect(daemon.sessionDataMessages).toEqual([]);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("pagehide 会立即失效 terminal WebSocket 并在 pageshow 恢复当前 session", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    fireEvent(window, new Event("pagehide"));
    fireEvent(window, new Event("pageshow"));

    await waitFor(
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("visibility probe 在途时发生 pagehide 和 pageshow 也不会丢失恢复事件", async () => {
    setViewportWidth(390);
    const probeSpy = vi.spyOn(V070Client.prototype, "probeTerminalLiveness");
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    setDocumentVisibility("hidden");
    setDocumentVisibility("visible");
    expect(probeSpy).toHaveBeenCalledTimes(1);
    fireEvent(window, new Event("pagehide"));
    fireEvent(window, new Event("pageshow"));

    await waitFor(
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    await new Promise((resolve) => window.setTimeout(resolve, 300));
    expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("终端输出在 visible 路径排进 rAF 后切到 hidden 仍会补 timer flush", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);

    try {
      setDocumentHasFocus(true);
      rafQueue.clear();
      daemon.pushSessionData(DEFAULT_SESSION_ID, "hidden-raf-race-output\n");
      await waitFor(() => expect(rafQueue.size).toBeGreaterThan(0));

      setDocumentVisibility("hidden");
      await screen.findByText(/hidden-raf-race-output/);
    } finally {
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      restoreDocumentVisibility();
    }
  });

  it("终端输出在 visible 路径排进 rAF 后切到 blur 仍会补 timer flush", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);

    try {
      setDocumentHasFocus(true);
      daemon.pushSessionData(DEFAULT_SESSION_ID, "blur-raf-race-output\n");
      await waitFor(() => expect(rafQueue.size).toBeGreaterThan(0));

      setDocumentHasFocus(false);
      window.dispatchEvent(new Event("blur"));
      await screen.findByText(/blur-raf-race-output/);
    } finally {
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      restoreDocumentVisibility();
    }
  });

  it("窗口 blur 后新到的终端输出直接走 timer flush，不依赖后续 rAF", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const rafQueue = new Map<number, FrameRequestCallback>();
    let nextRafId = 1;
    const rafSpy = vi.spyOn(window, "requestAnimationFrame").mockImplementation(((callback: FrameRequestCallback) => {
      const rafId = nextRafId;
      nextRafId += 1;
      rafQueue.set(rafId, callback);
      return rafId;
    }) as typeof window.requestAnimationFrame);
    const cancelSpy = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(((rafId: number) => {
      rafQueue.delete(rafId);
    }) as typeof window.cancelAnimationFrame);

    try {
      setDocumentHasFocus(false);
      window.dispatchEvent(new Event("blur"));
      daemon.pushSessionData(DEFAULT_SESSION_ID, "blur-direct-flush-output\n");

      await screen.findByText(/blur-direct-flush-output/);
    } finally {
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
      restoreDocumentVisibility();
    }
  });

  it("窗口 blur/focus 不主动重建终端 WebSocket", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));
    const acceptedConnectionsBeforeBlur = daemon.acceptedConnections;

    window.dispatchEvent(new Event("blur"));
    daemon.pushSessionData(DEFAULT_SESSION_ID, "blur-live-output\n");
    await screen.findByText(/blur-live-output/);
    window.dispatchEvent(new Event("focus"));
    const terminalFrame = screen.getByTestId("terminal-pane").querySelector<HTMLElement>(".terminal-frame");
    expect(terminalFrame).not.toBeNull();
    await user.click(terminalFrame!);
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.value = "after-focus-input";
    fireEvent.input(terminalInput!);
    daemon.pushSessionData(DEFAULT_SESSION_ID, "post-focus-click-output\n");
    await new Promise((resolve) => window.setTimeout(resolve, 650));

    // 中文注释：普通窗口失焦不等于 transport 断开；focus 只能补状态轮询，
    // 点击 xterm 也只能走同一条 terminal WebSocket 的 resize/cursor/input segment，
    // 不能关闭当前 terminal stream 后重新 attach，否则会触发完整 snapshot 重绘。
    expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]);
    expect(daemon.acceptedConnections).toBe(acceptedConnectionsBeforeBlur);
    expect(daemon.sessionDataMessages).toContain("after-focus-input");
    expect(screen.getByText(/blur-live-output/)).toBeInTheDocument();
    await screen.findByText(/post-focus-click-output/);
  });

  it("页面 hidden 期间 metadata 保持连接且不关闭终端", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    setDocumentVisibility("hidden");
    await new Promise((resolve) => window.setTimeout(resolve, 100));

    // 中文注释：后台期间 status 这类普通 segment 可能超时；它只能影响状态栏，
    // 不能关闭承载 terminal stream 的当前 session WebSocket。
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expectTerminalAndMetadataConnectionBudget(daemon);

    setDocumentVisibility("visible");
    expect(daemon.v070MetadataConnections).toBe(1);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path === "/api/control/daemon/status"),
    ).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("已 attach 时旁路 RPC 超时不能关闭当前 session WebSocket", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
      daemonStatusDelayMs: APP_CONNECTION_TIMEOUT_MS + 500,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.daemonStatusRequests).toBeGreaterThan(0));
    await new Promise((resolve) =>
      window.setTimeout(resolve, APP_CONNECTION_TIMEOUT_MS + 700),
    );

    // 中文注释：单 WebSocket 模型下，status/files/git 这类非终端 RPC 可能被大输出排队。
    // 普通 request timeout 只能标记该 RPC 失败，不能关闭承载 terminal stream 的当前 session 连接。
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expectTerminalAndMetadataConnectionBudget(daemon);
  }, 15_000);

  it("已 attach 时旁路 RPC 关闭 socket 会走终端重连而不是卡在连接已关闭", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));
    const panel = await screen.findByLabelText("session files");
    const terminalConnectionCountBefore = daemon.activeConnectionCount();
    const attachedSessionCountBefore = daemon.attachedSessions.length;

    daemon.closeNextDaemonStatusRequests(1);
    await waitFor(() => expect(daemon.daemonStatusRequests).toBeGreaterThan(0));
    const acceptedAfterClose = daemon.acceptedConnections;

    await user.click(within(panel).getByRole("button", { name: "Refresh files" }));
    await new Promise((resolve) => window.setTimeout(resolve, 80));
    // 中文注释：HTTP 控制面失败不应误杀 terminal stream，也不应为了 files 刷新额外重建
    // 认证连接；当前 terminal attach 仍应保持原样。
    expect(daemon.acceptedConnections).toBe(acceptedAfterClose);
    expect(daemon.activeConnectionCount()).toBe(terminalConnectionCountBefore);
    expect(daemon.attachedSessions).toHaveLength(attachedSessionCountBefore);

    // 中文注释：新结构里 daemon.status 已迁到 HTTP 控制面；单次 status transport 失败
    // 只能影响旁路状态，不该触发 terminal attach 重连。
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.value = "after-status-close-input";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-status-close-input"));
  });

  it("metadata ready 后不制造 daemon.status HTTP 请求并保持终端可用", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() =>
      expect(daemon.receivedPackets.some((packet) => packet.method === "metadata.subscribe")).toBe(true),
    );

    await new Promise((resolve) => window.setTimeout(resolve, DAEMON_LATENCY_POLL_INTERVAL_MS * 2 + 500));

    expect(
      daemon.receivedHttpRequests.some((request) => request.path === "/api/control/daemon/status"),
    ).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.value = "after-status-auth-refresh";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-status-auth-refresh"));
  });

  it("session.files 慢响应只影响文件 panel，不卸载终端", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
      sessionFilesDelayMs: APP_CONNECTION_TIMEOUT_MS + 500,
      sessionFiles: {
        [DEFAULT_SESSION_ID]: {
          session_id: DEFAULT_SESSION_ID,
          path: "/slow/files",
          entries: [],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.sessionFileRequests.length).toBeGreaterThan(0));
    await waitFor(() => expect(within(screen.getByLabelText("session files")).getByText("unavailable")).toBeInTheDocument(), {
      timeout: APP_CONNECTION_TIMEOUT_MS + 1500,
    });

    // 慢文件请求只更新右侧 panel，不影响独立的 terminal WebSocket。
    const panel = await screen.findByLabelText("session files");
    expect(within(panel).getByText("unavailable")).toBeInTheDocument();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expectTerminalAndMetadataConnectionBudget(daemon);
  }, 15_000);

  it("切换 session 会关闭旧 WebSocket 并为新 session 重建连接", async () => {
    const user = userEvent.setup();
    const nextSession = {
      session_id: "00000000-0000-0000-0000-000000000402",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expectTerminalAndMetadataConnectionBudget(daemon));
    daemon.setSessions([
      {
        session_id: DEFAULT_SESSION_ID,
        state: "running",
        size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
      },
      nextSession,
    ]);
    await waitForWorkspaceSession("beta");
    const acceptedBeforeSwitch = daemon.acceptedConnections;

    await clickSessionCard(user, "beta");

    await waitFor(() => expect(daemon.attachedSessions).toContain(nextSession.session_id));
    // 中文注释：终端会话切换以 WebSocket 生命周期为边界。新 session 必须重新走
    // route/hello/auth/terminal.attach，旧连接关闭后 relay/daemon 都能用 transport close
    // 明确清理旧 client context。
    expect(daemon.acceptedConnections).toBeGreaterThan(acceptedBeforeSwitch);
    await waitFor(() => expectTerminalAndMetadataConnectionBudget(daemon), {
      timeout: 3500,
    });
  });

  it("切换 session 会取消尚未完成的 workspace WebSocket 握手", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000405",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000406",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000407",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    const acceptedBeforeSwitch = daemon.acceptedConnections;
    const releaseBetaRouteReady = daemon.holdNextRouteReady();

    try {
      fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
      await waitFor(() => expect(daemon.acceptedConnections).toBeGreaterThan(acceptedBeforeSwitch));

      fireEvent.click(screen.getByRole("button", { name: "Open gamma" }));

      // 中文注释：beta 的 relay route_ready 被故意卡住；切到 gamma 时必须 abort
      // 这个半开 workspace client，而不是复用它继续等到前一次握手超时。
      await waitFor(() => expect(daemon.acceptedConnections).toBeGreaterThanOrEqual(acceptedBeforeSwitch + 2), {
        timeout: 650,
      });
      await waitFor(() => expect(daemon.attachedSessions).toContain(gammaSession.session_id), {
        timeout: 1000,
      });
      await waitFor(() => expectTerminalAndMetadataConnectionBudget(daemon), {
        timeout: 1500,
      });
    } finally {
      releaseBetaRouteReady();
    }
    expect(selectedSessionName()).toBe("gamma");
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("切换 session 会取消卡在 auth.challenge 前的半开 WebSocket 握手", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000425",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000426",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000427",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    const acceptedBeforeSwitch = daemon.acceptedConnections;

    daemon.setDropAuthChallenge(true);
    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(daemon.acceptedConnections).toBeGreaterThan(acceptedBeforeSwitch));

    daemon.setDropAuthChallenge(false);
    fireEvent.click(screen.getByRole("button", { name: "Open gamma" }));

    await waitFor(() => expect(daemon.acceptedConnections).toBeGreaterThanOrEqual(acceptedBeforeSwitch + 2), {
      timeout: 1000,
    });
    await waitFor(() => expect(daemon.attachedSessions).toContain(gammaSession.session_id), {
      timeout: 1500,
    });
    await waitFor(() => expectTerminalAndMetadataConnectionBudget(daemon), {
      timeout: 2000,
    });
    expect(selectedSessionName()).toBe("gamma");
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("终端输入走同一条 terminal stream，不再拆出额外 attach stream", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    await waitFor(() => {
      const attachStreams = daemon.receivedPackets.filter(
        (packet) => packet.kind === "stream_open" && packet.method === "terminal.attach",
      );
      expect(attachStreams).toHaveLength(1);
    });

    const attachStreams = daemon.receivedPackets.filter(
      (packet) => packet.kind === "stream_open" && packet.method === "terminal.attach",
    );
    const outputStream = attachStreams[0];
    const payload = outputStream?.payload as { watch_updates?: boolean } | undefined;
    expect(payload?.watch_updates).toBe(true);
    expect(outputStream?.stream_id).toBeDefined();

    const terminalInput = await waitFor(() => {
      const textarea = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(textarea).not.toBeNull();
      return textarea!;
    });
    terminalInput.value = "isolated-input";
    fireEvent.input(terminalInput);

    await waitFor(() => expect(daemon.sessionDataMessages).toContain("isolated-input"));
    const inputChunks = daemon.receivedPackets.filter(
      (packet) => packet.kind === "stream_chunk" && (packet.payload as { session_id?: string }).session_id === DEFAULT_SESSION_ID,
    );
    expect(inputChunks.at(-1)?.stream_id).toBe(outputStream!.stream_id);
  });

  it("设置面板支持切换语言和浅色主题，并持久化到浏览器本地状态", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    await user.click(await screen.findByLabelText("Light"));
    await user.click(await screen.findByLabelText("中文"));

    await waitFor(() => expect(document.documentElement).toHaveAttribute("data-theme", "light"));
    expect(document.documentElement).toHaveAttribute("lang", "zh-CN");
    expect(screen.getByRole("dialog", { name: "设置" })).toBeVisible();
    expect(screen.getByLabelText("守护进程管理器")).toBeInTheDocument();
    expect(screen.queryByLabelText("daemon manager")).toBeNull();
    await waitFor(async () => {
      await expect(loadBrowserState()).resolves.toMatchObject({
        preferences: { language: "zh-CN", theme: "light" },
      });
    });
  });

  it("已 attach 终端切换主题会重建 xterm 并请求完整 snapshot", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    const initialTheme = document.documentElement.dataset.theme === "light" ? "light" : "dark";
    const nextTheme = initialTheme === "light" ? "dark" : "light";
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    await user.click(await screen.findByLabelText(nextTheme === "light" ? "Light" : "Dark"));

    await waitFor(() => expect(document.documentElement).toHaveAttribute("data-theme", nextTheme));
    await waitFor(
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    const reconnectAttach = daemon.attachRequests.at(-1);
    // 中文注释：主题变更要走完整 snapshot；如果带 last_terminal_seq，xterm 只会增量续写旧主题 buffer。
    expect(reconnectAttach).toMatchObject({ session_id: DEFAULT_SESSION_ID, watch_updates: true });
    expect(reconnectAttach?.last_terminal_seq ?? null).toBeNull();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("已配对 web 初次打开和刷新后自动 attach 第一个 session 并显示输出", async () => {
    const user = userEvent.setup();
    const firstRender = render(<App />);

    await waitFor(() => expect(document.title).toBe("Termd"));

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(document.title).toBe(`Termd - ${daemon.url} - ${DEFAULT_SESSION_NAME}`));
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    firstRender.unmount();
    render(<App />);

    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(document.title).toBe(`Termd - ${daemon.url} - ${DEFAULT_SESSION_NAME}`));
    await waitFor(() =>
      expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
    );
  });

  it("在底部状态栏显示 daemon 状态，移动端只保留核心指标", async () => {
    const user = userEvent.setup();
    const desktopRender = render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const desktopStatus = await screen.findByRole("contentinfo", { name: "daemon server status" });
    await within(desktopStatus).findByText("CPU");
    await within(desktopStatus).findByText("7.5%");
    expect(within(desktopStatus).getByRole("img", { name: "CPU usage bars" })).toBeInTheDocument();
    expect(within(desktopStatus).getByText("Mem")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("3.0 GB / 8.0 GB")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("Net")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("Disk")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("64 GB / 128 GB")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("Load")).toBeInTheDocument();
    expect(within(desktopStatus).getByText("Uptime")).toBeInTheDocument();
    expect(within(desktopStatus).queryByText("Procs")).toBeNull();
    expect(within(desktopStatus).queryByText(/atop/)).toBeNull();
    expect(within(desktopStatus).queryByRole("button", { name: "Refresh server status" })).toBeNull();
    expect(screen.queryByText("session active")).toBeNull();

    desktopRender.unmount();
    await daemon.stop();
    await clearBrowserState();
    setViewportWidth(390);
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const mobileStatus = await screen.findByRole("contentinfo", { name: "daemon server status" });
    await within(mobileStatus).findByText("CPU");
    expect(within(mobileStatus).getByText("Mem")).toBeInTheDocument();
    expect(within(mobileStatus).getByText("Net")).toBeInTheDocument();
    expect(within(mobileStatus).getByText("Disk")).toBeInTheDocument();
    expect(within(mobileStatus).queryByRole("button", { name: "Refresh server status" })).toBeNull();
    expect(within(mobileStatus).queryByText("Load")).toBeNull();
    expect(within(mobileStatus).queryByText("Uptime")).toBeNull();
    expect(within(mobileStatus).queryByText("Procs")).toBeNull();
    expect(within(mobileStatus).queryByText(/atop/)).toBeNull();
  });

  it("daemon 状态栏由 metadata WebSocket 驱动且不创建 HTTP 轮询", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.v070MetadataConnections).toBeGreaterThan(0));
    expect(
      daemon.receivedHttpRequests.some((request) => request.path === "/api/control/daemon/status"),
    ).toBe(false);
  });

  it("底部状态栏使用固定列宽，避免指标内容变化时横向抖动", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain('font-family: "HarmonyOS Sans SC";');
    expect(css).toContain('--font-ui: "HarmonyOS Sans SC", "HarmonyOS Sans", "Aptos", "Segoe UI", sans-serif;');
    expect(css).toContain("--daemon-status-cpu-width: 148px;");
    expect(css).toContain("--daemon-status-memory-width: 188px;");
    expect(css).toContain("--daemon-status-network-width: 184px;");
    expect(css).toContain("--daemon-status-disk-width: 188px;");
    expect(css).toContain("--daemon-status-load-width: 142px;");
    expect(css).toContain("--daemon-status-uptime-width: 132px;");
    expect(css).toContain("grid-template-columns: max-content minmax(0, 1fr);");
    expect(css).toContain("flex: 0 0 var(--daemon-status-memory-width);");
    expect(css).toContain("flex-basis: var(--daemon-status-cpu-width);");
    expect(css).toContain("flex-basis: var(--daemon-status-disk-width);");
    expect(css).toContain(".daemon-status-strip .daemon-status-load {\n    display: none;");
    expect(css).toContain("justify-content: start;");
  });

  it("浅色主题使用 Everforest soft light 底色，避免面板和终端大面积纯白", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("--color-bg-page: #e5dfc5;");
    expect(css).toContain("--color-surface: #f3ead3;");
    expect(css).toContain("--color-terminal-bg: #eae4ca;");
    expect(css).toContain("--color-text: #5c6a72;");
    expect(css).not.toContain("--color-surface: #ffffff;");
    expect(css).not.toContain("--color-toast-bg: rgba(255, 255, 255");
  });

  it("暗色主题使用 Everforest soft dark 底色，避免霓虹高对比黑绿", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const html = readFileSync(resolve(process.cwd(), "index.html"), "utf8");

    expect(css).toContain("--color-bg-page: #293136;");
    expect(css).toContain("--color-surface: #333c43;");
    expect(css).toContain("--color-terminal-bg: #293136;");
    expect(css).toContain("--color-text: #d3c6aa;");
    expect(css).toContain("--color-accent: #a7c080;");
    expect(html).toContain('<meta name="theme-color" content="#293136" />');
    expect(css).not.toContain("--color-accent: #d6ff5f;");
  });

  it("移动端状态栏和快捷输入栏固定占满父容器，避免内容变化带动宽度", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("position: fixed;\n    left: var(--termd-visual-viewport-offset-left, 0px);");
    expect(css).toContain("width: var(--termd-layout-viewport-width, var(--termd-visual-viewport-width, 100dvw));");
    expect(css).toContain("height: var(--termd-layout-viewport-height, var(--termd-visual-viewport-height, 100dvh));");
    expect(css).toContain("max-width: none;");
    expect(css).toContain(".daemon-status-strip {\n    width: 100%;");
    expect(css).toContain(".daemon-status-strip .daemon-status-grid {\n    width: 100%;");
    expect(css).toContain("display: grid;\n    grid-template-columns:\n      minmax(58px, 0.6fr)");
    expect(css).toContain("minmax(124px, 1.25fr);");
    const mobileShortcutsBlock = css.match(/\.terminal-mobile-shortcuts \{[^}]+\}/)?.[0] ?? "";
    expect(mobileShortcutsBlock).toContain("position: absolute;");
    expect(mobileShortcutsBlock).toContain("bottom: 0;");
    expect(mobileShortcutsBlock).toContain("width: 100%;");
    expect(css).toContain("overflow-x: auto;");
    expect(css).toContain("scrollbar-width: none;");
    expect(css).toContain("flex: 0 0 64px;");
    expect(css).toContain(".terminal-mobile-paste-button {\n    flex-basis: 82px;");
  });

  it("基于相邻 daemon 状态快照计算网卡上下行速度", () => {
    expect(networkRateFromSamples(undefined, { rxBytes: 1000, txBytes: 2000, sampledAtMs: 5000 })).toBeUndefined();
    expect(
      networkRateFromSamples(
        { rxBytes: 1000, txBytes: 2000, sampledAtMs: 5000 },
        { rxBytes: 2000, txBytes: 3500, sampledAtMs: 10_000 },
      ),
    ).toEqual({
      rxBytesPerSecond: 200,
      txBytesPerSecond: 300,
    });
    // daemon 重启或网卡计数器回退时，不展示错误的负速度。
    expect(
      networkRateFromSamples(
        { rxBytes: 2000, txBytes: 3500, sampledAtMs: 10_000 },
        { rxBytes: 1000, txBytes: 3600, sampledAtMs: 15_000 },
      ),
    ).toBeUndefined();
  });

  it("标题栏显示网络延迟，daemon 状态栏不再显示 RTT", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    const status = await screen.findByRole("contentinfo", { name: "daemon server status" });

    await waitFor(() => {
      const latency = document.querySelector<HTMLElement>(".toolbar-title .toolbar-latency");
      expect(latency).not.toBeNull();
      expect(latency?.textContent).toMatch(/\d+ms/);
    });
    expect(within(status).queryByText(/RTT/)).toBeNull();
    expect(daemon.v070MetadataConnections).toBeGreaterThan(0);
  });

  it("持续在每次 metadata pong 一秒后发起下一次延迟检测", async () => {
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
      metadataPingDelayMs: 250,
      closeMetadataOnPingNumber: 2,
    });
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(
      () => expect(daemon.metadataPingReceivedAtMs).toHaveLength(4),
      { timeout: DAEMON_LATENCY_POLL_INTERVAL_MS * 2 + 1_000 },
    );
    expect(daemon.v070MetadataConnections).toBeGreaterThanOrEqual(2);
    expect(daemon.metadataPongSentAtMs).toHaveLength(2);
    expect(
      daemon.metadataPingReceivedAtMs[1] - daemon.metadataPongSentAtMs[0],
    ).toBeGreaterThanOrEqual(DAEMON_LATENCY_POLL_INTERVAL_MS);
    expect(
      daemon.metadataPingReceivedAtMs[3] - daemon.metadataPongSentAtMs[1],
    ).toBeGreaterThanOrEqual(DAEMON_LATENCY_POLL_INTERVAL_MS);
  });

  it("标题栏 RTT 按延迟阈值返回颜色等级", () => {
    expect(latencyLevelClass(undefined)).toBeUndefined();
    expect(latencyLevelClass(50)).toBe("latency-good");
    expect(latencyLevelClass(51)).toBe("latency-warning");
    expect(latencyLevelClass(150)).toBe("latency-warning");
    expect(latencyLevelClass(151)).toBe("latency-danger");
  });

  it("可以通过拖动手柄调整 session 顺序，并在刷新后保留", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000402",
          name: "work",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          created_at_ms: 2000,
        },
        {
          session_id: "00000000-0000-0000-0000-000000000401",
          name: "shell",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          created_at_ms: 1000,
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("work");
    expect(visibleSessionNames()).toEqual(["work", "shell"]);

    const rows = document.querySelectorAll<HTMLElement>(".session-row");
    rows.forEach((row, index) => {
      row.getBoundingClientRect = vi.fn(() => ({
        x: 0,
        y: index * 60,
        width: 260,
        height: 52,
        top: index * 60,
        right: 260,
        bottom: index * 60 + 52,
        left: 0,
        toJSON: () => ({}),
      }));
    });

    const shellHandle = screen.getByRole("button", { name: "Drag shell" });
    fireEvent.mouseDown(shellHandle, { button: 0, clientY: 90 });
    fireEvent.mouseMove(shellHandle, { clientY: 10 });
    fireEvent.mouseUp(shellHandle, { clientY: 10 });

    await waitFor(() => expect(visibleSessionNames()).toEqual(["shell", "work"]));
    await waitFor(() =>
      expect(daemon.sessionReorders).toEqual([
        [
          "00000000-0000-0000-0000-000000000401",
          "00000000-0000-0000-0000-000000000402",
        ],
      ]),
    );

    await waitFor(() => expect(visibleSessionNames()).toEqual(["shell", "work"]));
  });

  it("初次加载 session list 时采用 daemon 返回顺序", async () => {
    const user = userEvent.setup();
    const workSession = {
      session_id: "00000000-0000-0000-0000-000000000402",
      name: "work",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    const shellSession = {
      session_id: "00000000-0000-0000-0000-000000000401",
      name: "shell",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 1000,
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [workSession, shellSession],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("work");
    expect(visibleSessionNames()).toEqual(["work", "shell"]);

    expect(daemon.sessionReorders).toEqual([]);
  });

  it("迟到的旧 session list 响应不能覆盖刚完成的拖拽排序", async () => {
    const user = userEvent.setup();
    const workSession = {
      session_id: "00000000-0000-0000-0000-000000000402",
      name: "work",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    const shellSession = {
      session_id: "00000000-0000-0000-0000-000000000401",
      name: "shell",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 1000,
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [workSession, shellSession],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("work");
    expect(visibleSessionNames()).toEqual(["work", "shell"]);

    daemon.queueSessionListResponse([workSession, shellSession], 30);

    await waitFor(() => {
      const rows = Array.from(document.querySelectorAll<HTMLElement>(".session-row"));
      expect(rows).toHaveLength(2);
      rows.forEach((row, index) => {
        vi.spyOn(row, "getBoundingClientRect").mockReturnValue({
          x: 0,
          y: index * 60,
          width: 260,
          height: 52,
          top: index * 60,
          right: 260,
          bottom: index * 60 + 52,
          left: 0,
          toJSON: () => ({}),
        });
      });
    });

    const shellHandle = screen.getByRole("button", { name: "Drag shell" });
    fireEvent.mouseDown(shellHandle, { button: 0, clientY: 90 });
    fireEvent.mouseMove(shellHandle, { clientY: 10 });
    fireEvent.mouseUp(shellHandle, { clientY: 10 });

    await waitFor(() => expect(visibleSessionNames()).toEqual(["shell", "work"]));
    await waitFor(() =>
      expect(daemon.sessionReorders).toEqual([
        [
          "00000000-0000-0000-0000-000000000401",
          "00000000-0000-0000-0000-000000000402",
        ],
      ]),
    );
    await new Promise((resolve) => window.setTimeout(resolve, 60));

    expect(visibleSessionNames()).toEqual(["shell", "work"]);
  });

  it("迟到的 session list 刷新不能把刚切换的 session 选中态改回第一行", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000411",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 3000,
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000412",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000413",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 1000,
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    expect(selectedSessionName()).toBe("alpha");

    daemon.queueSessionListResponse([alphaSession, betaSession, gammaSession], 120);
    await clickSessionCard(user, "gamma");
    await waitFor(() => expect(selectedSessionName()).toBe("gamma"));
    await new Promise((resolve) => window.setTimeout(resolve, 180));

    expect(selectedSessionName()).toBe("gamma");
  });

  it("快速切换 session 会关闭尚未完成的 attach 连接", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000431",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000432",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000433",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession],
      attachOutput: "attached-ready\n",
      attachDelayMs: 180,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    const cancelCount = () => daemon.receivedPackets.filter((packet) => packet.kind === "cancel").length;
    const beforeSwitch = cancelCount();

    await clickSessionCard(user, "beta");
    await waitFor(() => expect(cancelCount()).toBeGreaterThan(beforeSwitch));

    await clickSessionCard(user, "gamma");
    await waitFor(() => expect(selectedSessionName()).toBe("gamma"));
    await waitFor(() => expect(daemon.attachedSessions).toContain(gammaSession.session_id));
    expect(cancelCount()).toBeGreaterThan(beforeSwitch);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("迟到的旧 attach ack 必须取消旧 terminal stream，不能留下旧 session watcher", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000435",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000436",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000437",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession],
      attachOutput: "attached-ready\n",
      attachDelayMs: 180,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    daemon.receivedPackets.splice(0);
    daemon.attachedSessions.splice(0);

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() =>
      expect(
        daemon.receivedPackets.some((packet) =>
          packet.kind === "stream_open" &&
          packet.method === "terminal.attach" &&
          JSON.stringify(packet.payload).includes(betaSession.session_id)
        ),
      ).toBe(true),
    );

    fireEvent.click(screen.getByRole("button", { name: "Open gamma" }));
    await waitFor(() => expect(selectedSessionName()).toBe("gamma"));
    await waitFor(() => expect(daemon.attachedSessions).toContain(gammaSession.session_id));

    const betaAttach = daemon.receivedPackets.find((packet) =>
      packet.kind === "stream_open" &&
      packet.method === "terminal.attach" &&
      JSON.stringify(packet.payload).includes(betaSession.session_id)
    );
    expect(betaAttach?.stream_id).toBeDefined();
    await waitFor(() =>
      expect(
        daemon.receivedPackets.some((packet: ProtocolPacket) =>
          packet.kind === "cancel" && packet.stream_id === betaAttach?.stream_id
        ),
      ).toBe(true),
    );
    await waitFor(() =>
      expect(
        daemon.sentPacketLog.some(({ packet }) =>
          packet.kind === "stream_chunk" &&
          JSON.stringify(packet.payload).includes(gammaSession.session_id),
        ),
      ).toBe(true),
    );
    // 中文注释：gamma attach 的首屏输出会稍晚于 attach ack flush 到终端；
    // 先等当前目标 session 稳定，再统计 stale beta 输出是否误写入。
    await new Promise((resolve) => window.setTimeout(resolve, 30));

    const stats = resetTerminalStats();
    daemon.pushTerminalFrame(betaSession.session_id, {
      kind: "output",
      session_id: betaSession.session_id,
      data_base64: btoa("late-beta-output\n"),
      terminal_seq: 99,
    });
    await new Promise((resolve) => window.setTimeout(resolve, 30));

    expect(stats.writes).toBe(0);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("连续快速切换 session 只让最后一次 attach 生效", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000441",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000442",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gammaSession = {
      session_id: "00000000-0000-0000-0000-000000000443",
      name: "gamma",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const deltaSession = {
      session_id: "00000000-0000-0000-0000-000000000444",
      name: "delta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession, gammaSession, deltaSession],
      attachOutput: "attached-ready\n",
      attachDelayMs: 160,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    daemon.attachedSessions.splice(0);
    daemon.attachRequests.splice(0);

    const openSession = (name: string) => {
      fireEvent.click(screen.getByRole("button", { name: `Open ${name}` }));
    };
    for (const name of ["beta", "gamma", "alpha", "beta", "gamma", "alpha", "beta", "delta"]) {
      openSession(name);
    }

    await waitFor(() => expect(selectedSessionName()).toBe("delta"));
    await waitFor(() => expect(daemon.attachedSessions).toEqual([deltaSession.session_id]));
    const watchedAttachRequests = daemon.attachRequests.filter((request) => request.watch_updates !== false);
    expect(watchedAttachRequests).toEqual([{ session_id: deltaSession.session_id, watch_updates: true }]);
    expect(document.body.textContent).not.toContain("response_timeout");
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("切换 session 时立即停止当前输出 stream，避免旧输出继续占用渲染通道", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000451",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000452",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "alpha-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/alpha-ready/);

    const stats = resetTerminalStats();
    const cancelCount = () => daemon.receivedPackets.filter((packet) => packet.kind === "cancel").length;
    const beforeSwitch = cancelCount();

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    daemon.pushSessionData(alphaSession.session_id, "late-alpha-output\n");

    // 旧输出 stream 的关闭必须发生在 attach 合并窗口之前；否则旧 session 的大输出会继续占用
    // xterm 渲染和当前 session 连接，把新 session 的恢复拖慢。
    await new Promise((resolve) => window.setTimeout(resolve, 30));

    expect(cancelCount()).toBeGreaterThan(beforeSwitch);
    expect(stats.writes).toBe(0);
    expect(terminalText()).not.toContain("late-alpha-output");
  });

  it("新 attach 的输出必须等 TerminalPane reset 确认后才写入 xterm", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000461",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000462",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "session-ready\n",
    });
    const deferredResetConfirmations: Array<() => void> = [];
    (globalThis as { __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void })
      .__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__ = (confirm) => {
        deferredResetConfirmations.push(confirm);
      };
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await waitFor(() => expect(deferredResetConfirmations.length).toBeGreaterThan(0));
    while (deferredResetConfirmations.length > 0) {
      deferredResetConfirmations.shift()?.();
    }
    await screen.findByText(/session-ready/);

    const stats = resetTerminalStats();
    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(deferredResetConfirmations.length).toBeGreaterThan(0));
    await new Promise((resolve) => window.setTimeout(resolve, 300));

    expect(stats.writes).toBe(0);
    expect(terminalText()).not.toContain("session-ready");

    while (deferredResetConfirmations.length > 0) {
      deferredResetConfirmations.shift()?.();
    }

    await waitFor(() => expect(terminalText()).toContain("session-ready"));
    expect(stats.writes).toBeGreaterThan(0);
  });

  it("切换 session 后最后一笔 terminal_frame output 不需要输入也会刷新", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000471",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000472",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "alpha-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/alpha-ready/);
    (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
      .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
    (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__?: boolean })
      .__TERMD_TEST_DEFER_TERMINAL_RENDER_READY_AFTER_WRITE_CALLBACK__ = true;

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
    await new Promise((resolve) => window.setTimeout(resolve, 120));
    daemon.sessionDataMessages.length = 0;
    const stats = resetTerminalStats();

    daemon.pushTerminalFrameBatch(betaSession.session_id, [
      {
        kind: "snapshot",
        session_id: betaSession.session_id,
        base_seq: 0,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        data_base64: btoa("beta-snapshot\n"),
      },
      {
        kind: "output",
        session_id: betaSession.session_id,
        terminal_seq: 1,
        data_base64: btoa("beta-final-tail\n"),
      },
    ]);

    await waitFor(() =>
      expect(terminalText()).toContain("beta-final-tail"),
    );
    expect(stats.refreshes).toBeGreaterThanOrEqual(2);
    expect(daemon.sessionDataMessages).toEqual([]);
  });

  it("session 切换等待 reset 时不能丢弃最后目标 session 的输入", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000481",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000482",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "session-ready\n",
    });
    const deferredResetConfirmations: Array<() => void> = [];
    (globalThis as { __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void })
      .__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__ = (confirm) => {
        deferredResetConfirmations.push(confirm);
      };
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await waitFor(() => expect(deferredResetConfirmations.length).toBeGreaterThan(0));
    while (deferredResetConfirmations.length > 0) {
      deferredResetConfirmations.shift()?.();
    }
    await screen.findByText(/session-ready/);
    daemon.sessionDataMessages.length = 0;

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));
    await waitFor(() => expect(deferredResetConfirmations.length).toBeGreaterThan(0));
    const terminalInput = await waitFor(() => {
      const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(input).not.toBeNull();
      return input!;
    });
    fireEvent.input(terminalInput, { target: { value: "input-during-reset" } });

    await waitFor(() => expect(daemon.sessionDataMessages).toContain("input-during-reset"));
    const inputPacket = daemon.receivedPackets.find((packet) => {
      if (packet.kind !== "stream_chunk") {
        return false;
      }
      const payload = packet.payload as AttachFramePayload;
      const bytes = payload.data_bytes ?? sessionDataFromBase64(payload.data_base64 ?? "");
      try {
        const frame = decodeSupervisorTerminalClientFrame(bytes);
        return frame.type === "input" && new TextDecoder().decode(frame.data_bytes) === "input-during-reset";
      } catch {
        return false;
      }
    });
    expect(inputPacket?.stream_id).toBeDefined();
    const betaTerminalAttachPacket = daemon.receivedPackets.find((packet) =>
      packet.kind === "stream_open" &&
      packet.method === "terminal.attach" &&
      JSON.stringify(packet.payload).includes(betaSession.session_id) &&
      JSON.stringify(packet.payload).includes('"watch_updates":true')
    );
    expect(inputPacket?.stream_id).toBe(betaTerminalAttachPacket?.stream_id);

    while (deferredResetConfirmations.length > 0) {
      deferredResetConfirmations.shift()?.();
    }
  });

  it("旧控制连接的迟到失败不能触发旧 session 重连", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000491",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000492",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "session-ready\n",
      daemonStatusDelayMs: 250,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/session-ready/);
    await waitFor(() => expect(daemon.daemonStatusRequests).toBeGreaterThan(0));
    daemon.attachRequests.length = 0;
    const acceptedBeforeSwitch = daemon.acceptedConnections;

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));
    await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
    // 中文注释：首个 attach reconnect 延迟是 250ms；这里多等一段，确认旧 control
    // RPC 的 connection_closed 不会重新 attach 回 alpha。
    await new Promise((resolve) => window.setTimeout(resolve, 450));

    expect(selectedSessionName()).toBe("beta");
    expect(daemon.attachRequests.every((request) => request.session_id === betaSession.session_id)).toBe(true);
    expect(daemon.acceptedConnections).toBeLessThanOrEqual(acceptedBeforeSwitch + 2);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(document.body.textContent).not.toContain("connection_closed");
  });

  it("迟到的 Git 结果不能覆盖当前 session panel", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000493",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000494",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    const gitResult = (sessionId: string, branch: string): SessionGitResultPayload => ({
      session_id: sessionId,
      cwd: `/repo/${branch}`,
      repository_root: `/repo/${branch}`,
      worktrees: [
        {
          path: `/repo/${branch}`,
          branch,
          head: branch.slice(0, 6),
          is_current: true,
          staged: [],
          unstaged: [],
        },
      ],
      graph: [`* ${branch.slice(0, 6)} ${branch} commit`],
      error: null,
    });
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "session-ready\n",
      sessionFiles: {
        [alphaSession.session_id]: { session_id: alphaSession.session_id, path: "/repo/alpha", entries: [] },
        [betaSession.session_id]: { session_id: betaSession.session_id, path: "/repo/beta", entries: [] },
      },
      sessionGitDelayMsBySession: {
        [alphaSession.session_id]: APP_CONNECTION_TIMEOUT_MS - 400,
      },
      sessionGit: {
        [alphaSession.session_id]: gitResult(alphaSession.session_id, "alpha-branch"),
        [betaSession.session_id]: gitResult(betaSession.session_id, "beta-branch"),
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/session-ready/);
    await waitFor(() =>
      expect(daemon.sessionGitRequests.some((request) => request.session_id === alphaSession.session_id)).toBe(true),
    );

    fireEvent.click(screen.getByRole("button", { name: "Open beta" }));
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));
    await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
    const panel = await screen.findByLabelText("session files");
    await user.click(within(panel).getByRole("tab", { name: "Git" }));
    await waitFor(() => expect(within(panel).getAllByText("beta-branch").length).toBeGreaterThan(0));
    await waitFor(() =>
      expect(daemon.sessionGitRequests.some((request) => request.session_id === betaSession.session_id)).toBe(true),
    );

    // 中文注释：alpha 的旧 Git RPC 比 beta 晚返回；它不能再覆盖当前 beta panel。
    await new Promise((resolve) => window.setTimeout(resolve, APP_CONNECTION_TIMEOUT_MS));
    expect(within(panel).queryByText("alpha-branch")).toBeNull();
    expect(within(panel).getAllByText("beta-branch").length).toBeGreaterThan(0);
  });

  it("持续输出时合并写入 xterm，并且不为每个输出刷新布局", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);
    await new Promise((resolve) => window.setTimeout(resolve, 80));
    // 中文注释：这条用例只衡量持续输出阶段的 xterm drain/refresh 行为；
    // attach/snapshot 的尾帧 stabilize 必须先完全落稳，避免把前序刷新混进统计。
    await waitForTerminalStatsToSettle();
    daemon.sessionCursorUpdates.length = 0;
    const stats = resetTerminalStats();

    for (let index = 0; index < 80; index += 1) {
      daemon.pushSessionData(DEFAULT_SESSION_ID, `burst-output-${index}\n`);
    }

    await waitFor(() =>
      expect(terminalText()).toContain("burst-output-79"),
    );
    await new Promise((resolve) => window.setTimeout(resolve, 160));

    expect(stats.writes).toBeLessThan(80);
    // 队列真正 idle 后允许双帧 refresh 兜住 xterm 尾包绘制；持续输出期间仍不能逐条刷新。
    expect(stats.refreshes).toBeLessThanOrEqual(2);
    expect(daemon.sessionCursorUpdates.length).toBeLessThan(20);
  });

  it("后台 session 输出不混入当前终端，打开后从 snapshot 恢复", async () => {
    const user = userEvent.setup();
    const shellSessionId = "00000000-0000-0000-0000-000000000401";
    const workSessionId = "00000000-0000-0000-0000-000000000402";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: shellSessionId,
          name: "shell",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          created_at_ms: 1000,
        },
        {
          session_id: workSessionId,
          name: "work",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          created_at_ms: 2000,
        },
      ],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("shell");
    await clickSessionCard(user, "shell");
    await screen.findByText(/attached-ready/);

    daemon.pushSessionData(workSessionId, "background-work-output\n");

    await new Promise((resolve) => window.setTimeout(resolve, 80));
    expect(terminalText()).not.toContain("background-work-output");

    await clickSessionCard(user, "work");

    await screen.findByText(/background-work-output/);
  });

  it("xterm 鼠标选中后自动复制并提示复制成功", async () => {
    const user = userEvent.setup();
    const writeTextSpy = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);

    triggerTerminalSelection("termd-e2e-ready");

    await waitFor(() => expect(writeTextSpy).toHaveBeenCalledWith("termd-e2e-ready"));
    expect(await screen.findByRole("status")).toHaveTextContent("Copied");
  });

  it("点击 xterm 已渲染文字也能聚焦终端", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    const host = terminalHost();
    expect(terminalInput).not.toBeNull();
    expect(host).not.toBeNull();
    terminalInput!.blur();

    const renderedText = document.createElement("span");
    renderedText.textContent = "rendered-terminal-text";
    // xterm 的文字层会处理鼠标选择，真实浏览器里可能阻断冒泡阶段事件。
    // 测试这里显式阻断冒泡，确保外层捕获阶段仍能完成聚焦。
    renderedText.addEventListener("mousedown", (event) => event.stopPropagation());
    renderedText.addEventListener("click", (event) => event.stopPropagation());
    host!.append(renderedText);

    fireEvent.mouseDown(renderedText);
    fireEvent.click(renderedText);

    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    expect(document.activeElement).not.toBe(host);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
  });

  it("移动端顶部菜单保持 terminal-first，并把 daemon 管理放到独立后台页", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    expect(screen.queryByRole("navigation", { name: "mobile workspace menu" })).toBeNull();
    expect(screen.queryByRole("navigation", { name: "mobile workspace actions" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Clients" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Daemons" })).toBeNull();

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const menu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    expect(within(menu).getByRole("button", { name: "Daemons" })).toBeEnabled();
    expect(within(menu).getByRole("button", { name: "Sessions" })).toBeEnabled();
    expect(within(menu).getByRole("button", { name: "Files" })).toBeEnabled();
    expect(within(menu).getByRole("button", { name: "New" })).toBeEnabled();
    expect(within(menu).queryByRole("button", { name: "Refresh sessions" })).toBeNull();

    await user.click(within(menu).getByRole("button", { name: "Daemons" }));
    const admin = await screen.findByLabelText("daemon admin");
    expect(within(admin).getByLabelText("daemon manager")).toBeVisible();
    await user.click(within(admin).getByRole("button", { name: "Open workspace" }));
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() =>
      expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
    );

    await user.click(screen.getByRole("button", { name: "Open session list from title" }));
    const titleSessionsPanel = await screen.findByLabelText("sessions panel");
    await expect(titleSessionsPanel).toBeVisible();
    await user.click(within(titleSessionsPanel).getByRole("button", { name: "Close sessions panel" }));
    expect(screen.queryByLabelText("sessions panel")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const sessionsMenu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    await within(sessionsMenu).getByRole("button", { name: "Sessions" }).click();
    const sessionsPanel = await screen.findByLabelText("sessions panel");
    const refreshSessions = within(sessionsPanel).getByRole("button", { name: "Refresh sessions" });
    await expect(refreshSessions).toBeEnabled();
    await user.click(refreshSessions);
    await clickSessionCard(user, DEFAULT_SESSION_NAME, sessionsPanel);

    await waitFor(() => expect(screen.queryByLabelText("sessions panel")).toBeNull());
    await screen.findByText(/termd-e2e-ready/);
    expect(await screen.findByRole("contentinfo", { name: "daemon server status" })).toBeInTheDocument();
    expect(screen.queryByText("session active")).toBeNull();
    expect(screen.queryByLabelText("session operators")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const secondMenu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    expect(within(secondMenu).getByRole("button", { name: "Files" })).toBeEnabled();

    await within(secondMenu).getByRole("button", { name: "Files" }).click();
    const filesPanel = screen.getByLabelText("session files");
    await expect(filesPanel).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Hide files panel" }));
    await expect(screen.queryByLabelText("session files")).toBeNull();
  });

  it("移动端标题栏向下拖动不会创建 session list HTTP 且不打开 session 面板", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const httpRequestsBeforePull = daemon.receivedHttpRequests.length;
    const title = screen.getByRole("button", { name: "Open session list from title" });

    fireTouchPointer(title, "pointerdown", { pointerId: 7, clientX: 180, clientY: 18 });
    fireTouchPointer(title, "pointermove", { pointerId: 7, clientX: 182, clientY: 82 });
    fireTouchPointer(title, "pointerup", { pointerId: 7, clientX: 182, clientY: 82 });

    await new Promise((resolve) => window.setTimeout(resolve, 200));
    expect(daemon.receivedHttpRequests.slice(httpRequestsBeforePull).some((request) =>
      /\/(?:list|clients|status)(?:\/|$)/u.test(request.path)
    )).toBe(false);
    expect(screen.queryByLabelText("sessions panel")).toBeNull();
  });

  it("移动端软键盘打开时让快捷键栏贴近键盘并隐藏底部状态行", async () => {
    setViewportWidth(390);
    setMobileVisualViewport(820, 460, 20);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const shell = await waitFor(() => {
      const element = document.querySelector<HTMLElement>(".app-shell");
      expect(element).not.toBeNull();
      expect(element).toHaveClass("mobile-keyboard-open");
      return element!;
    });
    expect(shell.style.getPropertyValue("--termd-layout-viewport-height")).toBe("460px");
    expect(shell.style.getPropertyValue("--termd-visual-viewport-height")).toBe("460px");
    expect(shell.style.getPropertyValue("--termd-visual-viewport-keyboard-inset")).toBe("340px");
    expect(shell.style.getPropertyValue("--termd-visual-viewport-offset-top")).toBe("20px");
    expect(screen.getByRole("contentinfo", { name: "daemon server status" })).toHaveClass(
      "daemon-status-strip",
    );
    expect(screen.getByLabelText("mobile terminal shortcuts")).toBeInTheDocument();
  });

  it("移动端软键盘弹出时不上报更小 PTY 尺寸，只通过视觉位移露出输入区", async () => {
    setViewportWidth(390);
    const viewport = installMutableMobileVisualViewport(820, 820, 0);
    const restoreTerminalLayout = mockTerminalLayout({
      viewportWidth: 390,
      viewportHeight: 720,
      frameWidth: 390,
      frameHeight: 720,
    });
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    try {
      const user = userEvent.setup();
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession(DEFAULT_SESSION_NAME, { timeout: 5000 });
      await screen.findByText(/termd-e2e-ready/);

      const terminalInput = await waitFor(() => {
        const element = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(element).not.toBeNull();
        return element!;
      });
      terminalInput.focus();
      await waitFor(() =>
        expect(daemon.sessionResizes).toContainEqual({
          session_id: DEFAULT_SESSION_ID,
          size: { rows: 24, cols: 80, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
        }),
      );
      const resizeCountAfterFocus = daemon.sessionResizes.length;
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 12,
        cols: 80,
      };

      await act(async () => {
        viewport.setMetrics(820, 460, 20);
      });

      const shell = await waitFor(() => {
        const element = document.querySelector<HTMLElement>(".app-shell");
        expect(element).not.toBeNull();
        expect(element).toHaveClass("mobile-keyboard-open");
        return element!;
      });
      expect(shell.style.getPropertyValue("--termd-visual-viewport-keyboard-inset")).toBe("340px");
      expect(shell.style.getPropertyValue("--termd-visual-viewport-offset-top")).toBe("20px");
      expect(daemon.sessionResizes).toHaveLength(resizeCountAfterFocus);
    } finally {
      restoreTerminalLayout();
    }
  });

  it("移动端软键盘未打开时隐藏快捷键栏", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const shell = document.querySelector<HTMLElement>(".app-shell");
    expect(shell).not.toHaveClass("mobile-keyboard-open");
    expect(screen.queryByLabelText("mobile terminal shortcuts")).toBeNull();
  });

  it("移动端收起键盘后 visualViewport 事件不写回 PTY 尺寸", async () => {
    setViewportWidth(390);
    const viewport = installMutableMobileVisualViewport(820, 460, 20);
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 12,
      cols: 80,
    };
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const terminalInput = await waitFor(() => {
      const element = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(element).not.toBeNull();
      return element!;
    });
    terminalInput.focus();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 80));
    });
    // 中文注释：键盘打开时 visualViewport 变矮只代表输入法遮挡，不代表 PTY 应改成小尺寸。
    expect(daemon.sessionResizes).toEqual([]);

    terminalInput.blur();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 160));
    });
    const resizeCountAfterBlur = daemon.sessionResizes.length;
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };

    await act(async () => {
      viewport.setMetrics(820, 820, 0);
    });

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 64));
    });

    expect(daemon.sessionResizes.slice(resizeCountAfterBlur)).toEqual([]);
  });

  it("移动端 visualViewport 只改变高度时仍触发布局刷新但不写回 PTY", async () => {
    setViewportWidth(390);
    const viewport = installMutableMobileVisualViewport(820, 820, 0);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const terminalInput = await waitFor(() => {
      const element = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(element).not.toBeNull();
      return element!;
    });
    terminalInput.focus();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 64));
    });
    const resizeCountAfterFocus = daemon.sessionResizes.length;

    await act(async () => {
      viewport.setMetrics(820, 760, 0);
    });
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 64));
    });

    // 中文注释：移动输入保护期内首轮 focus resize 可能被延后；该用例只要求
    // visualViewport 高度变化不额外写回 PTY 尺寸。
    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterFocus);
  });

  it("未保存 daemon 时手动 token 不会猜测 server_id", async () => {
    const user = userEvent.setup();
    render(<App />);

    await setConnectionUrl(user, daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await screen.findByText("pairing_server_unknown: pairing requires a known daemon server id");
    expect(daemon.outerWireLog).toEqual([]);
  });

  it("metadata WebSocket error envelope 会在 admin 主体显示 alert", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      routePreludeError: {
        code: "invalid_envelope",
        message: "message envelope is invalid",
      },
    });
    render(<App />);

    await setConnectionUrl(user, daemon.url);
    fireEvent.change(screen.getByLabelText("Pairing token"), {
      target: { value: pairingInviteCode(daemon) },
    });
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    const admin = await screen.findByLabelText("daemon admin", {}, { timeout: 5000 });
    const alert = await within(admin).findByRole("alert", { name: "Connection error" }, { timeout: 5000 });
    expect(alert).toHaveTextContent("invalid_envelope");
    expect(alert).toHaveTextContent("message envelope is invalid");
    expect(await screen.findByText("invalid_envelope: message envelope is invalid")).toBeInTheDocument();
    expect(screen.getByLabelText("Pairing token")).toHaveValue("");
  });

  it("已配对后可以把连接地址改成 relay /ws URL", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const relayUrl = `${daemon.url}?relay_token=relay-secret`;
    await user.click(screen.getByRole("button", { name: "Daemons" }));
    await setConnectionUrl(user, relayUrl);
    await user.click(screen.getByRole("button", { name: "Save URL" }));

    await waitForWorkspaceSession();
    await expectDaemonUrlInAdmin(user, relayUrl);
    await user.click(screen.getByRole("button", { name: "Open workspace" }));
    await waitForWorkspaceSession();
  });

  it("一个 Web 可以保存并切换多个 daemon", async () => {
    const user = userEvent.setup();
    const secondDaemon = await MockDaemon.start({
      token: "second-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000421",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    try {
      await setConnectionUrl(user, daemon.url);
      fireEvent.change(screen.getByLabelText("Pairing token"), {
        target: { value: pairingInviteCode(daemon) },
      });
      await user.click(screen.getByRole("button", { name: "Pair" }));
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      await setConnectionUrl(user, secondDaemon.url);
      await user.clear(screen.getByLabelText("Pairing token"));
      fireEvent.change(screen.getByLabelText("Pairing token"), {
        target: { value: pairingInviteCode(secondDaemon, "second-token") },
      });
      await user.click(screen.getByRole("button", { name: "Pair" }));
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const manager = await screen.findByLabelText("daemon manager");
      expect(within(manager).getByText(daemon.url)).toBeInTheDocument();
      expect(within(manager).getByText(secondDaemon.url)).toBeInTheDocument();
      expect(screen.queryByLabelText("Daemon")).toBeNull();

      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const refreshedManager = await screen.findByLabelText("daemon manager");
      const sessionListRequestsBeforeSwitch = daemon.receivedPackets.filter(
        (packet) => packet.kind === "request" && packet.method === "session.list",
      ).length;
      await user.click(within(refreshedManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitFor(() => expect(
        daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list").length,
      ).toBeGreaterThan(sessionListRequestsBeforeSwitch));
      await waitForWorkspaceSession(DEFAULT_SESSION_NAME, { timeout: 5000 });
    } finally {
      await secondDaemon.stop();
    }
  });

  it("daemon 管理面支持重命名和删除 daemon", async () => {
    const user = userEvent.setup();
    const secondDaemon = await MockDaemon.start({
      token: "second-token",
      sessions: [],
    });
    render(<App />);

    try {
      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession(DEFAULT_SESSION_NAME, { timeout: 5000 });

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      await setConnectionUrl(user, secondDaemon.url);
      fireEvent.change(screen.getByLabelText("Pairing token"), {
        target: { value: pairingInviteCode(secondDaemon, "second-token") },
      });
      await user.click(screen.getByRole("button", { name: "Pair" }));
      await waitForWorkspaceSession("No session", { timeout: 5000 });

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const manager = await screen.findByLabelText("daemon manager");
      expect(within(manager).getByText(daemon.url)).toBeInTheDocument();
      expect(within(manager).getByText(secondDaemon.url)).toBeInTheDocument();

      await user.click(within(manager).getAllByRole("button", { name: /Rename daemon/ })[1]);
      const nameInput = within(manager).getByLabelText("Daemon name");
      await user.clear(nameInput);
      await user.type(nameInput, "Laptop relay");
      await user.click(within(manager).getByRole("button", { name: "Save daemon name" }));

      await within(manager).findByText("Laptop relay");
      expect(screen.getByLabelText("selected daemon")).toHaveTextContent("Laptop relay");

      await user.click(within(manager).getByRole("button", { name: /Delete daemon Laptop relay/ }));
      const afterDeleteManager = await screen.findByLabelText("daemon manager");
      expect(within(afterDeleteManager).getByText(daemon.url)).toBeInTheDocument();
      await waitFor(() => expect(screen.queryByText("Laptop relay")).toBeNull());

      const remainingManager = afterDeleteManager;
      await user.click(within(remainingManager).getByRole("button", { name: /Delete daemon/ }));

      await waitFor(() => expect(within(screen.getByLabelText("daemon manager")).getByText("No daemons")).toBeVisible());
      expect(await screen.findByLabelText("Pairing token")).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "New session" })).toBeNull();
    } finally {
      await secondDaemon.stop();
    }
  });

  it("选到不可用 daemon 后会回到后台管理页，并可切回可用 daemon", async () => {
    const user = userEvent.setup();
    const secondDaemon = await MockDaemon.start({
      token: "second-token",
      sessions: [],
    });
    let secondStopped = false;
    render(<App />);

    try {
      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      await setConnectionUrl(user, secondDaemon.url);
      fireEvent.change(screen.getByLabelText("Pairing token"), {
        target: { value: pairingInviteCode(secondDaemon, "second-token") },
      });
      await user.click(screen.getByRole("button", { name: "Pair" }));
      await waitForWorkspaceSession("No session", { timeout: 5000 });

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const initialManager = await screen.findByLabelText("daemon manager");
      await user.click(within(initialManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitForWorkspaceSession(DEFAULT_SESSION_NAME, { timeout: 5000 });

      await secondDaemon.stop();
      secondStopped = true;

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const manager = await screen.findByLabelText("daemon manager");
      await user.click(within(manager).getByRole("button", { name: /Use daemon Daemon 2/ }));

      const recoveredAdmin = await screen.findByLabelText("daemon admin");
      const recoveredManager = within(recoveredAdmin).getByLabelText("daemon manager");
      expect(recoveredManager).toBeVisible();
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(secondDaemon.url));

      await user.click(within(recoveredManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      const sessionListRequestsBeforeRecovery = daemon.receivedPackets.filter(
        (packet) => packet.kind === "request" && packet.method === "session.list",
      ).length;
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitFor(() => expect(
        daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list").length,
      ).toBeGreaterThan(sessionListRequestsBeforeRecovery));
      await waitForWorkspaceSession(DEFAULT_SESSION_NAME, { timeout: 5000 });
    } finally {
      if (!secondStopped) {
        await secondDaemon.stop();
      }
    }
  }, 20_000);

  it("点击 session 卡片直接进入 shared-control operator", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    await waitForWorkspaceSession();
    await clickSessionCard(user);

    await screen.findByText(/termd-e2e-ready/);
    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Attached" })).toBeNull();
    expect(daemon.attachedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]);
  });

  it("WebSocket error envelope 会在 workspace 主体显示 alert 且不泄漏敏感字段", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000402",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionDataError: {
        code: "invalid_envelope_token",
        message: "message envelope is invalid private_key=private-value signature=sig ciphertext_base64=abc",
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.value = "workspace-input";
    fireEvent.input(terminalInput!);

    const workspaceBody = document.querySelector<HTMLElement>(".workspace-body");
    expect(workspaceBody).not.toBeNull();
    const alert = await within(workspaceBody!).findByRole("alert", { name: "Connection error" });
    expect(alert).toHaveTextContent("protocol_error");
    expect(alert).toHaveTextContent("protocol operation failed");
    expect(screen.queryByText("session active")).toBeNull();

    const renderedText = document.body.textContent ?? "";
    for (const sensitive of ["invalid_envelope_token", "private_key", "private-value", "signature", "ciphertext_base64"]) {
      expect(renderedText).not.toContain(sensitive);
    }
  });

  it("移动端 PWA 恢复后会静默重新 attach 当前 session", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    daemon.dropConnections();

    await waitFor(() =>
      expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    await screen.findByText(/termd-e2e-ready/);
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("attach WebSocket 短断时保留终端并静默重连当前 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    let sawConnectionAlert = false;
    const observer = new MutationObserver(() => {
      if (document.querySelector('[role="alert"][aria-label="Connection error"]')) {
        sawConnectionAlert = true;
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
    daemon.dropConnections();
    await waitFor(() => expect(daemon.activeConnectionCount()).toBe(0));
    await new Promise((resolve) => setTimeout(resolve, 80));

    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expect(screen.getByText(/termd-e2e-ready/)).toBeInTheDocument();
    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2200 },
    );
    const reconnectAttach = daemon.attachRequests.at(-1);
    expect(reconnectAttach?.session_id).toBe(DEFAULT_SESSION_ID);
    // v0.7 重连会重新接收权威 snapshot，已经渲染的首屏不能重复显示。
    const terminalText = screen.getByTestId("terminal-pane").textContent ?? "";
    expect(terminalText.match(/termd-e2e-ready/g) ?? []).toHaveLength(1);
    observer.disconnect();
    expect(sawConnectionAlert).toBe(false);
  });

  it("attach WebSocket 短断恢复后还能继续渲染新的 terminal frame", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    let sawConnectionAlert = false;
    const observer = new MutationObserver(() => {
      if (document.querySelector('[role="alert"][aria-label="Connection error"]')) {
        sawConnectionAlert = true;
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
    daemon.dropConnections();

    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2200 },
    );

    const stats = resetTerminalStats();
    daemon.pushTerminalFrameBatch(DEFAULT_SESSION_ID, [
      {
        kind: "snapshot",
        session_id: DEFAULT_SESSION_ID,
        base_seq: 0,
        terminal_seq: 1,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        data_base64: btoa("post-reconnect-snapshot\n"),
      },
      {
        kind: "output",
        session_id: DEFAULT_SESSION_ID,
        terminal_seq: 1,
        data_base64: btoa("post-reconnect-output\n"),
      },
    ]);

    await waitFor(() =>
      expect(terminalText()).toContain(
        "post-reconnect-output",
      ),
    );
    observer.disconnect();
    expect(stats.writes).toBeGreaterThan(0);
    expect(sawConnectionAlert).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("attach WebSocket error 时保留终端并静默重连当前 session", async () => {
    type WsEmitter = WebSocket & { emit: (event: "error", error: Error) => boolean };
    const OriginalWebSocket = globalThis.WebSocket;
    const sockets: WsEmitter[] = [];
    const CapturingWebSocket = class extends (OriginalWebSocket as unknown as { new(url: string, protocols?: string | string[]): WebSocket }) {
      constructor(url: string, protocols?: string | string[]) {
        super(url, protocols);
        sockets.push(this as unknown as WsEmitter);
      }
    } as unknown as typeof WebSocket;
    (globalThis as unknown as { WebSocket: typeof WebSocket }).WebSocket = CapturingWebSocket;
    const user = userEvent.setup();
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await screen.findByText(/termd-e2e-ready/);
      await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

      let sawConnectionAlert = false;
      const observer = new MutationObserver(() => {
        if (document.querySelector('[role="alert"][aria-label="Connection error"]')) {
          sawConnectionAlert = true;
        }
      });
      observer.observe(document.body, { childList: true, subtree: true });
      // 中文注释：直接触发客户端 WebSocket error，覆盖 close/drop 之外的 transport error 重连路径。
      sockets.at(-1)?.emit("error", new Error("mock transport error"));
      await waitFor(
        () =>
          expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
        { timeout: 2200 },
      );

      expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
      expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
      await screen.findByText(/termd-e2e-ready/);
      observer.disconnect();
      expect(sawConnectionAlert).toBe(false);
    } finally {
      (globalThis as unknown as { WebSocket: typeof WebSocket }).WebSocket = OriginalWebSocket;
    }
  });

  it("connection closed 后会静默按短延迟重试当前 session", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    daemon.dropConnections();

    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2200 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("daemon 重启窗口的 relay tunnel 503 会继续重试当前 session", async () => {
    const originalAttachSession = V070Client.prototype.attachSession;
    let failNextAttach = false;
    const attachSpy = vi.spyOn(V070Client.prototype, "attachSession").mockImplementation(async function (
      this: V070Client,
      sessionId,
    ) {
      if (failNextAttach) {
        failNextAttach = false;
        throw new ProtocolClientError("relay_tunnel_failed", "relay could not forward the application request");
      }
      return originalAttachSession.call(this, sessionId);
    });
    const user = userEvent.setup();
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await screen.findByText(/termd-e2e-ready/);
      await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

      failNextAttach = true;
      daemon.dropConnections();

      await waitFor(
        () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
        { timeout: 3500 },
      );
      expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
      expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    } finally {
      attachSpy.mockRestore();
    }
  });

  it("浏览器 offline 后 online 会丢弃半开 WebSocket 并重连当前 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    window.dispatchEvent(new Event("offline"));
    await waitFor(() => expect(daemon.activeConnectionCount()).toBe(0));

    // 中文注释：浏览器 offline 不保证 WebSocket 及时 close；online 时必须基于当前
    // session 重新建立 workspace client，而不是复用旧的半开 transport。
    window.dispatchEvent(new Event("online"));

    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("terminal transport 恢复期间输入不会静默丢失，会在重连后补发到当前 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    window.dispatchEvent(new Event("offline"));
    await waitFor(() => expect(daemon.activeConnectionCount()).toBe(0));

    window.dispatchEvent(new Event("online"));
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    // 中文注释：这里模拟“页面还显示当前 session，但 attach transport 刚断开/正在恢复”时的输入。
    // App 不能直接 return 掉这段输入，否则 relay 恢复窗口里用户敲下的首条命令会凭空消失。
    terminalInput!.value = "queued-after-online-reconnect";
    fireEvent.input(terminalInput!);

    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("queued-after-online-reconnect"));
  });

  it("大段 UTF-8 输入按安全帧大小切块且不拆分 emoji", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    const terminalInput = await waitFor(() => {
      const inputElement = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(inputElement).not.toBeNull();
      return inputElement!;
    });
    const input = `${"a".repeat(64 * 1024 - 1)}🙂${"界".repeat(4096)}`;
    terminalInput.value = input;
    fireEvent.input(terminalInput);

    await waitFor(() => expect(daemon.sessionDataMessages.join("")).toBe(input));
    expect(daemon.sessionDataMessages.length).toBeGreaterThan(1);
    for (const chunk of daemon.sessionDataMessages) {
      expect(new TextEncoder().encode(chunk).byteLength).toBeLessThanOrEqual(64 * 1024);
    }
  });

  it.each([
    { label: "首块", failedCall: 1 },
    { label: "中间块", failedCall: 2 },
  ])("终端输入在$label发送失败后保留未发送尾部并在重连后续传", async ({ failedCall }) => {
    const originalSendSessionData = V070Client.prototype.sendSessionData;
    let sendCalls = 0;
    const sendSpy = vi.spyOn(V070Client.prototype, "sendSessionData").mockImplementation(async function (
      this: V070Client,
      sessionId,
      bytes,
    ) {
      sendCalls += 1;
      if (sendCalls === failedCall) {
        throw new ProtocolClientError("connection_closed", "injected terminal send failure");
      }
      await originalSendSessionData.call(this, sessionId, bytes);
    });
    const user = userEvent.setup();
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

      const terminalInput = await waitFor(() => {
        const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(input).not.toBeNull();
        return input!;
      });
      const input = `${"first-".repeat(12_000)}🙂${"tail-".repeat(5_000)}`;
      terminalInput!.value = input;
      fireEvent.input(terminalInput!);

      await waitFor(
        () => expect(daemon.sessionDataMessages.join("")).toBe(input),
        { timeout: 4000 },
      );
      expect(sendCalls).toBeGreaterThan(failedCall);
      expect(daemon.attachedSessions.length).toBeGreaterThanOrEqual(2);
    } finally {
      sendSpy.mockRestore();
    }
  });

  it("输入超过离线队列字节预算时丢弃最新溢出并显示安全错误", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    const terminalInput = await waitFor(() => {
      const inputElement = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(inputElement).not.toBeNull();
      return inputElement!;
    });
    const overflowMarker = "must-not-be-queued";
    terminalInput.value = `${"界".repeat(400_000)}${overflowMarker}`;
    fireEvent.input(terminalInput);

    const alert = await screen.findByRole("alert", { name: "Connection error" });
    expect(alert).toHaveTextContent("terminal_input_overflow");
    expect(alert).not.toHaveTextContent(overflowMarker);

    await waitFor(() => expect(daemon.sessionDataMessages.length).toBeGreaterThan(0));
    const received = daemon.sessionDataMessages.join("");
    expect(new TextEncoder().encode(received).byteLength).toBeLessThanOrEqual(1024 * 1024);
    expect(received).not.toContain(overflowMarker);
  });

  it("session 切换会丢弃旧 session 的离线输入且不会串写到新 session", async () => {
    const originalSendSessionData = V070Client.prototype.sendSessionData;
    let failAlphaInput = true;
    const sendSpy = vi.spyOn(V070Client.prototype, "sendSessionData").mockImplementation(async function (
      this: V070Client,
      sessionId,
      bytes,
    ) {
      if (failAlphaInput && sessionId === DEFAULT_SESSION_ID) {
        failAlphaInput = false;
        throw new ProtocolClientError("connection_closed", "injected alpha transport failure");
      }
      await originalSendSessionData.call(this, sessionId, bytes);
    });
    const user = userEvent.setup();
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000499",
      name: "beta-buffer-target",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          name: "alpha-buffer-source",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
        betaSession,
      ],
      attachOutput: "session-ready\n",
    });
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession("alpha-buffer-source");
      const terminalInput = await waitFor(() => {
        const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(input).not.toBeNull();
        return input!;
      });
      terminalInput.value = "alpha-offline-secret";
      fireEvent.input(terminalInput);
      await waitFor(() => expect(failAlphaInput).toBe(false));

      fireEvent.click(screen.getByRole("button", { name: "Open beta-buffer-target" }));
      await waitFor(() => expect(selectedSessionName()).toBe("beta-buffer-target"));
      await waitFor(
        () => expect(daemon.attachedSessions).toContain(betaSession.session_id),
        { timeout: 3000 },
      );

      expect(daemon.sessionDataMessages).not.toContain("alpha-offline-secret");
      const betaInput = await waitFor(() => {
        const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(input).not.toBeNull();
        return input!;
      });
      betaInput.value = "beta-after-reconnect";
      fireEvent.input(betaInput);
      await waitFor(() => expect(daemon.sessionDataMessages).toContain("beta-after-reconnect"));
      expect(daemon.sessionDataMessages).not.toContain("alpha-offline-secret");
    } finally {
      sendSpy.mockRestore();
    }
  });

  it("focus 和 online 恢复重叠时不会重复 attach 当前 session", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
      attachDelayMs: 220,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    const attachCountBeforeReconnect = daemon.attachRequests.length;
    daemon.dropConnections();
    await waitFor(() => expect(daemon.activeConnectionCount()).toBe(0));

    // 中文注释：真实浏览器里 relay/系统恢复可能把 focus、online 等多个恢复入口挤在一起。
    // 这里只要当前 session 已经在恢复，就不能再发第二次 terminal.attach。
    fireEvent(window, new Event("focus"));
    fireEvent(window, new Event("online"));

    await waitFor(
      () =>
        expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2800 },
    );
    await new Promise((resolve) => window.setTimeout(resolve, 320));

    expect(daemon.attachRequests).toHaveLength(attachCountBeforeReconnect + 1);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("relay 后台恢复时保持 workspace，不依赖侧栏手动刷新", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    await new Promise((resolve) =>
      window.setTimeout(resolve, APP_CONNECTION_TIMEOUT_MS + 700),
    );

    expect(screen.queryByLabelText("daemon admin")).toBeNull();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
  }, 12_000);

  it("relay 慢握手时首次 session 连接不能被普通超时误杀", async () => {
    const user = userEvent.setup();
    render(<App />);

    await setConnectionUrl(user, daemon.url);
    fireEvent.change(screen.getByLabelText("Pairing token"), {
      target: { value: pairingInviteCode(daemon, "secret-token") },
    });
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());

    // 中文注释：relay 真实路径包含浏览器->relay、relay->daemon mux、E2EE 和 auth。
    // session 连接建立阶段不能继续使用普通 RPC 预算，否则 relay 正常但 Web 会自己关闭半开连接。
    daemon.delayNextRouteReady(APP_CONNECTION_TIMEOUT_MS + 500);
    await waitFor(
      () =>
        expect(
          daemon.receivedPackets.filter((packet) => packet.kind === "request" && packet.method === "session.list").length,
        ).toBeGreaterThan(0),
      { timeout: APP_CONNECTION_TIMEOUT_MS + 4000 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  }, 12_000);

  it("relay 慢 session.list 时首次工作台加载保持在 workspace 并等待长预算结果", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "bootstrap-ready\n",
    });
    // 中文注释：首次进入 workspace 时，relay 真实链路上的 session.list 可能比普通 5s
    // RPC 更慢，但这不应该把页面打回 admin，也不应该立刻升级成全局连接错误。
    daemon.queueSessionListResponse([
      {
        session_id: DEFAULT_SESSION_ID,
        state: "running",
        size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
      },
    ], APP_CONNECTION_TIMEOUT_MS + 500);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitFor(() => expect(screen.queryByLabelText("daemon admin")).toBeNull());
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();

    await waitFor(() => expect(visibleSessionNames()).toEqual([DEFAULT_SESSION_NAME]), {
      timeout: APP_CONNECTION_TIMEOUT_MS + 4000,
    });
    await waitFor(() => expect(daemon.attachedSessions).toContain(DEFAULT_SESSION_ID), {
      timeout: APP_CONNECTION_TIMEOUT_MS + 4000,
    });
    await screen.findByText(/bootstrap-ready/);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  }, 18_000);

  it("移动端空 workspace 手动刷新时 session.list 瞬时失败不打回 admin", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    expect(screen.queryByLabelText("daemon admin")).toBeNull();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const menu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    await user.click(within(menu).getByRole("button", { name: "Daemons" }));
    const admin = await screen.findByLabelText("daemon admin");
    await user.click(within(admin).getByRole("button", { name: "Open workspace" }));
    await waitForWorkspaceSession("No session");

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const sessionsMenu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    await user.click(within(sessionsMenu).getByRole("button", { name: "Sessions" }));
    const sessionsPanel = await screen.findByLabelText("sessions panel");
    daemon.closeNextSessionListRequests(1);
    await user.click(within(sessionsPanel).getByRole("button", { name: "Refresh sessions" }));
    await new Promise((resolve) => window.setTimeout(resolve, 120));

    // 中文注释：移动端空工作台里的手动 Refresh 仍属于 workspace 内的旁路 session.list。
    // relay/HTTP 控制面瞬断只能让这一次刷新失败，不能把页面切回 admin 或弹出全局断线。
    expect(screen.queryByLabelText("daemon admin")).toBeNull();
    expect(screen.getByRole("button", { name: "Open mobile workspace menu" })).toBeInTheDocument();
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByText("No session")).toBeInTheDocument();
  });

  it("relay 恢复慢握手时重新 attach 使用长超时并静默恢复", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    daemon.delayNextRouteReady(APP_CONNECTION_TIMEOUT_MS + 500);
    daemon.dropConnections();

    // 中文注释：断线后的重新 attach 是 terminal 恢复路径，必须使用 attach 级长超时；
    // 如果复用普通 RPC 超时，这里会在 route_ready 到达前失败并显示连接错误。
    await waitFor(
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: APP_CONNECTION_TIMEOUT_MS + 4000 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  }, 12_000);

  it("terminal resync 的 attach 重连第一次失败后会继续排第二次并恢复当前 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    daemon.failNextWatchedTerminalAttaches(1);
    daemon.pushTerminalFrame(DEFAULT_SESSION_ID, {
      kind: "output",
      session_id: DEFAULT_SESSION_ID,
      terminal_seq: 5,
      data_base64: "b3V0LW9mLXN5bmMK",
    });

    await waitFor(() => expect(daemon.failedTerminalAttachRequests).toBe(1), { timeout: 1200 });
    await waitFor(() => expect(daemon.attachedSessions.length).toBeGreaterThanOrEqual(2), { timeout: 2800 });
    expect(daemon.attachedSessions[0]).toBe(DEFAULT_SESSION_ID);
    expect(daemon.attachedSessions.at(-1)).toBe(DEFAULT_SESSION_ID);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("移动端软键盘可以通过 beforeinput 输入空格、逗号和数字", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();

    const spaceEvent = dispatchMobileTextInput(terminalInput!, " ");
    const commaEvent = dispatchMobileTextInput(terminalInput!, ",");
    const digitEvent = dispatchMobileTextInput(terminalInput!, "1");

    expect(spaceEvent.defaultPrevented).toBe(true);
    expect(commaEvent.defaultPrevented).toBe(true);
    expect(digitEvent.defaultPrevented).toBe(true);
    await waitFor(() => expect(daemon.sessionDataMessages).toEqual([" ", ",", "1"]));
  });

  it("移动端中文组合输入期间 beforeinput 空格不会额外发送到终端", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const terminalInput = await waitFor(() => {
      const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(input).not.toBeNull();
      return input!;
    });
    terminalInput.focus();

    terminalInput.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    const candidateSpaceEvent = dispatchMobileTextInput(terminalInput, " ", { isComposing: true });
    terminalInput.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "你" }));

    expect(candidateSpaceEvent.defaultPrevented).toBe(false);
    expect(daemon.sessionDataMessages).toEqual([]);

    // 中文注释：组合输入最终内容仍交给 xterm 的 input/composition 逻辑发送，fallback 不重复发送候选空格。
    terminalInput.value = "你";
    fireEvent.input(terminalInput);
    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["你"]));
  });

  it("移动端可以通过快捷栏按钮和原生粘贴事件输入剪贴板文本", async () => {
    setViewportWidth(390);
    setMobileVisualViewport(844, 520);
    const user = userEvent.setup();
    const readTextSpy = vi.fn<() => Promise<string>>(() => Promise.resolve("shortcut-paste"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        ...navigator.clipboard,
        readText: readTextSpy,
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const terminalInput = await waitFor(() => {
      const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(input).not.toBeNull();
      return input!;
    });
    terminalInput.focus();

    await user.click(await screen.findByRole("button", { name: "Paste" }));
    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["shortcut-paste"]));

    const pasteEvent = dispatchMobilePasteInput(terminalInput, "native-paste");
    expect(pasteEvent.defaultPrevented).toBe(true);
    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["shortcut-paste", "native-paste"]));

    const clipboardPasteEvent = dispatchMobileClipboardPaste(terminalInput, "clipboard-event-paste");
    expect(clipboardPasteEvent.defaultPrevented).toBe(true);
    await waitFor(() =>
      expect(daemon.sessionDataMessages).toEqual(["shortcut-paste", "native-paste", "clipboard-event-paste"]),
    );
  });

  it("终端搜索会查询 session snapshot，并支持切换命中结果", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    await screen.findByText(/termd-e2e-ready/);
    daemon.pushSessionData(DEFAULT_SESSION_ID, "alpha beta\nbeta gamma\n");
    await screen.findByText(/beta gamma/);

    const terminalPane = screen.getByTestId("terminal-pane");
    await user.click(within(terminalPane).getByRole("button", { name: "Search terminal" }));
    const searchInput = await within(terminalPane).findByPlaceholderText("Search scrollback");
    await user.type(searchInput, "beta");
    await user.keyboard("{Enter}");

    await waitFor(() =>
      expect(daemon.sessionSearchRequests).toContainEqual({
        session_id: DEFAULT_SESSION_ID,
        query: "beta",
        case_sensitive: false,
        max_results: 80,
      }),
    );
    await within(terminalPane).findByText("1/2");
    await waitFor(() =>
      expect(within(terminalPane).getByTestId("terminal-search-highlight")).toHaveTextContent("beta"),
    );
    await user.click(within(terminalPane).getByRole("button", { name: "Next match" }));
    await within(terminalPane).findByText("2/2");
  });

  it("可以创建 session 并自动 attach 到 terminal", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "web-session-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");

    expect(screen.queryByLabelText("Command")).toBeNull();
    await user.click(screen.getByRole("button", { name: "New session" }));

    await waitForWorkspaceSession();
    expect(screen.queryByRole("button", { name: "Attached" })).toBeNull();
    await screen.findByText(/web-session-ready/);
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    expect(document.activeElement).not.toBe(terminalHost());
    expect(daemon.createdCommands).toEqual([[]]);
    // 中文注释：terminal.create 已经打开 terminal stream；新建会话后不能再追加一次
    // terminal.attach，否则慢 relay 下第二个 attach response 会被 create stream 输出阻塞。
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.create"),
    ).toHaveLength(1);
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach"),
    ).toHaveLength(0);
  });

  it("空工作台新建 session 只新增一条 terminal WebSocket", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "workspace-reuse-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    await waitFor(() =>
      expect(
        daemon.receivedPackets.some(
          (packet) => packet.kind === "request" && packet.method === "metadata.subscribe",
        ),
      ).toBe(true),
    );
    const acceptedConnectionsBeforeCreate = daemon.acceptedConnections;

    await user.click(screen.getByRole("button", { name: "New session" }));

    await screen.findByText(/workspace-reuse-ready/);
    expect(daemon.acceptedConnections).toBe(acceptedConnectionsBeforeCreate + 1);
    expect(daemon.v070MetadataConnections).toBe(1);
    expect(daemon.v070TerminalConnections).toBe(1);
  });

  it("新建 session 将 terminal.create 作为终端级请求处理", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "slow-create-ready\n",
    });
    const createSpy = vi.spyOn(V070Client.prototype, "createSession");
    render(<App />);

    try {
      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession("No session");
      await user.click(screen.getByRole("button", { name: "New session" }));

      await waitFor(() => expect(createSpy).toHaveBeenCalled());
      expect(createSpy.mock.calls.at(-1)).toHaveLength(2);
      await waitFor(() => expect(visibleSessionNames()).toHaveLength(1));
      await waitFor(() => expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull());
      await screen.findByText(/slow-create-ready/);
    } finally {
      createSpy.mockRestore();
    }
  });

  it("terminal.create 响应前到达的首屏输出不会被丢弃", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      createOutputBeforeResponse: "early-create-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    await user.click(screen.getByRole("button", { name: "New session" }));

    await waitFor(() => expect(visibleSessionNames()).toHaveLength(1));
    await screen.findByText(/early-create-ready/);
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.create"),
    ).toHaveLength(1);
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach"),
    ).toHaveLength(0);
  });

  it("terminal.create 的首屏输出必须等 TerminalPane reset 确认后才写入 xterm", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "create-reset-ready\n",
    });
    const resetConfirmations: Array<() => void> = [];
    (globalThis as { __TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__?: (confirm: () => void) => void })
      .__TERMD_TEST_DEFER_OUTPUT_RESET_APPLIED__ = (confirm) => {
        resetConfirmations.push(confirm);
      };
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    await user.click(screen.getByRole("button", { name: "New session" }));
    await waitFor(() => expect(visibleSessionNames()).toHaveLength(1));

    // 中文注释：create 与普通 attach 一样会切换 xterm 实例。reset 未确认前不能消费
    // create stream 的 snapshot，否则切换回来时可能把首屏写进旧实例或被下一次 reset 重放。
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 80));
    });
    expect(terminalText()).not.toContain("create-reset-ready");

    expect(resetConfirmations.length).toBeGreaterThan(0);
    for (const confirm of resetConfirmations.splice(0)) {
      confirm();
    }

    await screen.findByText(/create-reset-ready/);
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.create"),
    ).toHaveLength(1);
    expect(
      daemon.receivedPackets.filter((packet) => packet.kind === "stream_open" && packet.method === "terminal.attach"),
    ).toHaveLength(0);
  });

  it("新建 session 进行中不会把空列表显示成 No sessions", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "web-session-ready\n",
      sessionCreateDelayMs: 160,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    await user.click(screen.getByRole("button", { name: "New session" }));

    const sessionsRegion = screen.getByRole("region", { name: "sessions" });
    await within(sessionsRegion).findByText("Creating session");
    expect(within(sessionsRegion).queryByText("No sessions")).toBeNull();
  });

  it("新建 session 成功后由 metadata 更新保持在列表中", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "web-session-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");
    await user.click(screen.getByRole("button", { name: "New session" }));
    await waitFor(() => expect(visibleSessionNames()).toHaveLength(1));
    const createdName = visibleSessionNames()[0];
    const createdSession = daemon.createdCommands.length;
    expect(createdSession).toBe(1);
    await new Promise((resolve) => window.setTimeout(resolve, 120));

    await waitFor(() => expect(visibleSessionNames()).toEqual([createdName]));
    expect(screen.queryByText("No sessions")).toBeNull();
    expect(
      daemon.receivedHttpRequests.some((request) => /\/(?:list|clients)(?:\/|$)/u.test(request.path)),
    ).toBe(false);
  });

  it("新建 session 后不输入内容也会刷新初始回显", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "idle-shell-prompt$ ",
    });
    (globalThis as { __TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
      .__TERMD_TEST_DEFER_TERMINAL_RENDER_UNTIL_WRITE_CALLBACK__ = true;
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");

    await user.click(screen.getByRole("button", { name: "New session" }));

    const terminalInput = await waitFor(() => {
      const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(input).not.toBeNull();
      return input!;
    });
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    expect(document.activeElement).not.toBe(terminalHost());
    await waitFor(() =>
      expect(terminalText()).toContain("idle-shell-prompt$ "),
    );
    expect(daemon.decryptedInputs).toEqual([]);
  });

  it("新建多个 session 时已有 session 名称保持稳定", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "web-session-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");

    await user.click(screen.getByRole("button", { name: "New session" }));
    await waitFor(() => expect(visibleSessionNames()).toHaveLength(1));
    const firstName = visibleSessionNames()[0];
    expect(firstName).not.toMatch(/^Shell \d+$/);

    await user.click(screen.getByRole("button", { name: "New session" }));
    await waitFor(() => expect(visibleSessionNames()).toHaveLength(2));
    const namesAfterSecondCreate = visibleSessionNames();

    expect(namesAfterSecondCreate[1]).toBe(firstName);
    expect(namesAfterSecondCreate[0]).not.toBe(firstName);
    expect(namesAfterSecondCreate.every((name) => !/^Shell \d+$/.test(name))).toBe(true);
  });

  it("文件 panel 支持切换目录、上传、下载和删除", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000411";
    const rootPath = "/home/me/project";
    const srcPath = "/home/me/project/src";
    const rootFiles = {
      session_id: sessionId,
      path: rootPath,
      entries: [
        {
          name: "src",
          path: srcPath,
          kind: "directory",
          size_bytes: 0,
          modified_at_ms: null,
        },
        {
          name: "alpha.txt",
          path: "/home/me/project/alpha.txt",
          kind: "file",
          size_bytes: 12,
          modified_at_ms: 1_710_000_000_000,
        },
      ],
    } satisfies SessionFilesResultPayload;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: rootFiles,
        [rootPath]: rootFiles,
        [srcPath]: {
          session_id: sessionId,
          path: srcPath,
          entries: [
            {
              name: "main.rs",
              path: "/home/me/project/src/main.rs",
              kind: "file",
              size_bytes: 13,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp": {
          session_id: sessionId,
          path: "/tmp",
          entries: [],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "beta.log",
              path: "/tmp/work/beta.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    expect(panel.querySelector(".files-panel-header .files-path")).toBeNull();

    await within(panel).findByText("src");
    await within(panel).findByText("alpha.txt");
    expect(within(panel).getByText("12 B")).toBeInTheDocument();
    expect(daemon.sessionFileRequests[0]).toEqual({ session_id: sessionId });

    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    expect(followToggle).toBeChecked();

    const fileRequestCountBeforeRefresh = daemon.sessionFileRequests.length;
    await user.click(within(panel).getByRole("button", { name: "Refresh files" }));
    await waitFor(() => expect(daemon.sessionFileRequests.length).toBeGreaterThan(fileRequestCountBeforeRefresh));
    expect(daemon.sessionFileRequests.slice(fileRequestCountBeforeRefresh)).toContainEqual({ session_id: sessionId });

    await user.click(followToggle);
    expect(followToggle).not.toBeChecked();

    await user.click(within(panel).getByRole("button", { name: "Open src" }));
    await within(panel).findByText("main.rs");
    expect(daemon.sessionFileRequests).toContainEqual({ session_id: sessionId, path: srcPath });

    await user.click(within(panel).getByRole("button", { name: "Parent directory" }));
    await within(panel).findByText("alpha.txt");

    await user.click(within(panel).getByRole("button", { name: "Edit alpha.txt" }));
    const editor = await screen.findByRole("dialog", { name: "alpha.txt" });
    await waitFor(() => {
      expect(daemon.sessionFileReadRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
      });
    });
    expect(daemon.sessionFileDownloadChunkRequests).toEqual([]);
    const fileText = within(editor).getByLabelText("File text") as HTMLTextAreaElement;
    fireEvent.change(fileText, { target: { value: "edited from browser" } });
    await user.click(within(editor).getByRole("button", { name: "Save" }));
    await waitFor(() => {
      expect(daemon.sessionFileWrites).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
        text: "edited from browser",
      });
    });
    await user.click(within(editor).getByRole("button", { name: "Close editor" }));

    const downloadMock = installHttpDownloadMock(
      daemon,
      sessionId,
      "/home/me/project/alpha.txt",
      "alpha.txt",
      new TextEncoder().encode("hello world\n"),
    );
    try {
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
      await waitFor(() => {
        expect(downloadMock.calls()).toBe(1);
        expect(daemon.binaryPacketLog.some((entry) => entry.direction === "out" && entry.payload_type === "file_chunk")).toBe(false);
      });
    } finally {
      downloadMock.restore();
    }
    expect(daemon.sessionFileDownloadChunkRequests).toEqual([]);

    expect(within(panel).queryByRole("button", { name: "Copy alpha.txt" })).toBeNull();
    expect(within(panel).queryByRole("button", { name: "Move alpha.txt" })).toBeNull();

    await user.click(within(panel).getByRole("button", { name: "Delete alpha.txt" }));
    await waitFor(() => {
      expect(daemon.sessionFileDeletes).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
      });
    });
    await waitFor(() => {
      expect(within(panel).getByLabelText("Current directory")).toHaveValue(rootPath);
      expect(within(panel).getByLabelText("Current directory")).toBeEnabled();
    });

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp");
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await waitFor(() => {
      expect(daemon.sessionFileRequests).toContainEqual({ session_id: sessionId, path: "/tmp" });
    });
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp"));
    await within(panel).findByText("empty directory");

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "work");
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await waitFor(() => {
      expect(daemon.sessionFileRequests).toContainEqual({ session_id: sessionId, path: "/tmp/work" });
    });
    await within(panel).findByText("beta.log");

    await user.click(within(panel).getByRole("button", { name: "Parent directory" }));
    await within(panel).findByText("empty directory");

    const uploadFile = new File(["uploaded web file\n"], "notes.txt", { type: "text/plain" });
    const uploadMock = installHttpUploadOnceMock(daemon, sessionId, "/tmp/notes.txt", uploadFile);
    try {
      await user.upload(
        within(panel).getByLabelText("Upload file"),
        uploadFile,
      );
      await waitFor(() => {
        const uploaded = uploadMock.uploads.find((write) => write.path === "/tmp/notes.txt");
        expect(uploaded?.session_id).toBe(sessionId);
        expect(Array.from(uploaded?.bytes ?? [])).toEqual(Array.from(new TextEncoder().encode("uploaded web file\n")));
      });
    } finally {
      uploadMock.restore();
    }
    expect(
      daemon.receivedPacketLog.some((entry) => entry.packet.method === "session.file_upload"),
    ).toBe(false);
    expect(daemon.sessionFileWrites.some((write) => write.path === "/tmp/notes.txt")).toBe(false);
  });

  it("旧文件读取迟到后不会复活或覆盖当前编辑器", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000423";
    const alphaPath = "/home/me/project/alpha.txt";
    const betaPath = "/home/me/project/beta.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            { name: "alpha.txt", path: alphaPath, kind: "file", size_bytes: 6, modified_at_ms: null },
            { name: "beta.txt", path: betaPath, kind: "file", size_bytes: 5, modified_at_ms: null },
          ],
        },
      },
      sessionFileReads: {
        [alphaPath]: {
          session_id: sessionId,
          path: alphaPath,
          data_base64: Buffer.from("alpha\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
        [betaPath]: {
          session_id: sessionId,
          path: betaPath,
          data_base64: Buffer.from("beta\n", "utf8").toString("base64"),
          size_bytes: 5,
          modified_at_ms: null,
        },
      },
      sessionFileReadDelayMsByPath: {
        [alphaPath]: 120,
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await within(panel).findByText("alpha.txt");
    await within(panel).findByText("beta.txt");

    await user.click(within(panel).getByRole("button", { name: "Edit alpha.txt" }));
    await screen.findByRole("dialog", { name: "alpha.txt" });
    await waitFor(() => expect(daemon.sessionFileReadRequests).toContainEqual({ session_id: sessionId, path: alphaPath }));
    await user.click(screen.getByRole("button", { name: "Close editor" }));

    await user.click(within(panel).getByRole("button", { name: "Edit beta.txt" }));
    const betaEditor = await screen.findByRole("dialog", { name: "beta.txt" });
    await waitFor(() => expect(within(betaEditor).getByLabelText("File text")).toHaveValue("beta\n"));
    await waitFor(() => expect(daemon.sessionFileReadRequests).toContainEqual({ session_id: sessionId, path: betaPath }));

    await new Promise((resolve) => window.setTimeout(resolve, 180));
    expect(screen.getByRole("dialog", { name: "beta.txt" })).toBeInTheDocument();
    expect(screen.getByLabelText("File text")).toHaveValue("beta\n");
    expect(screen.queryByText("alpha\n")).toBeNull();
  });

  it("旧文件保存迟到后不会在切换 session 后复活编辑器", async () => {
    const user = userEvent.setup();
    const alphaSessionId = "00000000-0000-0000-0000-000000000423";
    const betaSessionId = "00000000-0000-0000-0000-000000000424";
    const alphaPath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: alphaSessionId,
          name: "alpha",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
        {
          session_id: betaSessionId,
          name: "beta",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [alphaSessionId]: {
          session_id: alphaSessionId,
          path: "/home/me/project",
          entries: [
            { name: "alpha.txt", path: alphaPath, kind: "file", size_bytes: 6, modified_at_ms: null },
          ],
        },
        [betaSessionId]: {
          session_id: betaSessionId,
          path: "/srv/beta",
          entries: [],
        },
      },
      sessionFileReads: {
        [alphaPath]: {
          session_id: alphaSessionId,
          path: alphaPath,
          data_base64: Buffer.from("alpha\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
      sessionFileWriteDelayMsByPath: {
        [alphaPath]: 140,
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await clickSessionCard(user, "alpha");

    const panel = await screen.findByLabelText("session files");
    await within(panel).findByText("alpha.txt");

    await user.click(within(panel).getByRole("button", { name: "Edit alpha.txt" }));
    const editor = await screen.findByRole("dialog", { name: "alpha.txt" });
    const fileText = within(editor).getByLabelText("File text") as HTMLTextAreaElement;
    fireEvent.change(fileText, { target: { value: "saved from alpha" } });
    await user.click(within(editor).getByRole("button", { name: "Save" }));
    await waitFor(() => {
      expect(daemon.sessionFileWrites).toContainEqual({
        session_id: alphaSessionId,
        path: alphaPath,
        text: "saved from alpha",
      });
    });

    await clickSessionCard(user, "beta");
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "alpha.txt" })).toBeNull());
    await waitFor(() => expect(within(screen.getByLabelText("session files")).queryByText("alpha.txt")).toBeNull());

    await new Promise((resolve) => window.setTimeout(resolve, 220));
    expect(selectedSessionName()).toBe("beta");
    expect(screen.queryByRole("dialog", { name: "alpha.txt" })).toBeNull();
    expect(within(screen.getByLabelText("session files")).queryByText("alpha.txt")).toBeNull();
  });

  it("上传进度在切换 session 后仍保留", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000431",
      name: "alpha",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000432",
      name: "beta",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      sessionFiles: {
        [alphaSession.session_id]: {
          session_id: alphaSession.session_id,
          path: "/home/alpha",
          entries: [],
        },
        [betaSession.session_id]: {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await clickSessionCard(user, "alpha");
    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/alpha"));

    const uploadFile = new File(["upload while switching\n"], "notes.txt", { type: "text/plain" });
    const uploadMock = installDelayedHttpUploadInitMock(
      daemon,
      alphaSession.session_id,
      "/home/alpha/notes.txt",
      uploadFile,
    );
    try {
      await user.upload(within(panel).getByLabelText("Upload file"), uploadFile);
      await screen.findByRole("status", { name: "Uploading notes.txt" });

      await clickSessionCard(user, "beta");
      await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
      // 中文注释：文件传输使用独立操作连接继续完成，但右侧文件面板只展示
      // 当前 attached session 的进度，避免旧 session 状态污染当前面板。
      expect(screen.queryByRole("status", { name: "Uploading notes.txt" })).not.toBeInTheDocument();

      await clickSessionCard(user, "alpha");
      expect(await screen.findByRole("status", { name: "Uploading notes.txt" })).toBeInTheDocument();

      uploadMock.releaseInit();
      await waitFor(() => {
        const uploaded = uploadMock.uploads.find((write) => write.path === "/home/alpha/notes.txt");
        expect(uploaded?.session_id).toBe(alphaSession.session_id);
        expect(Array.from(uploaded?.bytes ?? [])).toEqual(Array.from(new TextEncoder().encode("upload while switching\n")));
      });
    } finally {
      uploadMock.releaseInit();
      uploadMock.restore();
    }
  });

  it("下载进度在切换 session 后仍保留", async () => {
    const user = userEvent.setup();
    const alphaPath = "/home/alpha/alpha.txt";
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000433",
      name: "alpha",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000434",
      name: "beta",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      sessionFiles: {
        [alphaSession.session_id]: {
          session_id: alphaSession.session_id,
          path: "/home/alpha",
          entries: [
            {
              name: "alpha.txt",
              path: alphaPath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        [betaSession.session_id]: {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [],
        },
      },
      sessionFileReads: {
        [alphaPath]: {
          session_id: alphaSession.session_id,
          path: alphaPath,
          data_base64: Buffer.from("alpha\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    let resolveClose: (() => void) | undefined;
    const write = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const close = vi.fn<() => Promise<void>>(() => new Promise((resolve) => {
      resolveClose = resolve;
    }));
    const abort = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close, abort }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(
      daemon,
      alphaSession.session_id,
      alphaPath,
      "alpha.txt",
      new TextEncoder().encode("alpha\n"),
    );
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession("alpha");
      await clickSessionCard(user, "alpha");

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
      await screen.findByRole("status", { name: "Downloading alpha.txt" });
      await waitFor(() => expect(close).toHaveBeenCalledTimes(1));

      await clickSessionCard(user, "beta");
      await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
      // 中文注释：下载 close 尚未提交时，下载连接继续完成；但当前 beta
      // 文件面板不展示 alpha 的进度，避免跨 session UI 漂移。
      expect(screen.queryByRole("status", { name: "Downloading alpha.txt" })).not.toBeInTheDocument();

      await clickSessionCard(user, "alpha");
      expect(await screen.findByRole("status", { name: "Downloading alpha.txt" })).toBeInTheDocument();

      resolveClose?.();
      await waitFor(() => {
        expect(screen.getByRole("status", { name: "Downloading alpha.txt" })
          .querySelector<HTMLElement>(".files-transfer-bar-fill")
          ?.style.getPropertyValue("--files-transfer-progress")).toBe("100%");
      });
      expect(abort).not.toHaveBeenCalled();
    } finally {
      resolveClose?.();
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("旧 session 上传失败后不污染当前文件 panel", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000435",
      name: "alpha",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000436",
      name: "beta",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      sessionFiles: {
        [alphaSession.session_id]: {
          session_id: alphaSession.session_id,
          path: "/home/alpha",
          entries: [],
        },
        [betaSession.session_id]: {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [
            {
              name: "beta.txt",
              path: "/home/beta/beta.txt",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
        "/home/beta": {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [
            {
              name: "beta.txt",
              path: "/home/beta/beta.txt",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    const uploadMock = installDelayedHttpUploadInitFailure();
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession("alpha");
      await clickSessionCard(user, "alpha");

      const panel = await screen.findByLabelText("session files");
      await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/alpha"));
      await user.upload(
        within(panel).getByLabelText("Upload file"),
        new File(["broken upload\n"], "broken.txt", { type: "text/plain" }),
      );
      await screen.findByRole("status", { name: "Uploading broken.txt" });

      await clickSessionCard(user, "beta");
      await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
      await within(panel).findByText("beta.txt");

      uploadMock.failInit();
      await waitFor(() => expect(screen.queryByRole("status", { name: "Uploading broken.txt" })).toBeNull(), {
        timeout: 3500,
      });
      await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/beta"));
      // 中文注释：alpha 上传失败发生在切到 beta 之后，只能收敛 alpha 的传输；
      // 当前 beta 文件面板不能被旧错误改成 unavailable。
      expect(within(panel).queryByText("unavailable")).toBeNull();
      expect(within(panel).getByText("beta.txt")).toBeInTheDocument();
    } finally {
      uploadMock.failInit();
      uploadMock.restore();
    }
  });

  it("旧 session 下载失败后不污染当前文件 panel", async () => {
    const user = userEvent.setup();
    const alphaPath = "/home/alpha/alpha.txt";
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000437",
      name: "alpha",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000438",
      name: "beta",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      sessionFiles: {
        [alphaSession.session_id]: {
          session_id: alphaSession.session_id,
          path: "/home/alpha",
          entries: [
            {
              name: "alpha.txt",
              path: alphaPath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        [betaSession.session_id]: {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [
            {
              name: "beta.txt",
              path: "/home/beta/beta.txt",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
        "/home/beta": {
          session_id: betaSession.session_id,
          path: "/home/beta",
          entries: [
            {
              name: "beta.txt",
              path: "/home/beta/beta.txt",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    let rejectClose: ((error: Error) => void) | undefined;
    const write = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const close = vi.fn<() => Promise<void>>(() => new Promise((_, reject) => {
      rejectClose = reject;
    }));
    const abort = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close, abort }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(
      daemon,
      alphaSession.session_id,
      alphaPath,
      "alpha.txt",
      new TextEncoder().encode("alpha\n"),
    );
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession("alpha");
      await clickSessionCard(user, "alpha");

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
      await screen.findByRole("status", { name: "Downloading alpha.txt" });
      await waitFor(() => expect(close).toHaveBeenCalledTimes(1));

      await clickSessionCard(user, "beta");
      await waitFor(() => expect(daemon.attachedSessions).toContain(betaSession.session_id));
      await within(panel).findByText("beta.txt");

      rejectClose?.(new Error("download commit failed"));
      await waitFor(() => expect(screen.queryByRole("status", { name: "Downloading alpha.txt" })).toBeNull(), {
        timeout: 3500,
      });
      await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/beta"));
      // 中文注释：alpha 下载提交失败不能把 beta 文件面板覆盖成错误态。
      expect(within(panel).queryByText("unavailable")).toBeNull();
      expect(within(panel).getByText("beta.txt")).toBeInTheDocument();
    } finally {
      rejectClose?.(new Error("test cleanup"));
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("流式保存下载失败时 abort writer 而不是 close 半截文件", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000416";
    const rootPath = "/home/me/project";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: rootPath,
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    const write = vi.fn<() => Promise<void>>(() => Promise.reject(new Error("disk full")));
    const close = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const abort = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close, abort }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(daemon, sessionId, filePath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      await waitFor(() => expect(abort).toHaveBeenCalledTimes(1));
      expect(close).not.toHaveBeenCalled();
      expect(write).toHaveBeenCalled();
    } finally {
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("流式保存下载失败且 writer 没有 abort 时也不会 close 半截文件", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000417";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    const write = vi.fn<() => Promise<void>>(() => Promise.reject(new Error("disk full")));
    const close = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(daemon, sessionId, filePath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      await waitFor(() => expect(write).toHaveBeenCalled());
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(close).not.toHaveBeenCalled();
    } finally {
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("选择保存位置后 createWritable 非取消失败时不会回退到内存下载", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000420";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    const write = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const close = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click");
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.reject(new Error("writer setup failed")),
        }),
      ),
    });
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      await new Promise((resolve) => setTimeout(resolve, 50));
      expect(clickSpy).not.toHaveBeenCalled();
      expect(write).not.toHaveBeenCalled();
      expect(close).not.toHaveBeenCalled();
    } finally {
      clickSpy.mockRestore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("流式保存 close 失败时 abort writer 并保留 close 错误", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000418";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    const write = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const close = vi.fn<() => Promise<void>>(() => Promise.reject(new Error("commit failed")));
    const abort = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close, abort }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(daemon, sessionId, filePath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      await waitFor(() => expect(close).toHaveBeenCalledTimes(1));
      await waitFor(() => expect(abort).toHaveBeenCalledTimes(1));
      expect(write).toHaveBeenCalled();
    } finally {
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("下载进度在 writer.close 成功后才显示完成", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000419";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    let resolveClose: (() => void) | undefined;
    const write = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const close = vi.fn<() => Promise<void>>(() => new Promise((resolve) => {
      resolveClose = resolve;
    }));
    const abort = vi.fn<() => Promise<void>>(() => Promise.resolve());
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => Promise.resolve({ write, close, abort }),
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(daemon, sessionId, filePath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      const progress = await within(panel).findByRole("status", { name: "Downloading alpha.txt" });
      await waitFor(() => expect(close).toHaveBeenCalledTimes(1));
      const fill = progress.querySelector<HTMLElement>(".files-transfer-bar-fill");
      expect(fill?.style.getPropertyValue("--files-transfer-progress")).toBe("0%");

      resolveClose?.();
      await waitFor(() => {
        const currentProgress = within(panel).getByRole("status", { name: "Downloading alpha.txt" });
        expect(currentProgress.querySelector<HTMLElement>(".files-transfer-bar-fill")?.style.getPropertyValue("--files-transfer-progress")).toBe("100%");
      });
      expect(abort).not.toHaveBeenCalled();
    } finally {
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("旧下载完成后的延迟清理不会覆盖新的下载进度", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000421";
    const alphaPath = "/home/me/project/alpha.txt";
    const betaPath = "/home/me/project/beta.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: alphaPath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
            {
              name: "beta.txt",
              path: betaPath,
              kind: "file",
              size_bytes: 5,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [alphaPath]: {
          session_id: sessionId,
          path: alphaPath,
          data_base64: Buffer.from("alpha\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
        [betaPath]: {
          session_id: sessionId,
          path: betaPath,
          data_base64: Buffer.from("beta\n", "utf8").toString("base64"),
          size_bytes: 5,
          modified_at_ms: null,
        },
      },
    });
    let resolveFirstClose: (() => void) | undefined;
    let resolveSecondClose: (() => void) | undefined;
    const writers: Array<{
      write: ReturnType<typeof vi.fn>;
      close: ReturnType<typeof vi.fn>;
      abort: ReturnType<typeof vi.fn>;
    }> = [];
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    Object.defineProperty(globalThis, "showSaveFilePicker", {
      configurable: true,
      value: vi.fn(() =>
        Promise.resolve({
          createWritable: () => {
            const index = writers.length;
            const writer = {
              write: vi.fn<() => Promise<void>>(() => Promise.resolve()),
              close: vi.fn<() => Promise<void>>(
                () =>
                  new Promise((resolve) => {
                    if (index === 0) {
                      resolveFirstClose = resolve;
                    } else {
                      resolveSecondClose = resolve;
                    }
                  }),
              ),
              abort: vi.fn<() => Promise<void>>(() => Promise.resolve()),
            };
            writers.push(writer);
            return Promise.resolve(writer);
          },
        }),
      ),
    });
    const downloadMock = installHttpDownloadMock(daemon, sessionId, alphaPath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await within(panel).findByText("beta.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
      await waitFor(() => expect(writers[0]?.close).toHaveBeenCalledTimes(1));

      await user.click(within(panel).getByRole("button", { name: "Download beta.txt" }));
      await waitFor(() => expect(writers[1]?.close).toHaveBeenCalledTimes(1));
      await within(panel).findByRole("status", { name: "Downloading beta.txt" });

      // 中文注释：alpha 的 close 此时才完成，旧 transfer 的 finally 会尝试延迟清理；
      // 它不能清掉当前 beta transfer 的进度条。
      resolveFirstClose?.();
      await new Promise((resolve) => setTimeout(resolve, 1300));

      expect(within(panel).queryByRole("status", { name: "Downloading alpha.txt" })).toBeNull();
      expect(within(panel).getByRole("status", { name: "Downloading beta.txt" })).toBeTruthy();
      resolveSecondClose?.();
    } finally {
      downloadMock.restore();
      if (originalPicker === undefined) {
        delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
      } else {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
    }
  });

  it("没有 File System Access 时下载内存 fallback 也显示进度", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000420";
    const filePath = "/home/me/project/alpha.txt";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "alpha.txt",
              path: filePath,
              kind: "file",
              size_bytes: 6,
              modified_at_ms: null,
            },
          ],
        },
      },
      sessionFileReads: {
        [filePath]: {
          session_id: sessionId,
          path: filePath,
          data_base64: Buffer.from("hello\n", "utf8").toString("base64"),
          size_bytes: 6,
          modified_at_ms: null,
        },
      },
    });
    const originalPicker = (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    delete (globalThis as { showSaveFilePicker?: unknown }).showSaveFilePicker;
    const downloadMock = installHttpDownloadMock(daemon, sessionId, filePath, "alpha.txt", new TextEncoder().encode("hello\n"));
    try {
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      const panel = await screen.findByLabelText("session files");
      await within(panel).findByText("alpha.txt");
      await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));

      await within(panel).findByRole("status", { name: "Downloading alpha.txt" });
      await waitFor(
        () => {
          const currentProgress = within(panel).getByRole("status", { name: "Downloading alpha.txt" });
          expect(currentProgress.querySelector<HTMLElement>(".files-transfer-bar-fill")?.style.getPropertyValue("--files-transfer-progress")).toBe("100%");
        },
        { timeout: 6000 },
      );
    } finally {
      if (originalPicker !== undefined) {
        Object.defineProperty(globalThis, "showSaveFilePicker", {
          configurable: true,
          value: originalPicker,
        });
      }
      downloadMock.restore();
    }
  });

  it("文件 panel 可以切到 Git tab 查看未提交文件和提交图", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000415";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [],
        },
      },
      sessionGit: {
        [sessionId]: {
          session_id: sessionId,
          cwd: "/home/me/project",
          repository_root: "/home/me/project",
          worktrees: [
            {
              path: "/home/me/project",
              branch: "main",
              head: "a1b2c3d",
              is_current: true,
              staged: [{ path: "src/lib.rs", status: "M " }],
              unstaged: [{ path: "README.md", status: " M" }],
            },
            {
              path: "/home/me/project-feature",
              branch: "feature/files",
              head: "d4e5f6a",
              is_current: false,
              staged: [{ path: "src/git-panel.tsx", status: "A " }],
              unstaged: [],
            },
          ],
          graph: ["* a1b2c3d main commit", "| * d4e5f6a feature commit", "|/"],
          error: null,
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await user.click(within(panel).getByRole("tab", { name: "Git" }));

    await within(panel).findByText("main");
    await within(panel).findAllByText("Staged");
    await within(panel).findByText("src/lib.rs");
    await within(panel).findAllByText("Unstaged");
    await within(panel).findByText("README.md");
    await within(panel).findByText("feature/files");
    await within(panel).findByText("Changes");
    const gitStatusPane = within(panel).getByLabelText("Git status");
    expect(within(gitStatusPane).queryByText("Files")).toBeNull();
    const changesTree = within(gitStatusPane).getByRole("tree", { name: "Git changes tree" });
    expect(changesTree.classList.contains("git-status-body")).toBe(true);
    expect(panel.querySelector(".git-panel-compact")).not.toBeNull();
    const mainTreeItem = within(changesTree).getByRole("treeitem", { name: "main changes" });
    expect(mainTreeItem.querySelector(".git-worktree-floating-meta .git-worktree-head")?.textContent).toBe("a1b2c3d");
    const readmeTreeItem = within(changesTree).getByRole("treeitem", { name: "M README.md" });
    expect(readmeTreeItem.querySelector(".git-change-floating-actions button[aria-label='Stage README.md']")).not.toBeNull();
    expect(within(panel).getByRole("button", { name: "Discard README.md" }).querySelector(".lucide-undo2")).not.toBeNull();
    const graphResizer = within(panel).getByRole("separator", { name: "Resize Git graph" });
    fireEvent.keyDown(graphResizer, { key: "ArrowDown" });
    expect(panel.querySelector<HTMLElement>(".git-panel")?.style.getPropertyValue("--git-changes-pane-height")).toContain("px");
    await waitFor(() =>
      expect(panel.querySelector(".git-graph-commit")?.getAttribute("title")).toBe("a1b2c3d main commit"),
    );
    expect(panel.querySelector(".git-graph-row")?.textContent).not.toContain("* a1b2c3d main commit");
    expect(panel.querySelector(".git-graph-node")).not.toBeNull();

    await user.click(within(readmeTreeItem).getByRole("button", { name: "Diff README.md" }));
    await waitFor(() =>
      expect(daemon.sessionGitDiffRequests).toContainEqual({
        session_id: sessionId,
        worktree_path: "/home/me/project",
        file_path: "README.md",
        staged: false,
      }),
    );
    await screen.findByText(/mock unstaged diff for README\.md/);

    await user.click(within(panel).getByRole("button", { name: "Open README.md" }));
    await waitFor(() =>
      expect(daemon.sessionFileReadRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/README.md",
      }),
    );

    await user.click(within(panel).getByRole("button", { name: "Stage README.md" }));
    await user.click(within(panel).getByRole("button", { name: "Unstage src/lib.rs" }));
    await user.click(within(panel).getByRole("button", { name: "Discard README.md" }));
    await waitFor(() =>
      expect(daemon.sessionGitActions).toEqual(
        expect.arrayContaining([
          {
            session_id: sessionId,
            worktree_path: "/home/me/project",
            file_path: "README.md",
            action: "stage",
          },
          {
            session_id: sessionId,
            worktree_path: "/home/me/project",
            file_path: "src/lib.rs",
            action: "unstage",
          },
          {
            session_id: sessionId,
            worktree_path: "/home/me/project",
            file_path: "README.md",
            action: "discard",
          },
        ]),
      ),
    );

    const worktreeItem = within(panel).getByRole("treeitem", { name: "main changes" });
    expect(within(worktreeItem).queryByLabelText("Commit message")).toBeNull();
    expect(within(worktreeItem).queryByRole("button", { name: "Commit staged" })).toBeNull();
    expect(within(worktreeItem).queryByLabelText("Stash message")).toBeNull();
    expect(within(worktreeItem).queryByRole("button", { name: "Stash" })).toBeNull();

    await user.click(within(panel).getByRole("button", { name: "Collapse main worktree" }));
    expect(within(panel).queryByText("src/lib.rs")).toBeNull();
    await user.click(within(panel).getByRole("button", { name: "Expand main worktree" }));
    await within(panel).findByText("src/lib.rs");

    await user.click(within(panel).getByRole("button", { name: "Collapse Git changes" }));
    expect(within(panel).queryByText("README.md")).toBeNull();
    await user.click(within(panel).getByRole("button", { name: "Expand Git changes" }));
    await within(panel).findByText("README.md");

    await user.click(within(panel).getByRole("button", { name: "Collapse Git graph" }));
    expect(panel.querySelector(".git-graph-commit")).toBeNull();
  });

  it("旧 Git diff 迟到后不会覆盖当前 diff 弹窗", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000424";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [],
        },
      },
      sessionGit: {
        [sessionId]: {
          session_id: sessionId,
          cwd: "/home/me/project",
          repository_root: "/home/me/project",
          worktrees: [
            {
              path: "/home/me/project",
              branch: "main",
              head: "a1b2c3d",
              is_current: true,
              staged: [{ path: "src/lib.rs", status: "M " }],
              unstaged: [{ path: "README.md", status: " M" }],
            },
          ],
          graph: ["* a1b2c3d main commit"],
          error: null,
        },
      },
      sessionGitDiffDelayMsByPath: {
        "README.md": 120,
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await user.click(within(panel).getByRole("tab", { name: "Git" }));
    await within(panel).findByText("README.md");
    await within(panel).findByText("src/lib.rs");
    const changesTree = await within(panel).findByRole("tree", { name: "Git changes tree" });
    const readmeTreeItem = within(changesTree).getByRole("treeitem", { name: "M README.md" });
    const libTreeItem = within(changesTree).getByRole("treeitem", { name: "M src/lib.rs" });

    await user.click(within(readmeTreeItem).getByRole("button", { name: "Diff README.md" }));
    await screen.findByRole("dialog", { name: "README.md" });
    await waitFor(() =>
      expect(daemon.sessionGitDiffRequests).toContainEqual({
        session_id: sessionId,
        worktree_path: "/home/me/project",
        file_path: "README.md",
        staged: false,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Close editor" }));

    await user.click(within(libTreeItem).getByRole("button", { name: "Diff src/lib.rs" }));
    const libDiff = await screen.findByRole("dialog", { name: "lib.rs" });
    await waitFor(() =>
      expect((within(libDiff).getByLabelText("File text") as HTMLTextAreaElement).value).toContain("mock staged diff for src/lib.rs"),
    );
    await waitFor(() =>
      expect(daemon.sessionGitDiffRequests).toContainEqual({
        session_id: sessionId,
        worktree_path: "/home/me/project",
        file_path: "src/lib.rs",
        staged: true,
      }),
    );

    await new Promise((resolve) => window.setTimeout(resolve, 180));
    const currentDiff = screen.getByRole("dialog", { name: "lib.rs" });
    const currentDiffText = (within(currentDiff).getByLabelText("File text") as HTMLTextAreaElement).value;
    expect(currentDiffText).toContain("mock staged diff for src/lib.rs");
    expect(currentDiffText).not.toContain("mock unstaged diff for README.md");
  });

  it("文件 panel 跟随 metadata 推送的终端 cwd，并可关闭跟随", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000414";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "beta.log",
              path: "/tmp/work/beta.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    const requestCountBeforeInitialAttach = daemon.sessionFileRequests.length;
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeInitialAttach)).toContainEqual({
        session_id: sessionId,
      }),
    );
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));
    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    expect(followToggle).toBeChecked();

    const requestCountBeforeCwdChange = daemon.sessionFileRequests.length;
    daemon.pushSessionCwdChanged(sessionId, "/tmp/work");
    await waitFor(
      () => {
        expect(daemon.sessionFileRequests.slice(requestCountBeforeCwdChange)).toContainEqual({
          session_id: sessionId,
        });
      },
      { timeout: 1500 },
    );
    await within(panel).findByText("beta.log");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work"));

    await user.click(followToggle);
    expect(followToggle).not.toBeChecked();
    const requestCountAfterDisable = daemon.sessionFileRequests.length;
    daemon.pushSessionCwdChanged(sessionId, "/home/me");
    await new Promise((resolve) => window.setTimeout(resolve, 200));
    // 关闭 Follow 后，metadata cwd 更新不能覆盖用户停留的目录。
    expect(
      daemon.sessionFileRequests
        .slice(requestCountAfterDisable)
        .filter((request) => request.path === undefined || request.path === null),
    ).toHaveLength(0);
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work");
  });

  it("文件 panel 在跟随模式下手动切目录后会退出跟随，避免被轮询打回", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000416";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [
            {
              name: "project",
              path: "/home/me/project",
              kind: "directory",
              size_bytes: 0,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "src",
              path: "/home/me/project/src",
              kind: "directory",
              size_bytes: 0,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "beta.log",
              path: "/tmp/work/beta.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    expect(followToggle).toBeChecked();
    await within(panel).findByText("project");

    daemon.setSessionFilePosition(sessionId, "/home/me/project");
    await user.click(within(panel).getByRole("button", { name: "Open project" }));
    await within(panel).findByText("src");
    expect(followToggle).not.toBeChecked();
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me/project"));

    const requestCountBeforeFollowTick = daemon.sessionFileRequests.length;
    daemon.setSessionFilePosition(sessionId, "/tmp/work");
    await new Promise((resolve) => window.setTimeout(resolve, 1200));
    expect(daemon.sessionFileRequests.slice(requestCountBeforeFollowTick)).not.toContainEqual({
      session_id: sessionId,
    });
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me/project");
    expect(within(panel).queryByText("beta.log")).toBeNull();
  });

  it("文件 panel 关闭跟随后忽略 daemon 后台 cwd 轻事件，仍可手动切目录", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000417";
    const cwdPushFiles = {
      session_id: sessionId,
      path: "/tmp/work",
      entries: [
        {
          name: "beta.log",
          path: "/tmp/work/beta.log",
          kind: "file",
          size_bytes: 4,
          modified_at_ms: null,
        },
      ],
    } satisfies SessionFilesResultPayload;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [
            {
              name: "project",
              path: "/home/me/project",
              kind: "directory",
              size_bytes: 0,
              modified_at_ms: null,
            },
          ],
        },
        "/home/me/project": {
          session_id: sessionId,
          path: "/home/me/project",
          entries: [
            {
              name: "src",
              path: "/home/me/project/src",
              kind: "directory",
              size_bytes: 0,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp/work": cwdPushFiles,
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await within(panel).findByText("project");
    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    await user.click(followToggle);
    expect(followToggle).not.toBeChecked();

    daemon.pushSessionCwdChanged(sessionId, "/tmp/work");
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me");
    expect(within(panel).queryByText("beta.log")).toBeNull();

    await user.click(within(panel).getByRole("button", { name: "Open project" }));
    await within(panel).findByText("src");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me/project"));

    daemon.pushSessionCwdChanged(sessionId, "/tmp/work");
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me/project");
    expect(within(panel).queryByText("beta.log")).toBeNull();
  });

  it("关闭 Follow 后忽略已在路上的静默 initial cwd 刷新", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000418";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [
            {
              name: "home.log",
              path: "/home/me/home.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "late.log",
              path: "/tmp/work/late.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await within(panel).findByText("home.log");
    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    expect(followToggle).toBeChecked();

    daemon.queueSessionFilesResponse(
      sessionId,
      {
        session_id: sessionId,
        path: "/tmp/work",
        entries: [
          {
            name: "late.log",
            path: "/tmp/work/late.log",
            kind: "file",
            size_bytes: 4,
            modified_at_ms: null,
          },
        ],
      },
      { path: undefined, delayMs: 80 },
    );
    const requestCountBeforeReconnectRefresh = daemon.sessionFileRequests.length;
    daemon.dropConnections();
    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeReconnectRefresh)).toContainEqual({
        session_id: sessionId,
      }),
      { timeout: 2200 },
    );

    await user.click(followToggle);
    expect(followToggle).not.toBeChecked();
    await new Promise((resolve) => window.setTimeout(resolve, 140));

    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me");
    expect(within(panel).queryByText("late.log")).toBeNull();
    expect(within(panel).getByText("home.log")).toBeVisible();
  });

  it("关闭 Follow 后重新 attach session 时保留当前文件树目录", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000412";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "beta.log",
              path: "/tmp/work/beta.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    const requestCountBeforeInitialAttach = daemon.sessionFileRequests.length;
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeInitialAttach)).toContainEqual({
        session_id: sessionId,
      }),
    );
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp/work");
    const requestCountBeforeManualGo = daemon.sessionFileRequests.length;
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeManualGo)).toContainEqual({
        session_id: sessionId,
        path: "/tmp/work",
      }),
    );
    await within(panel).findByText("beta.log");

    await user.click(screen.getByRole("button", { name: "Daemons" }));
    await screen.findByLabelText("daemon admin");
    await user.click(screen.getByRole("button", { name: "Open workspace" }));
    await waitForWorkspaceSession();

    const requestCountBeforeReattach = daemon.sessionFileRequests.length;
    await clickSessionCard(user);

    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeReattach)).toContainEqual({
        session_id: sessionId,
        path: "/tmp/work",
      }),
    );
    expect(daemon.sessionFileRequests.slice(requestCountBeforeReattach)).not.toContainEqual({
      session_id: sessionId,
    });
    const currentPanel = await screen.findByLabelText("session files");
    await within(currentPanel).findByText("beta.log");
  });

  it("接收 session_cwd_changed 后主动重拉并同步当前 session 的文件树位置", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000413";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/work": {
          session_id: sessionId,
          path: "/tmp/work",
          entries: [
            {
              name: "beta.log",
              path: "/tmp/work/beta.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    const requestCountBeforeFollow = daemon.sessionFileRequests.length;
    const startedAt = Date.now();
    daemon.pushSessionCwdChanged(sessionId, "/tmp/work");

    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeFollow)).toContainEqual({
        session_id: sessionId,
      }),
      { timeout: 800 },
    );
    expect(Date.now() - startedAt).toBeLessThan(900);
    expect(daemon.sessionFileRequests.slice(requestCountBeforeFollow).length).toBe(1);
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work"));
    await within(panel).findByText("beta.log");
  });

  it("session_cwd_changed 的旧静默刷新不会覆盖用户刚手动切到的新目录", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000414";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/follow": {
          session_id: sessionId,
          path: "/tmp/follow",
          entries: [
            {
              name: "old.log",
              path: "/tmp/follow/old.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp/manual": {
          session_id: sessionId,
          path: "/tmp/manual",
          entries: [
            {
              name: "new.log",
              path: "/tmp/manual/new.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    daemon.queueSessionFilesResponse(
      sessionId,
      {
        session_id: sessionId,
        path: "/tmp/follow",
        entries: [
          {
            name: "old.log",
            path: "/tmp/follow/old.log",
            kind: "file",
            size_bytes: 4,
            modified_at_ms: null,
          },
        ],
      },
      { path: undefined, delayMs: 60 },
    );
    daemon.queueSessionFilesResponse(
      sessionId,
      {
        session_id: sessionId,
        path: "/tmp/manual",
        entries: [
          {
            name: "new.log",
            path: "/tmp/manual/new.log",
            kind: "file",
            size_bytes: 4,
            modified_at_ms: null,
          },
        ],
      },
      { path: "/tmp/manual", delayMs: 0 },
    );
    const requestCountBeforeFollow = daemon.sessionFileRequests.length;
    daemon.pushSessionCwdChanged(sessionId, "/tmp/follow");

    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeFollow)).toContainEqual({
        session_id: sessionId,
      }),
    );

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp/manual");
    await user.click(within(panel).getByRole("button", { name: "Go" }));

    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/manual"));
    await within(panel).findByText("new.log");
    expect(within(panel).queryByText("old.log")).toBeNull();
  });

  it("文件树可见刷新进行中收到 cwd 轻事件时，会在请求结束后补拉最新目录", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000414";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/manual": {
          session_id: sessionId,
          path: "/tmp/manual",
          entries: [
            {
              name: "manual.log",
              path: "/tmp/manual/manual.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
        "/tmp/follow-after-manual": {
          session_id: sessionId,
          path: "/tmp/follow-after-manual",
          entries: [
            {
              name: "follow.log",
              path: "/tmp/follow-after-manual/follow.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    daemon.queueSessionFilesResponse(
      sessionId,
      {
        session_id: sessionId,
        path: "/home/me",
        entries: [
          {
            name: "project",
            path: "/home/me/project",
            kind: "file",
            size_bytes: 4,
            modified_at_ms: null,
          },
        ],
      },
      { path: undefined, delayMs: 60 },
    );

    const requestCountBeforeRefresh = daemon.sessionFileRequests.length;
    await user.click(within(panel).getByRole("button", { name: "Refresh files" }));

    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeRefresh)).toContainEqual({
        session_id: sessionId,
      }),
    );

    daemon.pushSessionCwdChanged(sessionId, "/tmp/follow-after-manual");

    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/follow-after-manual"));
    await within(panel).findByText("follow.log");
    expect(within(panel).queryByText("project")).toBeNull();
  });

  it("旧 daemon 晚到的被动 session_files_result 不会覆盖用户刚手动切到的新目录", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000415";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/home/me",
          entries: [],
        },
        "/tmp/manual": {
          session_id: sessionId,
          path: "/tmp/manual",
          entries: [
            {
              name: "new.log",
              path: "/tmp/manual/new.log",
              kind: "file",
              size_bytes: 4,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp/manual");
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/manual"));
    await within(panel).findByText("new.log");

    daemon.pushSessionFiles({
      session_id: sessionId,
      path: "/tmp/stale-follow",
      entries: [
        {
          name: "stale.log",
          path: "/tmp/stale-follow/stale.log",
          kind: "file",
          size_bytes: 5,
          modified_at_ms: null,
        },
      ],
    });

    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/manual");
    expect(within(panel).queryByText("stale.log")).toBeNull();
  });

  it("Clients 按钮打开仅在线客户端面板并显示其正在查看的会话", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000410",
          name: "Build agent",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
        {
          session_id: "00000000-0000-0000-0000-000000000411",
          name: "Review agent",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      daemonClients: [
        {
          client_id: "00000000-0000-0000-0000-000000000701",
          device_id: "00000000-0000-0000-0000-000000000801",
          peer_ip: "192.0.2.41",
          online: true,
          connected_at_ms: 1_710_000_000_000,
          last_seen_at_ms: 1_710_000_000_000,
          attached_session_ids: [
            "00000000-0000-0000-0000-000000000410",
            "00000000-0000-0000-0000-000000000411",
          ],
          cursor_session_id: "00000000-0000-0000-0000-000000000410",
          cursor_row: 12,
          cursor_col: 8,
          cursor_focused: true,
        },
        {
          client_id: "00000000-0000-0000-0000-000000000702",
          device_id: "00000000-0000-0000-0000-000000000802",
          peer_ip: "198.51.100.9",
          online: false,
          connected_at_ms: 1_710_000_000_100,
          last_seen_at_ms: 1_710_000_030_000,
          attached_session_ids: [],
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const operators = await screen.findByLabelText("session operators");
    await within(operators).findByText("192.0.2.41");
    expect(within(operators).queryByText("12:8")).toBeNull();
    expect(within(operators).queryByText("cursor ?")).toBeNull();
    expect(within(operators).queryByText("focused")).toBeNull();
    expect(within(operators).queryByText("blurred")).toBeNull();
    expect(within(operators).queryByText(/selecting/)).toBeNull();

    expect(screen.queryByLabelText("daemon clients")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Clients" }));

    const clientPanel = await screen.findByLabelText("daemon clients");
    await within(clientPanel).findByText("Clients");
    await within(clientPanel).findByText("192.0.2.41");
    await within(clientPanel).findByText("online");
    await within(clientPanel).findByText("Build agent");
    await within(clientPanel).findByText("Review agent");
    expect(within(clientPanel).queryByText("198.51.100.9")).toBeNull();
    expect(within(clientPanel).queryByText("offline")).toBeNull();
  });

  it("Session 卡片点击即打开，标题行保留管理按钮", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const actions = await screen.findByLabelText("Session actions");

    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    expect(actions).toContainElement(screen.getByRole("button", { name: "Rename session" }));
    expect(actions).toContainElement(screen.getByRole("button", { name: "Close session" }));
  });

  it("Session 行名称在会话菜单里左对齐", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const openButton = screen.getByRole("button", { name: `Open ${DEFAULT_SESSION_NAME}` });
    const openButtonRule = cssRuleBody(css, ".session-open-button");
    const openButtonNameRule = cssRuleBody(css, ".session-open-button strong");

    expect(openButton).toHaveClass("session-open-button");
    expect(openButtonRule).toMatch(/display:\s*grid;/);
    expect(openButtonRule).toMatch(/justify-content:\s*stretch;/);
    expect(openButtonRule).toMatch(/justify-items:\s*start;/);
    expect(openButtonRule).toMatch(/width:\s*100%;/);
    expect(openButtonRule).toMatch(/text-align:\s*left;/);
    expect(openButtonNameRule).toMatch(/text-align:\s*left;/);
  });

  it("桌面侧栏固定标题和新建按钮，只让 session 列表滚动", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const sidebar = document.querySelector<HTMLElement>(".sidebar");
    expect(sidebar).not.toBeNull();
    const newSession = screen.getByRole("button", { name: "New session" });
    const sessionList = screen.getByRole("region", { name: "sessions" });
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    // 中文注释：侧栏顶端品牌和新建按钮是固定头部；不能再把新建按钮包进 panel，
    // session 很多时也只滚动 session-list，避免 title 跟着列表一起滚走。
    expect(newSession.closest(".panel")).toBeNull();
    expect(newSession.closest(".sidebar-fixed-header")).toBe(sidebar!.querySelector(".sidebar-fixed-header"));
    expect(sessionList.closest(".sidebar-scroll-region")).toBe(sidebar!.querySelector(".sidebar-scroll-region"));
    expect(css).toContain(".sidebar {\n  ");
    expect(css).toContain("overflow: hidden;");
    expect(css).toContain(".sidebar-fixed-header {\n  min-width: 0;\n  display: grid;\n  gap: 12px;\n}");
    expect(css).toContain(".sidebar-scroll-region {\n  min-height: 0;\n  overflow: hidden;\n  display: grid;\n}");
    expect(css).toContain(".session-list {\n  min-height: 0;\n  height: 100%;\n  overflow-y: auto;");
    expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Disconnect" })).toBeNull();
  });

  it("左侧栏可折叠成图标栏，右侧文件 panel 可隐藏后再展开", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    expect(await screen.findByLabelText("session files")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Collapse sidebar" }));

    expect(screen.getByRole("button", { name: "Expand sidebar" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New session" })).toBeInTheDocument();
    expect(screen.queryByText("New session")).toBeNull();
    expect(screen.queryByLabelText("connection status")).toBeNull();
    expect(screen.getByLabelText("collapsed sessions")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Expand sidebar" }));
    expect(screen.getByRole("button", { name: "Collapse sidebar" })).toBeInTheDocument();
    await screen.findByText("New session");

    await user.click(screen.getByRole("button", { name: "Hide files panel" }));
    await waitFor(() => expect(screen.queryByLabelText("session files")).toBeNull());
    expect(screen.getByRole("button", { name: "Show files panel" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Show files panel" }));
    expect(await screen.findByLabelText("session files")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Hide files panel" })).toBeInTheDocument();
  });

  it("桌面文件 panel 可以通过拖拽分隔条调整宽度", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    const workspaceBody = document.querySelector<HTMLElement>(".workspace-body");
    expect(workspaceBody).not.toBeNull();
    const initialColumns = workspaceBody!.style.gridTemplateColumns;
    const resizer = screen.getByRole("separator", { name: "Resize files panel" });
    expect(document.querySelector(".files-resizer")).toBeNull();
    expect(resizer.classList.contains("files-panel-edge-resizer")).toBe(true);

    fireEvent.pointerDown(resizer, { clientX: 1180, pointerId: 1 });
    fireEvent.pointerMove(window, { clientX: 1080, pointerId: 1 });
    fireEvent.pointerUp(window, { pointerId: 1 });

    await waitFor(() => expect(workspaceBody!.style.gridTemplateColumns).not.toBe(initialColumns));
    expect(workspaceBody!.style.gridTemplateColumns).toContain("px");
  });

  it("粘贴 QR payload 后会使用当前连接地址和 token", async () => {
    const user = userEvent.setup();
    render(<App />);
    await setConnectionUrl(user, daemon.url);

    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      token: "secret-token",
      server_id: daemon.serverId,
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;

    fireEvent.change(screen.getByLabelText("Pairing token"), { target: { value: inviteCode } });
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    expect(screen.queryByLabelText("WS URL")).toBeNull();
    await waitForWorkspaceSession();
    await expectDaemonUrlInAdmin(user, daemon.url);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("粘贴 relay base URL 邀请码时使用统一 /ws URL", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      relayClientPathOnly: true,
    });
    render(<App />);

    const relayUrl = daemon.url;
    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      ws_url: daemon.url,
      token: "secret-token",
      server_id: daemon.serverId,
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;

    fireEvent.change(screen.getByLabelText("Pairing token"), { target: { value: inviteCode } });
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitForWorkspaceSession("No session");
    await expectDaemonUrlInAdmin(user, relayUrl);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("direct Web 路径串联 supervisor-backed session 的 create/list/attach/input/resize/reconnect", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000501";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "direct-supervisor-ready\n",
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/tmp/direct-supervisor-cwd",
          entries: [
            {
              name: "direct.txt",
              path: "/tmp/direct-supervisor-cwd/direct.txt",
              kind: "file",
              size_bytes: 12,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await exerciseSupervisorBackedWebLifecycle(user, daemon, {
      sessionId,
      readyText: "direct-supervisor-ready",
      cwd: "/tmp/direct-supervisor-cwd",
      fileName: "direct.txt",
      inputText: "echo direct-supervisor-secret",
      postReconnectText: "direct-supervisor-after-reconnect\n",
    });
  });

  it("relay /ws 路径串联 supervisor-backed session 的 create/list/attach/input/resize/reconnect", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000501";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "relay-supervisor-ready\n",
      relayClientPathOnly: true,
      sessionFiles: {
        [sessionId]: {
          session_id: sessionId,
          path: "/tmp/relay-supervisor-cwd",
          entries: [
            {
              name: "relay.txt",
              path: "/tmp/relay-supervisor-cwd/relay.txt",
              kind: "file",
              size_bytes: 11,
              modified_at_ms: null,
            },
          ],
        },
      },
    });
    render(<App />);

    await exerciseSupervisorBackedWebLifecycle(user, daemon, {
      sessionId,
      readyText: "relay-supervisor-ready",
      cwd: "/tmp/relay-supervisor-cwd",
      fileName: "relay.txt",
      inputText: "echo relay-supervisor-secret",
      postReconnectText: "relay-supervisor-after-reconnect\n",
    });
    expect(daemon.outerWireText()).not.toContain("relay-supervisor-secret");
  });

  it("pairing 失败后清空 token，错误 UI 和 outer wire 都不泄漏敏感字段", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      pairFailure: {
        code: "token_private_signature_ciphertext",
        message:
          "wrong-token server_private_key=private-value signature=sig ciphertext_base64=abc terminal-secret",
      },
      sessions: [],
    });
    render(<App />);

    await setConnectionUrl(user, daemon.url);
    fireEvent.change(screen.getByLabelText("Pairing token"), {
      target: { value: pairingInviteCode(daemon, "wrong-token") },
    });
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.getByLabelText("Pairing token")).toHaveValue(""));
    await screen.findByText("protocol_error: protocol operation failed");
    const renderedText = document.body.textContent ?? "";

    for (const sensitive of [
      "wrong-token",
      "secret-token",
      "server_private_key",
      "private-value",
      "signature",
      "ciphertext_base64",
      "terminal-secret",
    ]) {
      expect(renderedText).not.toContain(sensitive);
    }

    // 中文注释：trusted relay 的 route_hello 会携带 pair_ticket admission，所以 pairing token
    // 可能出现在外层路由前置帧；这里仍要确保 daemon 错误、私钥和终端内容不会泄漏。
    for (const sensitive of [
      "secret-token",
      "server_private_key",
      "private-value",
      "signature=sig",
      "terminal-secret",
    ]) {
      expect(daemon.outerWireText()).not.toContain(sensitive);
    }
  });

  it("shared-control 模式不显示 Take control 或 viewer/controller 状态", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    expect(screen.queryByRole("button", { name: "Take control" })).toBeNull();
    expect(document.body.textContent).not.toContain("viewer");
    expect(document.body.textContent).not.toContain("controller");
  });

  it("可以在 Session 列表重命名和关闭 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    await user.click(await screen.findByRole("button", { name: "Rename session" }));
    expect(screen.getByRole("button", { name: "Save session name" })).toBeDisabled();
    expect(daemon.sessionRenames).toEqual([]);
    daemon.queueSessionListResponse([], 30);
    await waitFor(() => expect(screen.getByLabelText("Session name")).toHaveValue(DEFAULT_SESSION_NAME));
    await user.clear(screen.getByLabelText("Session name"));
    await user.type(screen.getByLabelText("Session name"), "work shell");
    await user.click(screen.getByRole("button", { name: "Save session name" }));

    await waitFor(() => expect(screen.queryAllByText("work shell").length).toBeGreaterThan(0));
    expect(daemon.sessionRenames).toEqual([
      {
        session_id: "00000000-0000-0000-0000-000000000401",
        name: "work shell",
      },
    ]);

    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => {
      expect(screen.queryByText("work shell")).toBeNull();
    });
    expect(daemon.closedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]);
  });

  it("关闭当前 session 会立即移除界面且只发一次 close、不重新 attach", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [{
        session_id: DEFAULT_SESSION_ID,
        name: DEFAULT_SESSION_NAME,
        state: "running",
        size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
      }],
      attachOutput: "termd-e2e-ready\n",
      sessionCloseDelayMs: 150,
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));
    expect(daemon.v070TerminalBinaryFramesSent).toBe(1);
    expect(daemon.v070MetadataConnections).toBe(1);
    expect(daemon.v070TerminalConnections).toBe(1);
    await screen.findByText(/termd-e2e-ready/);
    const httpRequestIndex = daemon.receivedHttpRequests.length;
    const attachCount = daemon.attachRequests.length;

    await user.click(screen.getByRole("button", { name: "Close session" }));

    expect(screen.queryByText(DEFAULT_SESSION_NAME)).toBeNull();
    expect(daemon.attachRequests).toHaveLength(attachCount);
    await waitFor(() => expect(daemon.closedSessions).toEqual([DEFAULT_SESSION_ID]));
    expect(
      daemon.receivedHttpRequests
        .slice(httpRequestIndex)
        .filter((request) => request.path === `/api/control/session/${DEFAULT_SESSION_ID}/close`),
    ).toHaveLength(1);
    expect(
      daemon.receivedHttpRequests
        .slice(httpRequestIndex)
        .filter((request) => /\/(?:attach|list|clients)(?:\/|$)/u.test(request.path)),
    ).toHaveLength(0);
    expect(daemon.attachRequests).toHaveLength(attachCount);
  });

  it("关闭当前 session 后保留其他列表项但进入无会话状态", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000481",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 3000,
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000482",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await waitFor(() => expect(selectedSessionName()).toBe("alpha"));
    const attachCount = daemon.attachRequests.length;
    const alphaRow = (await screen.findByRole("button", { name: "Open alpha" })).closest(".session-row");
    expect(alphaRow).not.toBeNull();

    await user.click(within(alphaRow as HTMLElement).getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual(["beta"]));
    expect(selectedSessionName()).toBeUndefined();
    await waitFor(() => expect(document.querySelector('textarea[aria-label="Terminal input"]')).toBeNull());
    expect(terminalText()).toBe("");
    expect(document.querySelector(".terminal-placeholder")).toHaveTextContent("detached");
    expect(daemon.hasActiveTerminalSession(alphaSession.session_id)).toBe(false);
    expect(daemon.attachRequests).toHaveLength(attachCount);
  });

  it("关闭其他 session 后保留当前 session 的操作者", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000491",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 3000,
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000492",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    const operatorClient = {
      client_id: "00000000-0000-0000-0000-000000000791",
      device_id: "00000000-0000-0000-0000-000000000891",
      peer_ip: "192.0.2.91",
      online: true,
      connected_at_ms: 1_710_000_000_000,
      last_seen_at_ms: 1_710_000_000_000,
      attached_session_ids: [betaSession.session_id],
      cursor_session_id: betaSession.session_id,
      cursor_row: 4,
      cursor_col: 9,
      cursor_focused: true,
    };
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "attached-ready\n",
      daemonClients: [operatorClient],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await clickSessionCard(user, "beta");
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));
    await waitFor(() => expect(daemon.hasActiveTerminalSession(betaSession.session_id)).toBe(true));
    const operators = await screen.findByLabelText("session operators");
    await within(operators).findByText("192.0.2.91");
    const attachCount = daemon.attachRequests.length;
    const alphaRow = (await screen.findByRole("button", { name: "Open alpha" })).closest(".session-row");
    expect(alphaRow).not.toBeNull();

    await user.click(within(alphaRow as HTMLElement).getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual(["beta"]));
    await waitFor(() => expect(daemon.closedSessions).toEqual([alphaSession.session_id]));
    daemon.setSessions([betaSession]);
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    const terminalStillAttached = daemon.hasActiveTerminalSession(betaSession.session_id);
    daemon.setDaemonClients([{
      ...operatorClient,
      attached_session_ids: terminalStillAttached ? [betaSession.session_id] : [],
      cursor_session_id: terminalStillAttached ? betaSession.session_id : undefined,
    }]);
    expect(selectedSessionName()).toBe("beta");
    await within(screen.getByLabelText("session operators")).findByText("192.0.2.91");
    expect(terminalHost()).not.toBeNull();
    expect(terminalStillAttached).toBe(true);
    expect(daemon.attachRequests).toHaveLength(attachCount);
  });

  it("切换 session 的合并窗口内关闭旧 session 不会取消新 attach", async () => {
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000493",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 3000,
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000494",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    const operatorClient = {
      client_id: "00000000-0000-0000-0000-000000000793",
      device_id: "00000000-0000-0000-0000-000000000893",
      peer_ip: "192.0.2.93",
      online: true,
      connected_at_ms: 1_710_000_000_000,
      last_seen_at_ms: 1_710_000_000_000,
      attached_session_ids: [betaSession.session_id],
      cursor_session_id: betaSession.session_id,
      cursor_row: 4,
      cursor_col: 9,
      cursor_focused: true,
    };
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "attached-ready\n",
      daemonClients: [operatorClient],
    });
    render(<App />);

    const user = userEvent.setup();
    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await waitFor(() => expect(daemon.hasActiveTerminalSession(alphaSession.session_id)).toBe(true));
    const betaOpenButton = await screen.findByRole("button", { name: "Open beta" });
    const alphaRow = (await screen.findByRole("button", { name: "Open alpha" })).closest(".session-row");
    expect(alphaRow).not.toBeNull();
    const alphaCloseButton = within(alphaRow as HTMLElement).getByRole("button", { name: "Close session" });

    vi.useFakeTimers();
    try {
      fireEvent.click(betaOpenButton);
      expect(selectedSessionName()).toBe("beta");
      fireEvent.click(alphaCloseButton);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
    } finally {
      vi.useRealTimers();
    }

    await waitFor(() => expect(daemon.hasActiveTerminalSession(betaSession.session_id)).toBe(true));
    expect(selectedSessionName()).toBe("beta");
    await within(screen.getByLabelText("session operators")).findByText("192.0.2.93");
  });

  it("旧 session.list 响应不会把刚关闭的 session 合并回列表", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    daemon.queueSessionListResponse([
      {
        session_id: DEFAULT_SESSION_ID,
        name: DEFAULT_SESSION_NAME,
        state: "running",
        size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
      },
    ], 40);

    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => {
      expect(screen.queryByText(DEFAULT_SESSION_NAME)).toBeNull();
    });
    expect(daemon.closedSessions).toEqual([DEFAULT_SESSION_ID]);
  });

  it("旧 session.list 响应不会把当前选中态指回已关闭且隐藏的 session", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-000000000421",
      name: "alpha",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 3000,
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-000000000422",
      name: "beta",
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      created_at_ms: 2000,
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await clickSessionCard(user, "beta");
    await waitFor(() => expect(selectedSessionName()).toBe("beta"));

    daemon.queueSessionListResponse([alphaSession, betaSession], 80);
    const betaOpenButton = await screen.findByRole("button", { name: "Open beta" });
    const betaRow = betaOpenButton.closest(".session-row");
    expect(betaRow).not.toBeNull();
    await user.click(within(betaRow as HTMLElement).getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual(["alpha"]));
    expect(selectedSessionName()).toBeUndefined();
    await new Promise((resolve) => window.setTimeout(resolve, 140));

    expect(visibleSessionNames()).toEqual(["alpha"]);
    expect(selectedSessionName()).toBeUndefined();
  });

  it("关闭已被 daemon 移除的 session 时按幂等删除处理", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const sessionId = "00000000-0000-0000-0000-000000000401";
    await waitForWorkspaceSession();
    daemon.forgetSession(sessionId);

    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual([]));
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(daemon.closedSessions).toEqual([]);
  });

  it("关闭当前已 attach 的 session 时忽略晚到的 session_not_found 错误", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: DEFAULT_SESSION_ID,
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "termd-e2e-ready\n",
      closeSessionUnownedError: {
        code: "session_not_found",
        message: "session was not found",
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual([]));
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(daemon.closedSessions).toEqual([DEFAULT_SESSION_ID]);
  });

  it("terminal WebSocket 已断开时仍通过 JSON close 关闭当前 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID]));

    daemon.dropConnections();
    // 中文注释：这里要等 transport close 真正传到前端，再点击 Close session。
    // 否则如果 graceful close 还没完成，RPC 可能偶发抢在 close 生效前送达 daemon，
    // 用例就会从“关闭失败显示错误”漂成“真的关闭成功”。
    await waitFor(() => expect(daemon.activeConnectionCount()).toBe(0));
    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual([]));
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(daemon.closedSessions).toEqual([DEFAULT_SESSION_ID]);
  });

  it("关闭 session 会取消合并窗口里的迟到 attach，避免已关闭 session 被本地复活", async () => {
    const user = userEvent.setup();
    const alphaSession = {
      session_id: "00000000-0000-0000-0000-0000000004a1",
      name: "alpha",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    const betaSession = {
      session_id: "00000000-0000-0000-0000-0000000004a2",
      name: "beta",
      state: "running",
      size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
    } as const;
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [alphaSession, betaSession],
      attachOutput: "attached-ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("alpha");
    await screen.findByText(/attached-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual([alphaSession.session_id]));

    // 中文注释：清掉初始自动 attach 的痕迹，后续只观察“打开 beta 再立刻关闭 beta”
    // 这一个竞态窗口里新增的 attach 请求。
    daemon.attachedSessions.splice(0);
    daemon.attachRequests.splice(0);

    const betaRow = document
      .querySelectorAll<HTMLElement>(".session-row")
      .item(1);
    expect(betaRow).not.toBeNull();
    const openButton = within(betaRow).getByRole("button", { name: "Open beta" });
    const closeButton = within(betaRow).getByRole("button", { name: "Close session" });

    act(() => {
      fireEvent.click(openButton);
      fireEvent.click(closeButton);
    });

    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await waitFor(() => {
      expect(screen.queryByText("beta")).toBeNull();
    });

    const betaWatchedAttachRequests = daemon.attachRequests.filter(
      (request) => request.session_id === betaSession.session_id && request.watch_updates !== false,
    );
    expect(daemon.closedSessions).toEqual([betaSession.session_id]);
    expect(betaWatchedAttachRequests).toHaveLength(0);
    expect(
      daemon.receivedHttpRequests.filter((request) => request.path === `/api/control/session/${betaSession.session_id}/close`),
    ).toHaveLength(1);
    expect(daemon.attachedSessions).toEqual([]);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
  });

  it("shared-control attach 后持续发送终端输入和 PTY resize", async () => {
    const user = userEvent.setup();
      const restoreTerminalLayout = mockTerminalLayout({
        viewportWidth: 600,
        viewportHeight: 420,
        frameWidth: 1200,
        frameHeight: 592,
      });
      // 中文注释：本用例要验证“本地浏览器容器尺寸接管 shared PTY”；
      // jsdom 的 xterm mock 若不显式给 fit 尺寸，会用当前 remote rows/cols
      // 反推容器大小，导致 focus 后看起来仍是 daemon 的旧 100x30。
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 24,
        cols: 80,
      };
      try {
        await daemon.stop();
      daemon = await MockDaemon.start({
        token: "secret-token",
        sessions: [
          {
            session_id: "00000000-0000-0000-0000-000000000402",
            state: "running",
            size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
          },
        ],
      });
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      let terminalInput: HTMLTextAreaElement | null = null;
      await waitFor(() => {
        terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(terminalInput).not.toBeNull();
      });
      expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
      // 中文注释：cursor 只存在于初始 snapshot，后续位置由 PTY 输出推进；
      // 浏览器 focus 不再通过独立 HTTP control 上报。
      await waitFor(() =>
        expect(daemon.sessionResizes).toContainEqual({
          session_id: "00000000-0000-0000-0000-000000000402",
          size: { rows: 24, cols: 80, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
        }),
      );
      terminalInput!.focus();
      terminalInput!.value = "first-terminal-secret";
      fireEvent.input(terminalInput!);

      await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret"]));

      terminalInput!.value = "second-terminal-secret";
      fireEvent.input(terminalInput!);

      await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret", "second-terminal-secret"]));
      terminalInput!.blur();
      const resizeCountAfterBlur = daemon.sessionResizes.length;
      fireEvent(window, new Event("focus"));
      terminalInput!.focus();
      expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
      expect(
        daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
      ).toBe(false);
      expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
      fireEvent(window, new Event("resize"));
      expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
      expect(daemon.outerWireText()).not.toContain("first-terminal-secret");
      expect(daemon.outerWireText()).not.toContain("second-terminal-secret");
    } finally {
      restoreTerminalLayout();
    }
  });

  it("移动端键盘上方快捷按钮会发送常用控制字符", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    setTouchCapability(true);
    setMobileVisualViewport(820, 460, 20);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => {
      expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    });

    await user.click(screen.getByRole("button", { name: "Send Tab" }));
    await user.click(screen.getByRole("button", { name: "Send Ctrl-C" }));
    await user.click(screen.getByRole("button", { name: "Send Ctrl-Z" }));

    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["\t", "\x03", "\x1a"]));
  });

  it("宽屏触摸设备仍会启用终端移动输入保护，但不会切换成移动布局", async () => {
    const user = userEvent.setup();
    setViewportWidth(1180);
    setTouchCapability(true);
    setMobileVisualViewport(820, 820, 0);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/termd-e2e-ready/);

    const shell = document.querySelector<HTMLElement>(".app-shell");
    expect(shell).not.toHaveClass("mobile-keyboard-open");
    expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    expect(screen.queryByRole("button", { name: "Send Tab" })).toBeNull();
  });

  it("移动端长按终端一秒后拖动会发送方向键序列", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => {
      expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    });
    const terminalFrame = await waitFor(() => {
      const frame = document.querySelector<HTMLElement>(".terminal-frame");
      expect(frame).not.toBeNull();
      return frame!;
    });

    vi.useFakeTimers();
    try {
      fireTouchPointer(terminalFrame, "pointerdown", {
        pointerId: 11,
        clientX: 160,
        clientY: 240,
      });
      act(() => {
        vi.advanceTimersByTime(1000);
      });

      expect(screen.queryByLabelText("mobile direction gesture")).toBeNull();
      fireTouchPointer(terminalFrame, "pointermove", {
        pointerId: 11,
        clientX: 160,
        clientY: 150,
      });
      expect(screen.getByLabelText("mobile direction gesture")).toBeInTheDocument();
      fireTouchPointer(terminalFrame, "pointerup", {
        pointerId: 11,
        clientX: 160,
        clientY: 150,
      });
    } finally {
      vi.useRealTimers();
    }

    await waitFor(() => expect(daemon.sessionDataMessages).toContain("\x1b[A"));
  });

  it("session 分辨率与当前客户端一致时不显示虚线框和缩放按钮", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000403",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    await waitFor(() => {
      expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    });
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
  });

  it("移动端独占 session 即使分辨率不一致也不显示虚线框", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000413",
          state: "running",
          size: { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    await waitFor(() => {
      expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    });
    expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
  });

  it("聚焦终端遇到浏览器窗口 resize 后保持聚焦并同步 PTY 尺寸", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000404",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() =>
      expect(daemon.sessionResizes).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000404",
        size: { rows: 30, cols: 100, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    expect(document.activeElement).toBe(terminalInput);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
  });

  it("前端发出 resize 请求后等 daemon 确认才更新 session 尺寸", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      resizeAckDelayMs: 240,
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000407",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();
    await waitFor(() => expect(screen.getAllByText("80x24").length).toBeGreaterThan(0));

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() =>
      expect(daemon.sessionResizes).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000407",
        size: { rows: 30, cols: 100, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    // resize 请求已经发出，但 mock daemon 还没返回 session_resized；UI 仍展示旧 session 尺寸。
    expect(screen.getAllByText("80x24").length).toBeGreaterThan(0);
    expect(screen.queryByText("100x30")).toBeNull();

    await screen.findByText("100x30");
    expect(screen.queryByText("80x24")).toBeNull();
  });

  it("持续输出场景下 resize ack 延迟不卸载已 attach 终端", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "resize-timeout-ready\n",
      resizeAckDelayMs: APP_CONNECTION_TIMEOUT_MS + 700,
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000408",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/resize-timeout-ready/);
    await clickSessionCard(user);

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 31,
      cols: 101,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() =>
      expect(daemon.sessionResizes).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000408",
        size: { rows: 31, cols: 101, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    // resize 在 terminal WebSocket 上等待服务端 frame 确认；确认延迟不能卸载 xterm。
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(document.body.textContent).not.toContain("response_timeout");
    expect(screen.getByText(/resize-timeout-ready/)).toBeInTheDocument();
    expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
  });

  it("终端聚焦不发送 cursor HTTP 且不影响已 attach 终端", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "cursor-timeout-ready\n",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000409",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/cursor-timeout-ready/);
    await clickSessionCard(user);

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();

    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(document.body.textContent).not.toContain("response_timeout");
    expect(screen.getByText(/cursor-timeout-ready/)).toBeInTheDocument();
    expect(document.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    terminalInput!.value = "after-focus-input";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-focus-input"));
  });

  it("session.resize 的 HTTP control 瞬时失败不升级成全局断线", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "resize-http-closed-ready\n",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-00000000040a",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/resize-http-closed-ready/);
    await clickSessionCard(user);

    daemon.closeNextSessionResizeRequests(1);
    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 32,
      cols: 102,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() =>
      expect(daemon.sessionResizes).toContainEqual({
        session_id: "00000000-0000-0000-0000-00000000040a",
        size: { rows: 32, cols: 102, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    // 中文注释：relay/浏览器把 session.resize 这笔 HTTP control fetch 直接打断时，
    // 只能丢掉本次辅助 ack，不能把整个 workspace 升级成 Connection error。
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(document.body.textContent).not.toContain("mock session resize closed");
    terminalInput!.value = "after-resize-http-close";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-resize-http-close"));
  });

  it("终端 focus 不调用 session.cursor HTTP control", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "cursor-http-closed-ready\n",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-00000000040b",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/cursor-http-closed-ready/);
    await clickSessionCard(user);

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();

    await new Promise((resolve) => window.setTimeout(resolve, 80));

    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(document.body.textContent).not.toContain("mock session cursor closed");
    terminalInput!.value = "after-cursor-http-close";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-cursor-http-close"));
  });

  it("session.resize 的 HTTP control 返回 http_file_transfer_failed 时不升级成全局断线", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "resize-http-file-transfer-ready\n",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-00000000040c",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/resize-http-file-transfer-ready/);
    await clickSessionCard(user);

    const restoreFetch = installHttpControlFailureOnceMock("/api/control/session/00000000-0000-0000-0000-00000000040c/resize");
    try {
      const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
      terminalInput!.focus();
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 34,
        cols: 104,
      };
      fireEvent(window, new Event("resize"));
      await new Promise((resolve) => window.setTimeout(resolve, 80));

      // 中文注释：relay/浏览器把 sidecar HTTP control 返回成非协议 5xx/plain body 时，
      // 前端会归一成 http_file_transfer_failed。这里仍只能丢掉本次辅助 ack。
      expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
      expect(document.body.textContent).not.toContain("http_file_transfer_failed");
      terminalInput!.value = "after-resize-http-file-transfer-failed";
      fireEvent.input(terminalInput!);
      await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-resize-http-file-transfer-failed"));
    } finally {
      restoreFetch();
    }
  });

  it("终端 focus 不依赖 session.cursor HTTP 响应", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      attachOutput: "cursor-http-file-transfer-ready\n",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-00000000040d",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await screen.findByText(/cursor-http-file-transfer-ready/);
    await clickSessionCard(user);

    const terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
    expect(terminalInput).not.toBeNull();
    terminalInput!.focus();
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    terminalInput!.value = "after-focus-without-cursor-http";
    fireEvent.input(terminalInput!);
    await waitFor(() => expect(daemon.sessionDataMessages).toContain("after-focus-without-cursor-http"));
  });

  it("浏览器窗口 resize 引发的短暂 focusout/focusin 不创建 cursor HTTP", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000405",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();

    fireEvent(window, new Event("resize"));
    // 真实浏览器在拖动窗口边界时可能短暂让 xterm textarea 失焦，随后又恢复焦点；
    // 这类 resize 伴随的瞬时 DOM focus 抖动不应变成 operator 的 focused/blurred 抖动。
    terminalInput!.blur();
    await new Promise((resolve) => window.setTimeout(resolve, 40));
    terminalInput!.focus();
    await new Promise((resolve) => window.setTimeout(resolve, 180));

    expect(document.activeElement).toBe(terminalInput);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
  });

  it("terminal frame 渲染后不发送 flow packet", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000406",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toBeInTheDocument());
    await waitFor(() =>
      expect(daemon.attachedSessions).toContain("00000000-0000-0000-0000-000000000406"),
    );
    await waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).not.toBeNull());
    await new Promise((resolve) => window.setTimeout(resolve, 120));

    const receivedPackets = () =>
      (
        daemon as unknown as {
          receivedPackets?: Array<{ kind: string; credit?: number }>;
        }
      ).receivedPackets ?? [];
    const countFlowPackets = () => receivedPackets().filter((packet) => packet.kind === "flow").length;
    const flowPacketsBefore = countFlowPackets();

    daemon.pushTerminalFrameBatch("00000000-0000-0000-0000-000000000406", [
      {
        kind: "snapshot",
        session_id: "00000000-0000-0000-0000-000000000406",
        base_seq: 0,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        data_base64: "",
      },
      {
        kind: "output",
        session_id: "00000000-0000-0000-0000-000000000406",
        terminal_seq: 1,
        data_base64: "YWJjZA==",
      },
    ]);

    await waitFor(() => expect(terminalText()).toContain("abcd"));
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(countFlowPackets()).toBe(flowPacketsBefore);
  });

  it("legacy session_data 渲染完成后不发送 flow packet", async () => {
    const user = userEvent.setup();
    const sessionId = "00000000-0000-0000-0000-000000000410";
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: sessionId,
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => expect(daemon.attachedSessions).toEqual([sessionId]));

    const outputStream = daemon.receivedPackets.find((packet) => {
      if (packet.kind !== "stream_open" || packet.method !== "terminal.attach") {
        return false;
      }
      const payload = packet.payload as { session_id?: string; watch_updates?: boolean };
      return payload.session_id === sessionId && payload.watch_updates === true;
    });
    expect(outputStream?.stream_id).toBeDefined();
    const flowPackets = () =>
      daemon.receivedPackets.filter((packet) => packet.kind === "flow" && packet.stream_id === outputStream!.stream_id);
    const flowPacketsBefore = flowPackets().length;
    const text = "legacy-stream-output\n";

    daemon.pushSessionData(sessionId, text);

    await screen.findByText(/legacy-stream-output/);
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(flowPackets()).toHaveLength(flowPacketsBefore);
  });

  it("同一批 terminal frame 渲染完成后不发送 flow packet", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000409",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
      attachOutput: "ready\n",
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toBeInTheDocument());
    await waitFor(() =>
      expect(daemon.attachedSessions).toContain("00000000-0000-0000-0000-000000000409"),
    );
    await waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')).not.toBeNull());
    await new Promise((resolve) => window.setTimeout(resolve, 120));

    const receivedPackets = () =>
      (
        daemon as unknown as {
          receivedPackets?: Array<{ kind: string; credit?: number }>;
        }
      ).receivedPackets ?? [];
    const countFlowPackets = () => receivedPackets().filter((packet) => packet.kind === "flow").length;
    const flowPacketsBefore = countFlowPackets();
    const chunk = Buffer.alloc(8 * 1024, "x").toString("base64");

    daemon.pushTerminalFrameBatch("00000000-0000-0000-0000-000000000409", [
      {
        kind: "snapshot",
        session_id: "00000000-0000-0000-0000-000000000409",
        base_seq: 0,
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        data_base64: "",
      },
      {
        kind: "output",
        session_id: "00000000-0000-0000-0000-000000000409",
        terminal_seq: 1,
        data_base64: chunk,
      },
      {
        kind: "output",
        session_id: "00000000-0000-0000-0000-000000000409",
        terminal_seq: 2,
        data_base64: chunk,
      },
      {
        kind: "output",
        session_id: "00000000-0000-0000-0000-000000000409",
        terminal_seq: 3,
        data_base64: chunk,
      },
      {
        kind: "output",
        session_id: "00000000-0000-0000-0000-000000000409",
        terminal_seq: 4,
        data_base64: chunk,
      },
    ]);

    await waitFor(() => expect(terminalText()).toContain("x"));
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(countFlowPackets()).toBe(flowPacketsBefore);
  });

  it("浏览器窗口失活后不再继续上报 PTY resize", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000408",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();

    const resizeCountAfterFocus = daemon.sessionResizes.length;
    fireEvent(window, new Event("blur"));
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));
    await new Promise((resolve) => window.setTimeout(resolve, 160));

    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterFocus);
    expect(
      daemon.receivedHttpRequests.some((request) => request.path.endsWith("/cursor")),
    ).toBe(false);
    expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
  });

  it("已有在线客户端时第二个客户端聚焦后按自己的分辨率接管 PTY", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000409",
          state: "running",
          size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        },
      ],
    });
    const firstRender = render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);

    let firstTerminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      firstTerminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
      expect(firstTerminalInput).not.toBeNull();
    });
    firstTerminalInput!.focus();
    fireEvent(window, new Event("blur"));
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    const resizeCountAfterBlur = daemon.sessionResizes.length;
    const secondRender = render(<App />);
    await waitForWorkspaceSession();
    await clickSessionCard(user, undefined, secondRender.container);

    await waitFor(() => {
      expect(secondRender.container.querySelector('textarea[aria-label="Terminal input"]')).not.toBeNull();
    });
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    const secondTerminalFrame = secondRender.container.querySelector<HTMLElement>(".terminal-frame");
    expect(secondTerminalFrame).not.toBeNull();
    await user.click(secondTerminalFrame!);

    await waitFor(() =>
      expect(daemon.sessionResizes.slice(resizeCountAfterBlur)).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000409",
        size: { rows: 30, cols: 100, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    expect(within(secondRender.container).queryByRole("button", { name: /zoom/i })).toBeNull();

    secondRender.unmount();
    firstRender.unmount();
  });

  it("失焦后窗口 resize 不触发本地客户端接管 PTY 尺寸", async () => {
    const user = userEvent.setup();
    const restoreTerminalLayout = mockTerminalLayout({
      viewportWidth: 600,
      viewportHeight: 420,
      frameWidth: 1200,
      frameHeight: 900,
      scrollHeight: 900,
    });
    await daemon.stop();
    try {
      daemon = await MockDaemon.start({
        token: "secret-token",
        sessions: [
          {
            session_id: "00000000-0000-0000-0000-000000000406",
            state: "running",
            size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
          },
        ],
      });
      render(<App />);

      await pairWithInvite(user, daemon);
      await waitForWorkspaceSession();
      await clickSessionCard(user);

      let terminalInput: HTMLTextAreaElement | null = null;
      await waitFor(() => {
        terminalInput = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]');
        expect(terminalInput).not.toBeNull();
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 30,
        cols: 100,
      };
      const resizeCountBeforeWindowResize = daemon.sessionResizes.length;
      fireEvent(window, new Event("resize"));
      expect(screen.queryByRole("button", { name: /zoom/i })).toBeNull();
      expect(daemon.sessionResizes).toHaveLength(resizeCountBeforeWindowResize);
    } finally {
      restoreTerminalLayout();
    }
  });

  it("未配对时只显示连接表单，并按当前页面来源和前缀推导 WebSocket 地址", async () => {
    render(<App />);

    expect(await screen.findByLabelText("daemon admin")).toBeVisible();
    expect(await screen.findByLabelText("WS URL")).toHaveValue(defaultWsUrlFromPage());
    expect(screen.getByRole("button", { name: "Workspace" })).toBeDisabled();
    expect(within(screen.getByLabelText("daemon manager")).getByText("No daemons")).toBeVisible();
    expect(screen.getByLabelText("Pairing token")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Scan QR" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New session" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
    expect(defaultWsUrlFromPage({ protocol: "http:", host: "192.168.55.155:8765" })).toBe(
      "ws://192.168.55.155:8765/ws",
    );
    expect(defaultWsUrlFromPage({ protocol: "https:", host: "example.test" })).toBe("wss://example.test/ws");
    expect(defaultWsUrlFromPage({ protocol: "https:", host: "example.test", pathname: "/termd/" })).toBe(
      "wss://example.test/termd/ws",
    );
    expect(defaultWsUrlFromPage({ protocol: "https:", host: "example.test", pathname: "/termd/index.html" })).toBe(
      "wss://example.test/termd/ws",
    );
    expect(
      browserReachableWsUrl("ws://127.0.0.1:8765/ws", {
        protocol: "http:",
        host: "192.168.55.155:8765",
        hostname: "192.168.55.155",
        pathname: "/termd/",
      }),
    ).toBe("ws://192.168.55.155:8765/termd/ws");
  });

  it("pairingWsUrlCandidates 会优先当前 Web 页面并统一到 /ws", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";
    const relayPage = {
      protocol: "https:",
      host: "relay.example",
      hostname: "relay.example",
      pathname: "/termd/",
    };

    expect(pairingWsUrlCandidates("wss://relay.example/termd/ws", serverId, relayPage)).toEqual([
      "wss://relay.example/termd/ws",
    ]);
    expect(pairingWsUrlCandidates("wss://relay.example/termd/ws?relay_token=abc", serverId, relayPage)).toEqual([
      "wss://relay.example/termd/ws",
    ]);
    expect(pairingWsUrlCandidates("wss://relay.example/termd/ws/00000000-0000-0000-0000-000000000123/client", serverId, relayPage)).toEqual([
      "wss://relay.example/termd/ws",
    ]);
  });

  it("已配对 daemon 从 relay 页面打开时优先使用当前页面 /ws，再回退到旧保存地址", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";
    const relayPage = {
      protocol: "https:",
      host: "termd.yiln.de",
      hostname: "termd.yiln.de",
      pathname: "/",
    };

    expect(
      knownServerWsUrlCandidates(
        "wss://old-relay.example/ws/00000000-0000-0000-0000-000000000123/client?relay_token=abc",
        serverId,
        relayPage,
      ),
    ).toEqual([
      "wss://termd.yiln.de/ws",
      "wss://old-relay.example/ws",
    ]);
  });

  it("HTTPS relay 页面同 hostname 但端口或路径变化时优先使用当前页面 /ws", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";
    const relayPage = {
      protocol: "https:",
      host: "termd.yiln.de",
      hostname: "termd.yiln.de",
      pathname: "/relay/",
    };

    expect(
      knownServerWsUrlCandidates(
        "wss://termd.yiln.de:9443/ws/00000000-0000-0000-0000-000000000123/client?relay_token=abc",
        serverId,
        relayPage,
      ),
    ).toEqual([
      "wss://termd.yiln.de/relay/ws",
      "wss://termd.yiln.de:9443/ws",
    ]);
  });

  it("Web 和 relay 同主机不同端口时优先使用显式 relay URL", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";
    const devPage = {
      protocol: "http:",
      host: "192.168.55.155:4174",
      hostname: "192.168.55.155",
      pathname: "/",
    };

    expect(knownServerWsUrlCandidates("ws://192.168.55.155:19180/ws", serverId, devPage)).toEqual([
      "ws://192.168.55.155:19180/ws",
      "ws://192.168.55.155:4174/ws",
    ]);
  });

  it("移动端从 relay 页面扫描默认 localhost 邀请码时会生成统一 /ws 候选 URL", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";
    const reachable = browserReachableWsUrl("ws://127.0.0.1:8765/ws", {
      protocol: "https:",
      host: "relay.example",
      hostname: "relay.example",
      pathname: "/relay/",
    });

    expect(reachable).toBe("wss://relay.example/relay/ws");
    expect(pairingWsUrlCandidates("ws://127.0.0.1:8765/ws", serverId, {
      protocol: "https:",
      host: "relay.example",
      hostname: "relay.example",
      pathname: "/relay/",
    })).toEqual([
      "wss://relay.example/relay/ws",
    ]);
  });

  it("同一个 invite 在 daemon Web 页面会优先尝试当前 daemon 直连地址", () => {
    const serverId = "00000000-0000-0000-0000-000000000123";

    expect(pairingWsUrlCandidates(
      "wss://relay.example/ws/00000000-0000-0000-0000-000000000123/client?relay_token=abc",
      serverId,
      {
        protocol: "http:",
        host: "192.168.55.155:8765",
        hostname: "192.168.55.155",
        pathname: "/termd/",
      },
    )).toEqual([
      "ws://192.168.55.155:8765/termd/ws",
      "wss://relay.example/ws",
    ]);
  });

  it("点击 Scan QR 后打开扫码 pairing 界面入口", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));

    expect(await screen.findByRole("dialog", { name: "Scan pairing QR" })).toBeInTheDocument();
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));
    await screen.findByText(/Scanning/);
  });

  it("扫码器在 iPhone Safari 上使用全画面扫描区域提高终端二维码识别率", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));

    const video = document.createElement("video");
    Object.defineProperty(video, "videoWidth", { configurable: true, value: 1920 });
    Object.defineProperty(video, "videoHeight", { configurable: true, value: 1080 });

    expect(qrScannerMock.options?.calculateScanRegion?.(video)).toEqual({
      x: 0,
      y: 0,
      width: 1920,
      height: 1080,
      downScaledWidth: 960,
      downScaledHeight: 540,
    });
    expect(await screen.findByText(/Fill the frame/)).toBeInTheDocument();
  });

  it("扫码界面关闭时释放摄像头 scanner", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });
    await user.click(screen.getByRole("button", { name: "Close scanner" }));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    expect(qrScannerMock.destroy).toHaveBeenCalledTimes(1);
  });

  it("启动中关闭扫码界面后不会继续启动摄像头 scanner", async () => {
    const user = userEvent.setup();
    let resolveHasCamera: (value: boolean) => void = () => undefined;
    qrScannerMock.hasCamera.mockReturnValue(
      new Promise<boolean>((resolve) => {
        resolveHasCamera = resolve;
      }),
    );
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });
    await waitFor(() => expect(qrScannerMock.hasCamera).toHaveBeenCalledTimes(1));
    await user.click(screen.getByRole("button", { name: "Close scanner" }));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    resolveHasCamera(true);

    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(qrScannerMock.destroy).not.toHaveBeenCalled();
    expect(qrScannerMock.start).not.toHaveBeenCalled();
  });

  it("scanner start 等待期间关闭扫码界面会销毁 scanner 且不重复释放", async () => {
    const user = userEvent.setup();
    let resolveStart: () => void = () => undefined;
    qrScannerMock.start.mockReturnValue(
      new Promise<void>((resolve) => {
        resolveStart = resolve;
      }),
    );
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));
    await user.click(screen.getByRole("button", { name: "Close scanner" }));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    expect(qrScannerMock.destroy).toHaveBeenCalledTimes(1);
    resolveStart();

    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(qrScannerMock.destroy).toHaveBeenCalledTimes(1);
  });

  it("扫描到 QR 内容后关闭扫码界面并填入 Pairing token", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });

    qrScannerMock.onDecode?.({ data: "scanned-token" });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    expect(screen.getByLabelText("Pairing token")).toHaveValue("scanned-token");
    expect(qrScannerMock.stop).toHaveBeenCalledTimes(1);
  });

  it("扫描 termd-pair 邀请码后自动配对且不显示 token", async () => {
    const user = userEvent.setup();
    render(<App />);
    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      ws_url: daemon.url,
      token: "secret-token",
      server_id: daemon.serverId,
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));

    qrScannerMock.onDecode?.({ data: inviteCode });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitForWorkspaceSession();
    await expectDaemonUrlInAdmin(user, daemon.url);
    expect(qrScannerMock.stop).toHaveBeenCalledTimes(1);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("扫码无法识别时可在扫码弹窗粘贴 invite 完成配对", async () => {
    const user = userEvent.setup();
    render(<App />);
    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      ws_url: daemon.url,
      token: "secret-token",
      server_id: daemon.serverId,
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });

    fireEvent.change(screen.getByLabelText("Invite code"), { target: { value: inviteCode } });
    await user.click(screen.getByRole("button", { name: "Use invite" }));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitForWorkspaceSession();
    await expectDaemonUrlInAdmin(user, daemon.url);
    expect(qrScannerMock.stop).toHaveBeenCalledTimes(1);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("扫码无法识别时可上传二维码图片解析 invite", async () => {
    const user = userEvent.setup();
    render(<App />);
    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      ws_url: daemon.url,
      token: "secret-token",
      server_id: daemon.serverId,
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
    qrScannerMock.scanImage.mockResolvedValue({ data: inviteCode, cornerPoints: [] });

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await screen.findByRole("dialog", { name: "Scan pairing QR" });

    const file = new File(["qr"], "pairing.png", { type: "image/png" });
    fireEvent.change(screen.getByLabelText("Upload QR image"), { target: { files: [file] } });

    await waitFor(() => expect(qrScannerMock.scanImage).toHaveBeenCalledWith(file, expect.objectContaining({ returnDetailedScanResult: true })));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await waitForWorkspaceSession();
    await expectDaemonUrlInAdmin(user, daemon.url);
    expect(qrScannerMock.stop).toHaveBeenCalledTimes(1);
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });

  it("扫描 server_id 不匹配的邀请码时拒绝配对且不显示 token", async () => {
    const user = userEvent.setup();
    render(<App />);
    const payload = JSON.stringify({
      type: "termd_pairing_qr",
      version: 1,
      ws_url: daemon.url,
      token: "secret-token",
      server_id: "00000000-0000-0000-0000-000000000999",
      daemon_public_key: daemon.daemonPublicKey,
      expires_at_ms: Date.now() + 60_000,
    });
    const inviteCode = `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));

    qrScannerMock.onDecode?.({ data: inviteCode });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull());
    const alert = await screen.findByRole("alert", { name: "Connection error" });
    expect(alert).toHaveTextContent("pairing_payload_server_mismatch");
    expect(alert).toHaveTextContent("pairing payload does not match the connected daemon");
    expect(screen.getByLabelText("Pairing token")).toHaveValue("");
    expectPairingTokenOnlyInRelayAdmission(daemon);
  });
});
