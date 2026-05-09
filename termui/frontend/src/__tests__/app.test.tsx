import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, { browserReachableWsUrl, defaultWsUrlFromPage } from "../App";
import type { SessionFilesResultPayload } from "../protocol/types";
import { clearBrowserState } from "../state/browser-state";
import { MockDaemon } from "../test/mock-daemon";

const qrScannerMock = vi.hoisted(() => ({
  destroy: vi.fn(),
  hasCamera: vi.fn<() => Promise<boolean>>(() => Promise.resolve(true)),
  onDecode: undefined as ((result: { data: string }) => void) | undefined,
  start: vi.fn<() => Promise<void>>(() => Promise.resolve()),
  stop: vi.fn(),
}));

vi.mock("qr-scanner", () => {
  class MockQrScanner {
    static NO_QR_CODE_FOUND = "No QR code found";
    static hasCamera = qrScannerMock.hasCamera;

    constructor(_video: HTMLVideoElement, onDecode: (result: { data: string }) => void) {
      qrScannerMock.onDecode = onDecode;
    }

    start = qrScannerMock.start;
    stop = qrScannerMock.stop;
    destroy = qrScannerMock.destroy;
  }

  return { default: MockQrScanner };
});

describe("termui web 工作台", () => {
  let daemon: MockDaemon;

  beforeEach(async () => {
    await clearBrowserState();
    qrScannerMock.destroy.mockClear();
    qrScannerMock.hasCamera.mockReset();
    qrScannerMock.hasCamera.mockResolvedValue(true);
    qrScannerMock.onDecode = undefined;
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await screen.findAllByText("00000000-0000-0000-0000-000000000401");
    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    await user.click(screen.getAllByText("00000000-0000-0000-0000-000000000401")[0]);

    await screen.findByText(/termd-e2e-ready/);
    await waitFor(() => expect(daemon.attachedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]));
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    expect(daemon.pingMessages).toBe(0);
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });

  it("已配对后可以把连接地址改成 relay client URL", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);

    const relayUrl = `${daemon.url.replace("/ws", `/ws/${daemon.serverId}/client`)}?relay_token=relay-secret`;
    await user.click(screen.getByRole("button", { name: "Edit connection" }));
    await user.clear(screen.getByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), relayUrl);
    await user.click(screen.getByRole("button", { name: "Save URL" }));

    await screen.findByText(relayUrl);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await screen.findAllByText("00000000-0000-0000-0000-000000000401");
  });

  it("点击 session 卡片直接进入 shared-control operator", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    await screen.findAllByText("00000000-0000-0000-0000-000000000401");
    await user.click(screen.getAllByText("00000000-0000-0000-0000-000000000401")[0]);

    await screen.findByText(/termd-e2e-ready/);
    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Attached" })).toBeNull();
    expect(daemon.attachedSessions).toEqual(["00000000-0000-0000-0000-000000000401"]);
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);

    expect(screen.queryByLabelText("Command")).toBeNull();
    await user.click(screen.getByRole("button", { name: "New session" }));

    await screen.findAllByText("00000000-0000-0000-0000-000000000501");
    expect(screen.queryByRole("button", { name: "Attached" })).toBeNull();
    await screen.findByText(/web-session-ready/);
    expect(daemon.createdCommands).toEqual([[]]);
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText(sessionId))[0]);

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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText(sessionId))[0]);

    const panel = await screen.findByLabelText("session files");
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue("/home/me"));

    await user.clear(within(panel).getByLabelText("Current directory"));
    await user.type(within(panel).getByLabelText("Current directory"), "/tmp/work");
    await user.click(within(panel).getByRole("button", { name: "Go" }));
    await within(panel).findByText("beta.log");

    await user.click(screen.getByRole("button", { name: "Disconnect" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Disconnect" })).toBeDisabled());

    const requestCountBeforeReattach = daemon.sessionFileRequests.length;
    await user.click(screen.getAllByText(sessionId)[0]);

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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText(sessionId))[0]);

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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText("00000000-0000-0000-0000-000000000410"))[0]);

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
    await within(clientPanel).findByText("attached 00000000");
    await within(clientPanel).findByText("detached");
  });

  it("Session 卡片点击即打开，底部只保留管理按钮", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    const actions = await screen.findByLabelText("Session actions");

    expect(screen.queryByRole("button", { name: "Open" })).toBeNull();
    expect(actions).toContainElement(screen.getByRole("button", { name: "Rename session" }));
    expect(actions).toContainElement(screen.getByRole("button", { name: "Close session" }));
  });

  it("左侧栏可折叠成图标栏，右侧文件 panel 可隐藏后再展开", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText("00000000-0000-0000-0000-000000000401"))[0]);

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

  it("粘贴 QR payload 后会切换到 payload 内的 ws_url 和 token", async () => {
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

    await user.clear(await screen.findByLabelText("WS URL"));
    fireEvent.change(screen.getByLabelText("Pairing token"), { target: { value: inviteCode } });
    await user.click(screen.getByRole("button", { name: "Pair" }));

    await waitFor(() => expect(screen.queryByLabelText("Pairing token")).toBeNull());
    expect(screen.queryByLabelText("WS URL")).toBeNull();
    await screen.findByText(daemon.url);
    await screen.findAllByText(daemon.serverId);
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "wrong-token");
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(await screen.findByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText("00000000-0000-0000-0000-000000000401"))[0]);

    expect(screen.queryByRole("button", { name: "Take control" })).toBeNull();
    expect(document.body.textContent).not.toContain("viewer");
    expect(document.body.textContent).not.toContain("controller");
  });

  it("可以在 Session 列表重命名和关闭 session", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    await user.click(await screen.findByRole("button", { name: "Rename session" }));
    await user.clear(screen.getByLabelText("Session name"));
    await user.type(screen.getByLabelText("Session name"), "work shell");
    await user.click(screen.getByRole("button", { name: "Save session name" }));

    await screen.findByText("work shell");
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

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click((await screen.findAllByText("00000000-0000-0000-0000-000000000402"))[0]);

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
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
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
    await screen.findByRole("button", { name: "Zoom out" });
    await user.click(screen.getByRole("button", { name: "Zoom out" }));
    await screen.findByText("90%");
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
    await user.click(screen.getByRole("button", { name: "Fit" }));
    await screen.findByText("100%");
    expect(screen.getByTestId("terminal-pane")).toHaveAttribute("data-viewer-mode", "true");
    fireEvent(window, new Event("resize"));
    expect(daemon.sessionResizes).toHaveLength(resizeCountAfterBlur);
    expect(daemon.outerWireText()).not.toContain("first-terminal-secret");
    expect(daemon.outerWireText()).not.toContain("second-terminal-secret");
  });

  it("未配对时只显示连接表单，并按当前页面来源推导 WebSocket 地址", async () => {
    render(<App />);

    await screen.findByLabelText("WS URL");
    expect(screen.getByLabelText("Pairing token")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Scan QR" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New session" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
    expect(defaultWsUrlFromPage({ protocol: "http:", host: "192.168.55.155:8765" })).toBe(
      "ws://192.168.55.155:8765/ws",
    );
    expect(defaultWsUrlFromPage({ protocol: "https:", host: "example.test" })).toBe("wss://example.test/ws");
    expect(
      browserReachableWsUrl("ws://127.0.0.1:8765/ws", {
        protocol: "http:",
        host: "192.168.55.155:8765",
        hostname: "192.168.55.155",
      }),
    ).toBe("ws://192.168.55.155:8765/ws");
  });

  it("点击 Scan QR 后打开扫码 pairing 界面入口", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "Scan QR" }));

    expect(await screen.findByRole("dialog", { name: "Scan pairing QR" })).toBeInTheDocument();
    await waitFor(() => expect(qrScannerMock.start).toHaveBeenCalledTimes(1));
    await screen.findByText("Scanning");
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
    await screen.findAllByText(daemon.serverId);
    await screen.findByText(daemon.url);
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
    await screen.findByText(/pairing_payload_server_mismatch/);
    expect(screen.getByLabelText("Pairing token")).toHaveValue("");
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });
});
