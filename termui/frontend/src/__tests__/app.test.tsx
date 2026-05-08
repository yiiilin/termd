import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App, { browserReachableWsUrl, defaultWsUrlFromPage } from "../App";
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
    await user.click(screen.getByRole("button", { name: "Attach" }));

    await screen.findAllByText("controller");
    expect(await screen.findByRole("button", { name: "Attached" })).toBeDisabled();
    await screen.findByText(/termd-e2e-ready/);
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    expect(daemon.pingMessages).toBe(0);
    expect(daemon.outerWireText()).not.toContain("secret-token");
  });

  it("点击 session 先进入 viewer，点击 Attach 后才成为 controller", async () => {
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

    await screen.findAllByText("viewer");
    await screen.findByText(/termd-e2e-ready/);
    expect(daemon.attachIntents).toEqual(["viewer"]);

    await user.click(screen.getByRole("button", { name: "Attach" }));

    await screen.findAllByText("controller");
    expect(daemon.attachIntents).toEqual(["viewer"]);
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
    await screen.findAllByText("controller");
    await screen.findByText(/web-session-ready/);
    expect(daemon.createdCommands).toEqual([[]]);
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
    await user.click(await screen.findByRole("button", { name: "Attach" }));

    expect(screen.queryByLabelText("daemon clients")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Clients" }));

    await screen.findByText("Clients");
    await screen.findByText("192.0.2.41");
    await screen.findByText("198.51.100.9");
    await screen.findByText("online");
    await screen.findByText("offline");
    await screen.findByText("attached 00000000");
    await screen.findByText("detached");
  });

  it("Session 卡片把所有操作按钮固定在底部操作区", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));

    const actions = await screen.findByLabelText("Session actions");

    expect(actions).toContainElement(screen.getByRole("button", { name: "Attach" }));
    expect(actions).toContainElement(screen.getByRole("button", { name: "Rename session" }));
    expect(actions).toContainElement(screen.getByRole("button", { name: "Close session" }));
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

  it("Take control 按钮等待 daemon grant 后才更新角色，并且工具栏不重复显示角色", async () => {
    daemon.nextAttachRole = "viewer";
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findAllByText(daemon.serverId);
    await user.click(await screen.findByRole("button", { name: "Refresh" }));
    await user.click(await screen.findByRole("button", { name: "Attach" }));

    await screen.findByText("viewer");
    expect(screen.queryByRole("button", { name: "Steal control" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Take control" }));
    await screen.findByText("controller");
    expect(screen.queryByRole("button", { name: "Take control" })).toBeNull();
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

  it("收到 controller_required 后降为 viewer，并停止发送后续终端输入", async () => {
    const user = userEvent.setup();
    await daemon.stop();
    daemon = await MockDaemon.start({
      token: "secret-token",
      sessionDataError: { code: "controller_required", message: "controller required" },
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
    await user.click(await screen.findByRole("button", { name: "Attach" }));
    await screen.findAllByText("controller");

    let terminalInput: HTMLTextAreaElement | null = null;
    await waitFor(() => {
      terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
      expect(terminalInput).not.toBeNull();
    });

    // 第一次输入模拟 daemon 发现当前连接已不再持有控制权，并返回 controller_required。
    terminalInput!.value = "first-terminal-secret";
    fireEvent.input(terminalInput!);

    await waitFor(() => expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret"]));
    await screen.findAllByText("viewer");

    terminalInput!.value = "second-terminal-secret";
    fireEvent.input(terminalInput!);

    // 给 WebSocket message 队列一个调度周期；若 UI 仍发送 session_data，mock daemon 会记录到这里。
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    expect(daemon.sessionDataMessages).toEqual(["first-terminal-secret"]);
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
