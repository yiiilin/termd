import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, {
  browserReachableWsUrl,
  connectPairingClient,
  defaultWsUrlFromPage,
  networkRateFromSamples,
  pairingWsUrlCandidates,
} from "../App";
import type { SessionFilesResultPayload } from "../protocol/types";
import { clearBrowserState, loadBrowserState } from "../state/browser-state";
import { MockDaemon } from "../test/mock-daemon";
import { fallbackSessionDisplayName } from "../session-names";
import { resetFileEditorDialogMonacoCacheForTests } from "../components/FileEditorDialog";

const DEFAULT_SESSION_ID = "00000000-0000-0000-0000-000000000401";
const DEFAULT_SESSION_NAME = fallbackSessionDisplayName(DEFAULT_SESSION_ID);

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

function dispatchMobileTextInput(target: HTMLTextAreaElement, data: string): InputEvent {
  const event = new InputEvent("beforeinput", {
    bubbles: true,
    cancelable: true,
    data,
    inputType: "insertText",
  });
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
  await waitFor(() => expect(within(admin).getAllByText(url).length).toBeGreaterThan(0));
}

async function waitForWorkspaceSession(name?: string): Promise<void> {
  await waitForWorkspaceReady();
  if (name) {
    await waitFor(() => expect(screen.queryAllByText(name).length).toBeGreaterThan(0));
    return;
  }
  await waitFor(() => {
    const sessionRows = document.querySelectorAll(".session-row").length;
    const toolbarName = document.querySelector<HTMLElement>(".toolbar-title span")?.textContent?.trim();
    expect(sessionRows > 0 || Boolean(toolbarName && toolbarName !== "No session")).toBe(true);
  });
}

async function waitForWorkspaceReady(): Promise<void> {
  await screen.findByTestId("terminal-pane");
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

function visibleSessionNames(): string[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".session-row strong"))
    .map((element) => element.textContent?.trim() ?? "")
    .filter(Boolean);
}

function resetXtermStats(): { writes: number; refreshes: number; writtenBytes: number } {
  const scope = globalThis as { __TERMD_TEST_XTERM_STATS__?: { writes: number; refreshes: number; writtenBytes: number } };
  scope.__TERMD_TEST_XTERM_STATS__ = { writes: 0, refreshes: 0, writtenBytes: 0 };
  return scope.__TERMD_TEST_XTERM_STATS__;
}

function triggerXtermSelection(text: string): void {
  const scope = globalThis as { __TERMD_TEST_XTERM__?: { select: (text: string) => void } };
  expect(scope.__TERMD_TEST_XTERM__).toBeDefined();
  // 测试 mock 只暴露选择完成事件，避免测试直接依赖 xterm 内部 DOM 结构。
  scope.__TERMD_TEST_XTERM__!.select(text);
}

function mockViewerLayout(input: {
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
    return this.classList.contains("terminal-viewer-frame") ? input.frameWidth : 0;
  });
  const offsetHeightSpy = vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("terminal-viewer-frame") ? input.frameHeight : 0;
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

  beforeEach(async () => {
    // app 集成测试运行在 jsdom 中；这里固定使用 fallback 编辑器，Monaco 的生产加载由构建验证覆盖。
    (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__ = true;
    resetFileEditorDialogMonacoCacheForTests();
    await clearBrowserState();
    setViewportWidth(1366);
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
    resetFileEditorDialogMonacoCacheForTests();
    delete (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__;
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await waitForWorkspaceSession();
    expect(document.body.textContent).not.toContain("00000000-0000-0000-0000-000000000401");
    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();

    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]));
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    expect(daemon.pingMessages).toBeGreaterThan(0);
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });

  it("设置面板支持切换语言和浅色主题，并持久化到浏览器本地状态", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    await user.click(screen.getByLabelText("Light"));
    await user.click(screen.getByLabelText("中文"));

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
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]));
  });

  it("在底部状态栏显示 daemon 状态，移动端只保留核心指标", async () => {
    const user = userEvent.setup();
    const desktopRender = render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    const desktopStatus = await screen.findByRole("contentinfo", { name: "daemon server status" });
    await within(desktopStatus).findByText("CPU");
    expect(within(desktopStatus).getByText("7.5%")).toBeInTheDocument();
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

  it("daemon 状态栏注册 1 秒自动轮询", async () => {
    const intervalSpy = vi.spyOn(window, "setInterval");
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    expect(intervalSpy).toHaveBeenCalledWith(expect.any(Function), 1000);
    intervalSpy.mockRestore();
  });

  it("底部状态栏使用固定列宽，避免指标内容变化时横向抖动", () => {
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

    expect(css).toContain("--daemon-status-cpu-width: 148px;");
    expect(css).toContain("--daemon-status-memory-width: 188px;");
    expect(css).toContain("--daemon-status-network-width: 236px;");
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

    expect(css).toContain("width: min(100vw, 100dvw);");
    expect(css).toContain("max-width: min(100vw, 100dvw);");
    expect(css).toContain(".daemon-status-strip {\n    width: 100%;");
    expect(css).toContain(".daemon-status-strip .daemon-status-grid {\n    width: 100%;");
    expect(css).toContain("display: grid;\n    grid-template-columns:\n      68px");
    expect(css).toContain(".terminal-mobile-shortcuts {\n    width: 100%;");
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

  it("daemon 状态栏显示网络延迟", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    const status = await screen.findByRole("contentinfo", { name: "daemon server status" });

    expect(await within(status).findByText(/RTT \d+ms/)).toBeInTheDocument();
    expect(daemon.pingMessages).toBeGreaterThan(0);
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

    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(visibleSessionNames()).toEqual(["shell", "work"]));
  });

  it("刷新 session list 时采用 daemon 返回顺序，而不是本地旧排序", async () => {
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

    daemon.setSessions([shellSession, workSession]);
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    await waitFor(() => expect(visibleSessionNames()).toEqual(["shell", "work"]));
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
    void user.click(screen.getByRole("button", { name: "Refresh" }));

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

  it("持续输出时合并写入 xterm，并且不为每个输出刷新布局", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);
    await new Promise((resolve) => window.setTimeout(resolve, 80));
    daemon.sessionCursorUpdates.length = 0;
    const stats = resetXtermStats();

    for (let index = 0; index < 80; index += 1) {
      daemon.pushSessionData(DEFAULT_SESSION_ID, `burst-output-${index}\n`);
    }

    await waitFor(() =>
      expect(document.querySelector<HTMLElement>(".xterm")?.textContent).toContain("burst-output-79"),
    );
    await new Promise((resolve) => window.setTimeout(resolve, 160));

    expect(stats.writes).toBeLessThan(80);
    expect(stats.refreshes).toBeLessThanOrEqual(1);
    expect(daemon.sessionCursorUpdates.length).toBeLessThan(20);
  });

  it("后台 session 收到输出时标记新输出，打开后清除", async () => {
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

    daemon.pushSessionDataToAll(workSessionId, "background-work-output\n");

    await waitFor(() => expect(screen.getByRole("button", { name: /Open work/ })).toHaveClass("has-new-output"));
    // 新输出提示只通过标题颜色表达，避免整行高亮或额外徽标长期占用列表空间。
    expect(screen.queryByText("New output")).toBeNull();
    expect(screen.getByRole("button", { name: "Open shell" })).not.toHaveClass("has-new-output");
    expect(document.querySelector<HTMLElement>(".xterm")?.textContent).not.toContain("background-work-output");

    await clickSessionCard(user, "work");

    await waitFor(() => expect(screen.getByRole("button", { name: "Open work" })).not.toHaveClass("has-new-output"));
  });

  it("xterm 鼠标选中后自动复制并提示复制成功", async () => {
    const user = userEvent.setup();
    const writeTextSpy = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await clickSessionCard(user);
    await screen.findByText(/termd-e2e-ready/);

    triggerXtermSelection("termd-e2e-ready");

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

    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
    const xterm = document.querySelector<HTMLElement>(".xterm");
    expect(terminalInput).not.toBeNull();
    expect(xterm).not.toBeNull();
    terminalInput!.blur();

    const renderedText = document.createElement("span");
    renderedText.textContent = "rendered-terminal-text";
    // xterm 的文字层会处理鼠标选择，真实浏览器里可能阻断冒泡阶段事件。
    // 测试这里显式阻断冒泡，确保外层捕获阶段仍能完成聚焦。
    renderedText.addEventListener("mousedown", (event) => event.stopPropagation());
    renderedText.addEventListener("click", (event) => event.stopPropagation());
    xterm!.append(renderedText);

    fireEvent.mouseDown(renderedText);
    fireEvent.click(renderedText);

    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: DEFAULT_SESSION_ID,
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );
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
    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]));

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
    expect(shell.style.getPropertyValue("--termd-visual-viewport-height")).toBe("460px");
    expect(shell.style.getPropertyValue("--termd-visual-viewport-offset-top")).toBe("20px");
    expect(screen.getByRole("contentinfo", { name: "daemon server status" })).toHaveClass(
      "daemon-status-strip",
    );
    expect(screen.getByLabelText("mobile terminal shortcuts")).toBeInTheDocument();
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

  it("未保存 daemon 时手动 token 不会猜测 server_id", async () => {
    const user = userEvent.setup();
    render(<App />);

    await setConnectionUrl(user, daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await screen.findByText("pairing_server_unknown: pairing requires a known daemon server id");
    expect(daemon.outerWireLog).toEqual([]);
  });

  it("WebSocket 外层 error envelope 会在 admin 主体显示 alert", async () => {
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

    const admin = await screen.findByLabelText("daemon admin");
    const alert = await within(admin).findByRole("alert", { name: "Connection error" });
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
      await user.click(screen.getByRole("button", { name: "Refresh" }));
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const refreshedManager = await screen.findByLabelText("daemon manager");
      await user.click(within(refreshedManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitForWorkspaceSession();
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
      await waitForWorkspaceSession();

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      await setConnectionUrl(user, secondDaemon.url);
      fireEvent.change(screen.getByLabelText("Pairing token"), {
        target: { value: pairingInviteCode(secondDaemon, "second-token") },
      });
      await user.click(screen.getByRole("button", { name: "Pair" }));
      await waitForWorkspaceSession("No session");

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
      await waitForWorkspaceSession("No session");

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const initialManager = await screen.findByLabelText("daemon manager");
      await user.click(within(initialManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitForWorkspaceSession();

      await secondDaemon.stop();
      secondStopped = true;

      await user.click(screen.getByRole("button", { name: "Daemons" }));
      const manager = await screen.findByLabelText("daemon manager");
      await user.click(within(manager).getByRole("button", { name: /Use daemon Daemon 2/ }));

      await waitFor(() => expect(screen.getByText("error")).toBeInTheDocument(), { timeout: 12_000 });
      const recoveredAdmin = await screen.findByLabelText("daemon admin");
      const recoveredManager = within(recoveredAdmin).getByLabelText("daemon manager");
      expect(recoveredManager).toBeVisible();

      await user.click(within(recoveredManager).getByRole("button", { name: /Use daemon Daemon 1/ }));
      await waitFor(() => expect(screen.getByLabelText("selected daemon")).toHaveTextContent(daemon.url));
      await user.click(screen.getByRole("button", { name: "Open workspace" }));
      await waitForWorkspaceSession();
      await waitForWorkspaceSession();
    } finally {
      if (!secondStopped) {
        await secondDaemon.stop();
      }
    }
  });

  it("配对候选 URL 会跳过 server_id 不匹配的 daemon", async () => {
    const secondDaemon = await MockDaemon.start({
      token: "second-token",
      sessions: [],
    });

    try {
      const { client, effectiveUrl } = await connectPairingClient(
        [daemon.url, secondDaemon.url],
        secondDaemon.serverId,
        "00000000-0000-0000-0000-000000000999",
        secondDaemon.daemonPublicKey,
      );

      expect(effectiveUrl).toBe(secondDaemon.url);
      expect(client.serverId).toBe(secondDaemon.serverId);
      client.close();
    } finally {
      await secondDaemon.stop();
    }
  });

  it("点击 session 卡片直接进入 shared-control operator", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await user.click(screen.getByRole("button", { name: "Refresh" }));

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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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

    await waitFor(() => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]));
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
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2200 },
    );
    observer.disconnect();
    expect(sawConnectionAlert).toBe(false);
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
      () => expect(daemon.attachedSessions).toEqual([DEFAULT_SESSION_ID, DEFAULT_SESSION_ID]),
      { timeout: 2200 },
    );
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(screen.getByTestId("terminal-pane")).toBeInTheDocument();
  });

  it("移动端软键盘可以通过 beforeinput 输入空格和逗号", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();

    const spaceEvent = dispatchMobileTextInput(terminalInput!, " ");
    const commaEvent = dispatchMobileTextInput(terminalInput!, ",");

    expect(spaceEvent.defaultPrevented).toBe(true);
    expect(commaEvent.defaultPrevented).toBe(true);
    await waitFor(() => expect(daemon.sessionDataMessages).toEqual([" ", ","]));
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
      const input = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
      expect(within(terminalPane).getByTestId("xterm-search-highlight")).toHaveTextContent("beta"),
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
    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
    expect(terminalInput).not.toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    expect(daemon.createdCommands).toEqual([[]]);
  });

  it("新建 session 后不输入内容也会刷新初始回显", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [],
      attachOutput: "idle-shell-prompt$ ",
    });
    (globalThis as { __TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__?: boolean })
      .__TERMD_TEST_DEFER_XTERM_RENDER_UNTIL_WRITE_CALLBACK__ = true;
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession("No session");

    await user.click(screen.getByRole("button", { name: "New session" }));

    const terminalInput = await waitFor(() => {
      const input = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(input).not.toBeNull();
      return input!;
    });
    await waitFor(() => expect(document.activeElement).toBe(terminalInput));
    await waitFor(() =>
      expect(document.querySelector<HTMLElement>(".xterm")?.textContent).toContain("idle-shell-prompt$ "),
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
      expect(daemon.sessionFileDownloadChunkRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
        offset_bytes: 0,
        max_bytes: 262144,
      });
    });
    expect(daemon.sessionFileReadRequests).toEqual([]);
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

    await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
    await waitFor(() => {
      expect(daemon.sessionFileDownloadChunkRequests.filter((request) => request.path === "/home/me/project/alpha.txt")).toHaveLength(2);
      expect(daemon.sessionFileDownloadChunkRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
        offset_bytes: 0,
        max_bytes: 262144,
      });
    });
    expect(daemon.sessionFileReadRequests).toEqual([]);

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

    await user.upload(
      within(panel).getByLabelText("Upload file"),
      new File(["uploaded web file\n"], "notes.txt", { type: "text/plain" }),
    );
    await waitFor(() => {
      expect(daemon.sessionFileWrites).toContainEqual({
        session_id: sessionId,
        path: "/tmp/notes.txt",
        text: "uploaded web file\n",
      });
    });
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
      expect(daemon.sessionFileDownloadChunkRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/README.md",
        offset_bytes: 0,
        max_bytes: expect.any(Number),
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

  it("文件 panel 默认每秒跟随终端 cwd，并可关闭跟随", async () => {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));
    const followToggle = within(panel).getByLabelText("Follow terminal cwd") as HTMLInputElement;
    expect(followToggle).toBeChecked();

    const requestCountBeforeCwdChange = daemon.sessionFileRequests.length;
    daemon.setSessionFilePosition(sessionId, "/tmp/work");
    await waitFor(
      () => {
        expect(daemon.sessionFileRequests.slice(requestCountBeforeCwdChange)).toContainEqual({
          session_id: sessionId,
        });
      },
      { timeout: 2500 },
    );
    await within(panel).findByText("beta.log");
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work");

    await user.click(followToggle);
    expect(followToggle).not.toBeChecked();
    const requestCountAfterDisable = daemon.sessionFileRequests.length;
    daemon.setSessionFilePosition(sessionId, "/home/me");
    await new Promise((resolve) => window.setTimeout(resolve, 1200));
    expect(daemon.sessionFileRequests).toHaveLength(requestCountAfterDisable);
    expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work");
  });

  it("重新 attach session 时恢复该 session 的文件树目录", async () => {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp/work");
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await within(panel).findByText("beta.log");

    await user.click(screen.getByRole("button", { name: "Disconnect" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Disconnect" })).toBeDisabled());
    await waitFor(() => expect(screen.getByText("No session")).toBeInTheDocument());
    expect(document.querySelector(".session-row.selected")).toBeNull();

    const requestCountBeforeReattach = daemon.sessionFileRequests.length;
    await clickSessionCard(user);

    await waitFor(() =>
      expect(daemon.sessionFileRequests.slice(requestCountBeforeReattach)).toContainEqual({
        session_id: sessionId,
      }),
    );
    await within(panel).findByText("beta.log");
  });

  it("接收 daemon 推送后同步当前 session 的文件树位置", async () => {
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
      },
    });
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    daemon.pushSessionFiles({
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
    });

    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/tmp/work"));
    await within(panel).findByText("beta.log");
  });

  it("显示 daemon 级客户端在线、离线和 attach 状态", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessions: [
        {
          session_id: "00000000-0000-0000-0000-000000000410",
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
          attached_session_ids: ["00000000-0000-0000-0000-000000000410"],
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    const operators = await screen.findByLabelText("session operators");
    await within(operators).findByText("192.0.2.41");
    await within(operators).findByText("12:8");
    await within(operators).findByText("focused");
    expect(within(operators).queryByText(/selecting/)).toBeNull();

    expect(screen.queryByLabelText("daemon clients")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Clients" }));

    const clientPanel = await screen.findByLabelText("daemon clients");
    await within(clientPanel).findByText("Clients");
    await within(clientPanel).findByText("192.0.2.41");
    await within(clientPanel).findByText("198.51.100.9");
    await within(clientPanel).findByText("online");
    await within(clientPanel).findByText("offline");
    await within(clientPanel).findByText("attached");
    await within(clientPanel).findByText("detached");

    const deleteOfflineClient = within(clientPanel).getByRole("button", { name: /Delete offline client/ });
    await user.dblClick(deleteOfflineClient);
    await waitFor(() => expect(within(clientPanel).queryByText("198.51.100.9")).toBeNull());
    expect(screen.queryByText("invalid_envelope")).toBeNull();
  });

  it("Session 卡片点击即打开，标题行保留管理按钮", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    const actions = await screen.findByLabelText("Session actions");

    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    expect(actions).toContainElement(screen.getByRole("button", { name: "Rename session" }));
    expect(actions).toContainElement(screen.getByRole("button", { name: "Close session" }));
  });

  it("左侧栏可折叠成图标栏，右侧文件 panel 可隐藏后再展开", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
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

    // outer wire 允许出现 encrypted_frame 的字段名，但不能出现 token 或终端/私钥明文值。
    for (const sensitive of [
      "wrong-token",
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
    await user.click(await screen.findByRole("button", { name: "Refresh" }));
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    await user.click(await screen.findByRole("button", { name: "Rename session" }));
    expect(screen.getByRole("button", { name: "Save session name" })).toBeDisabled();
    expect(daemon.sessionRenames).toEqual([]);
    daemon.queueSessionListResponse([], 30);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
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

  it("关闭已被 daemon 移除的 session 时按幂等删除处理", async () => {
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    const sessionId = "00000000-0000-0000-0000-000000000401";
    await waitForWorkspaceSession();
    daemon.forgetSession(sessionId);

    await user.click(screen.getByRole("button", { name: "Close session" }));

    await waitFor(() => {
      expect(screen.queryByText(sessionId)).toBeNull();
    });
    expect(screen.queryByRole("alert", { name: "Connection error" })).toBeNull();
    expect(daemon.closedSessions).toEqual([]);
  });

  it("shared-control attach 后持续发送终端输入、光标位置和聚焦状态", async () => {
    const user = userEvent.setup();
    const restoreViewerLayout = mockViewerLayout({
      viewportWidth: 600,
      viewportHeight: 420,
      frameWidth: 1200,
      frameHeight: 592,
    });
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
      await user.click(screen.getByRole("button", { name: "Refresh" }));
      await clickSessionCard(user);

      let terminalInput: HTMLTextAreaElement | null = null;
      await waitFor(() => {
        terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
        expect(terminalInput).not.toBeNull();
      });
      await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
      expect(screen.queryByLabelText("viewer controls")).toBeNull();
      expect(document.querySelector(".terminal-pane-viewer .terminal-viewer-frame")).toBeNull();
      expect(daemon.sessionResizes).toEqual([]);

      terminalInput!.focus();
      await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
      await waitFor(() =>
        expect(daemon.sessionCursorUpdates).toContainEqual({
          session_id: "00000000-0000-0000-0000-000000000402",
          row: expect.any(Number),
          col: expect.any(Number),
          focused: true,
        }),
      );
      await waitFor(() =>
        expect(daemon.sessionResizes).toContainEqual({
          session_id: "00000000-0000-0000-0000-000000000402",
          size: { rows: 24, cols: 80, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
        }),
      );
      terminalInput!.value = "first-terminal-secret";
      fireEvent.input(terminalInput!);

      await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret"]));

      terminalInput!.value = "second-terminal-secret";
      fireEvent.input(terminalInput!);

      await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret", "second-terminal-secret"]));
      expect(daemon.sessionCursorUpdates.length).toBeGreaterThan(0);
      terminalInput!.blur();
      await waitFor(() =>
        expect(daemon.sessionCursorUpdates).toContainEqual({
          session_id: "00000000-0000-0000-0000-000000000402",
          row: expect.any(Number),
          col: expect.any(Number),
          focused: false,
        }),
      );
      const resizeCountAfterBlur = daemon.sessionResizes.length;
      fireEvent(window, new Event("focus"));
      terminalInput!.focus();
      expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false");
      expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
      expect(screen.queryByLabelText("viewer controls")).toBeNull();
      expect(screen.queryByRole("button", { name: "Zoom out" })).toBeNull();
      fireEvent(window, new Event("resize"));
      expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
      expect(daemon.outerWireText()).not.toContain("first-terminal-secret");
      expect(daemon.outerWireText()).not.toContain("second-terminal-secret");
    } finally {
      restoreViewerLayout();
    }
  });

  it("移动端键盘上方快捷按钮会发送常用控制字符", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    setMobileVisualViewport(820, 460, 20);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => {
      expect(document.querySelector(".xterm-helper-textarea")).not.toBeNull();
    });

    await user.click(screen.getByRole("button", { name: "Send Tab" }));
    await user.click(screen.getByRole("button", { name: "Send Ctrl-C" }));
    await user.click(screen.getByRole("button", { name: "Send Ctrl-Z" }));

    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["\t", "\x03", "\x1a"]));
  });

  it("移动端长按终端一秒后拖动会发送方向键序列", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    await waitFor(() => {
      expect(document.querySelector(".xterm-helper-textarea")).not.toBeNull();
    });
    const viewerFrame = await waitFor(() => {
      const frame = document.querySelector<HTMLElement>(".terminal-viewer-frame");
      expect(frame).not.toBeNull();
      return frame!;
    });

    vi.useFakeTimers();
    try {
      fireTouchPointer(viewerFrame, "pointerdown", {
        pointerId: 11,
        clientX: 160,
        clientY: 240,
      });
      act(() => {
        vi.advanceTimersByTime(1000);
      });

      expect(screen.getByLabelText("mobile direction gesture")).toBeInTheDocument();
      fireTouchPointer(viewerFrame, "pointermove", {
        pointerId: 11,
        clientX: 160,
        clientY: 150,
      });
      fireTouchPointer(viewerFrame, "pointerup", {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    await waitFor(() => {
      expect(document.querySelector(".xterm-helper-textarea")).not.toBeNull();
    });
    await new Promise((resolve) => window.setTimeout(resolve, 80));

    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false");
    expect(screen.queryByLabelText("viewer controls")).toBeNull();
    expect(document.querySelector(".terminal-pane-viewer .terminal-viewer-frame")).toBeNull();
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
      expect(document.querySelector(".xterm-helper-textarea")).not.toBeNull();
    });
    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
    expect(screen.queryByLabelText("viewer controls")).toBeNull();
    expect(document.querySelector(".terminal-pane-viewer .terminal-viewer-frame")).toBeNull();
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();
    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000404",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );

    daemon.sessionCursorUpdates.length = 0;
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
    await waitFor(() =>
      expect(daemon.sessionResizes).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000404",
        size: { rows: 30, cols: 100, pixel_width: expect.any(Number), pixel_height: expect.any(Number) },
      }),
    );
    expect(daemon.sessionCursorUpdates.map((update) => update.focused)).not.toContain(false);
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
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

  it("浏览器窗口 resize 引发的短暂 focusout/focusin 不会上报聚焦抖动", async () => {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000405",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );

    daemon.sessionCursorUpdates.length = 0;
    fireEvent(window, new Event("resize"));
    // 真实浏览器在拖动窗口边界时可能短暂让 xterm textarea 失焦，随后又恢复焦点；
    // 这类 resize 伴随的瞬时 DOM focus 抖动不应变成 operator 的 focused/blurred 抖动。
    terminalInput!.blur();
    await new Promise((resolve) => window.setTimeout(resolve, 40));
    terminalInput!.focus();
    await new Promise((resolve) => window.setTimeout(resolve, 180));

    const focusUpdates = daemon.sessionCursorUpdates
      .filter((update) => update.session_id === "00000000-0000-0000-0000-000000000405")
      .map((update) => update.focused);
    expect(focusUpdates).not.toContain(false);
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false");
  });

  it("浏览器窗口失活后不再继续上报 PTY resize，也不把 resize owner 切成 viewer", async () => {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });
    terminalInput!.focus();
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000408",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );

    daemon.sessionCursorUpdates.length = 0;
    const resizeCountAfterFocus = daemon.sessionResizes.length;
    fireEvent(window, new Event("blur"));
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000408",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: false,
      }),
    );

    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));
    await new Promise((resolve) => window.setTimeout(resolve, 160));

    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterFocus);
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false");
    expect(screen.queryByLabelText("viewer controls")).toBeNull();
  });

  it("已有在线客户端时第二个客户端只显示 viewer zoom 且不发送 resize", async () => {
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
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await clickSessionCard(user);

    let firstTerminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      firstTerminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(firstTerminalInput).not.toBeNull();
    });
    firstTerminalInput!.focus();
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000409",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );
    fireEvent(window, new Event("blur"));
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000409",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: false,
      }),
    );

    const resizeCountAfterBlur = daemon.sessionResizes.length;
    const secondRender = render(<App />);
    await waitForWorkspaceSession();
    await user.click(screen.getAllByRole("button", { name: "Refresh" }).at(-1)!);
    await clickSessionCard(user, undefined, secondRender.container);

    await waitFor(() => {
      expect(secondRender.container.querySelector(".xterm-helper-textarea")).not.toBeNull();
    });
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    const secondTerminalFrame = secondRender.container.querySelector<HTMLElement>(".terminal-viewer-frame");
    expect(secondTerminalFrame).not.toBeNull();
    await user.click(secondTerminalFrame!);

    await waitFor(() =>
      expect(secondRender.container.querySelector("[data-testid='terminal-pane']")).toHaveAttribute(
        "data-viewer-mode",
        "true",
      ),
    );
    await new Promise((resolve) => window.setTimeout(resolve, 160));

    const laterResizes = daemon.sessionResizes.slice(resizeCountAfterBlur);
    expect(laterResizes).toEqual([]);
    expect(within(secondRender.container).getByLabelText("viewer controls")).toBeVisible();
    expect(within(secondRender.container).getByRole("button", { name: "Zoom in" })).toBeVisible();

    secondRender.unmount();
    firstRender.unmount();
  });

  it("resize owner 失焦后窗口 resize 不显示 viewer", async () => {
    const user = userEvent.setup();
    const restoreViewerLayout = mockViewerLayout({
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
      await user.click(screen.getByRole("button", { name: "Refresh" }));
      await clickSessionCard(user);

      let terminalInput: HTMLTextAreaElement | null = null;
      await waitFor(() => {
        terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
        expect(terminalInput).not.toBeNull();
      });
      (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
        rows: 30,
        cols: 100,
      };
      const resizeCountBeforeWindowResize = daemon.sessionResizes.length;
      fireEvent(window, new Event("resize"));
      await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false"));
      expect(screen.queryByLabelText("viewer controls")).toBeNull();
      expect(document.querySelector(".terminal-pane-viewer .terminal-viewer-frame")).toBeNull();
      expect(daemon.sessionResizes).toHaveLength(resizeCountBeforeWindowResize);
    } finally {
      restoreViewerLayout();
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
      "wss://relay.example/termd/ws?relay_token=abc",
    ]);
    expect(pairingWsUrlCandidates("wss://relay.example/termd/ws/00000000-0000-0000-0000-000000000123/client", serverId, relayPage)).toEqual([
      "wss://relay.example/termd/ws",
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
      "ws://192.168.55.155:8765/termd/ws?relay_token=abc",
      "wss://relay.example/ws?relay_token=abc",
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
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
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });
});
