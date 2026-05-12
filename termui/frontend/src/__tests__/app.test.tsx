import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, {
  browserReachableWsUrl,
  connectPairingClient,
  defaultWsUrlFromPage,
  pairingWsUrlCandidates,
} from "../App";
import type { SessionFilesResultPayload } from "../protocol/types";
import { clearBrowserState } from "../state/browser-state";
import { MockDaemon } from "../test/mock-daemon";
import { fallbackSessionDisplayName } from "../session-names";

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
    await user.click(await within(scope as HTMLElement).findByRole("button", { name: `Open ${name}` }));
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

describe("termui web 工作台", () => {
  let daemon: MockDaemon;

  beforeEach(async () => {
    await clearBrowserState();
    setViewportWidth(1366);
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
    await daemon.stop();
  });

  it("pairing 后清空 token，刷新 session list，并 attach 到 terminal", async () => {
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
    await clickSessionCard(user);

    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]));
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    expect(daemon.pingMessages).toBe(0);
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });

  it("移动端顶部菜单保持 terminal-first，并把 daemon 管理放到独立后台页", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
    render(<App />);

    await pairWithInvite(user, daemon);
    await waitForWorkspaceSession();
    expect(screen.queryByRole("navigation", { name: "mobile workspace menu" })).toBeNull();
    expect(screen.queryByRole("navigation", { name: "mobile workspace actions" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Clients" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Daemons" })).toBeNull();

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const menu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    expect(within(menu).getByRole("button", { name: "Daemons" })).toBeEnabled();
    expect(within(menu).getByRole("button", { name: "Sessions" })).toBeEnabled();
    expect(within(menu).getByRole("button", { name: "Files" })).toBeDisabled();
    expect(within(menu).getByRole("button", { name: "New" })).toBeEnabled();
    expect(within(menu).queryByRole("button", { name: "Refresh sessions" })).toBeNull();

    await within(menu).getByRole("button", { name: "Daemons" }).click();
    const admin = await screen.findByLabelText("daemon admin");
    expect(within(admin).getByLabelText("daemon manager")).toBeVisible();
    await user.click(within(admin).getByRole("button", { name: "Open workspace" }));
    await waitForWorkspaceSession();

    await user.click(screen.getByRole("button", { name: "Open mobile workspace menu" }));
    const sessionsMenu = await screen.findByRole("navigation", { name: "mobile workspace menu" });
    await within(sessionsMenu).getByRole("button", { name: "Sessions" }).click();
    const sessionsPanel = await screen.findByLabelText("sessions panel");
    const refreshSessions = within(sessionsPanel).getByRole("button", { name: "Refresh sessions" });
    await expect(refreshSessions).toBeEnabled();
    await user.click(refreshSessions);
    await clickSessionCard(user, DEFAULT_SESSION_NAME, sessionsPanel);

    await screen.findByText(/termd-e2e-ready/);
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
    expect(await screen.findByText("protocol_error: protocol operation failed")).toBeInTheDocument();

    const renderedText = document.body.textContent ?? "";
    for (const sensitive of ["invalid_envelope_token", "private_key", "private-value", "signature", "ciphertext_base64"]) {
      expect(renderedText).not.toContain(sensitive);
    }
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

    await within(panel).findByText("src");
    await within(panel).findByText("alpha.txt");
    expect(within(panel).getByText("12 B")).toBeInTheDocument();
    expect(daemon.sessionFileRequests).toEqual([{ session_id: sessionId }]);

    await user.click(within(panel).getByRole("button", { name: "Open src" }));
    await within(panel).findByText("main.rs");
    expect(daemon.sessionFileRequests).toContainEqual({ session_id: sessionId, path: srcPath });

    await user.click(within(panel).getByRole("button", { name: "Parent directory" }));
    await within(panel).findByText("alpha.txt");

    await user.click(within(panel).getByRole("button", { name: "Download alpha.txt" }));
    await waitFor(() => {
      expect(daemon.sessionFileReadRequests).toContainEqual({
        session_id: sessionId,
        path: "/home/me/project/alpha.txt",
      });
    });

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
    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true"));
    await within(screen.getByLabelText("viewer controls")).findByText("100x30");
    const viewerFrame = document.querySelector<HTMLElement>(".terminal-pane-viewer .terminal-viewer-frame");
    expect(viewerFrame).not.toBeNull();
    expect(viewerFrame?.style.width).toBe("calc(100ch + 26px)");
    expect(viewerFrame?.style.height).toBe("592px");
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000402",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: false,
      }),
    );
    expect(daemon.sessionResizes).toEqual([]);
    await user.click(screen.getByRole("button", { name: "Zoom out" }));
    await screen.findByText("90%");
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
    await user.click(screen.getByRole("button", { name: "Fit" }));
    await screen.findByText("100%");
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");

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
    const viewerCanvas = document.querySelector<HTMLElement>(".terminal-viewer-canvas");
    expect(viewerCanvas).not.toBeNull();
    await user.click(viewerCanvas!);
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "false");
    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
    expect(screen.queryByLabelText("viewer controls")).toBeNull();
    expect(screen.queryByRole("button", { name: "Zoom out" })).toBeNull();
    fireEvent(window, new Event("resize"));
    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
    expect(daemon.outerWireText()).not.toContain("first-terminal-secret");
    expect(daemon.outerWireText()).not.toContain("second-terminal-secret");
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

  it("聚焦终端遇到浏览器窗口 resize 后退回非聚焦 viewer 状态", async () => {
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

    const resizeCountBeforeWindowResize = daemon.sessionResizes.length;
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 30,
      cols: 100,
    };
    fireEvent(window, new Event("resize"));

    await waitFor(() => expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true"));
    // 进入 viewer 后，xterm host 会被远端 PTY frame 框住；如果这时把该 host 再拿来测
    // 本地可容纳尺寸，就会误判为“分辨率一致”，导致 viewer true/false 来回振荡。
    (globalThis as { __TERMD_TEST_FIT_DIMENSIONS__?: { rows: number; cols: number } }).__TERMD_TEST_FIT_DIMENSIONS__ = {
      rows: 24,
      cols: 80,
    };
    fireEvent(window, new Event("resize"));
    await new Promise((resolve) => window.setTimeout(resolve, 80));
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
    expect(daemon.sessionResizes).toHaveLength(resizeCountBeforeWindowResize);
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000404",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: false,
      }),
    );
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
