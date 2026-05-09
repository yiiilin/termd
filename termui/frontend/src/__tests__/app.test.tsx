import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, { browserReachableWsUrl, defaultWsUrlFromPage } from "../App";
import type { SessionFilesResultPayload } from "../protocol/types";
import { clearBrowserState } from "../state/browser-state";
import { MockDaemon } from "../test/mock-daemon";

describe("termui web 工作台", () => {
  let daemon: MockDaemon;

  beforeEach(async () => {
    await clearBrowserState();
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
    await waitFor(() => expect(within(panel).getByLabelText("Current directory")).toHaveValue(rootPath));

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

    await user.clear(await screen.findByLabelText("WS URL"));
    fireEvent.change(screen.getByLabelText("Pairing token"), { target: { value: payload } });
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
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000402",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: false,
      }),
    );

    terminalInput!.focus();
    await waitFor(() =>
      expect(daemon.sessionCursorUpdates).toContainEqual({
        session_id: "00000000-0000-0000-0000-000000000402",
        row: expect.any(Number),
        col: expect.any(Number),
        focused: true,
      }),
    );
    terminalInput!.value = "first-terminal-secret";
    fireEvent.input(terminalInput!);

    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret"]));

    terminalInput!.value = "second-terminal-secret";
    fireEvent.input(terminalInput!);

    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret", "second-terminal-secret"]));
    expect(daemon.sessionCursorUpdates.length).toBeGreaterThan(0);
    expect(daemon.outerWireText()).not.toContain("first-terminal-secret");
    expect(daemon.outerWireText()).not.toContain("second-terminal-secret");
  });

  it("未配对时只显示连接表单，并按当前页面来源推导 WebSocket 地址", async () => {
    render(<App />);

    await screen.findByLabelText("WS URL");
    expect(screen.getByLabelText("Pairing token")).toBeInTheDocument();
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
});
