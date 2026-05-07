import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "../App";
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

    await waitFor(() => expect(screen.getByLabelText("Pairing token")).toHaveValue(""));
    await screen.findByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await screen.findAllByText("00000000-0000-0000-0000-000000000401");
    await user.click(screen.getByRole("button", { name: "Attach" }));

    await screen.findAllByText("controller");
    await screen.findByText(/termd-e2e-ready/);
    expect(daemon.outerWireText()).not.toContain("secret-token");
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

    await waitFor(() => expect(screen.getByLabelText("Pairing token")).toHaveValue(""));
    expect(await screen.findByLabelText("WS URL")).toHaveValue(daemon.url);
    await screen.findByText(daemon.serverId);
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

  it("control 按钮等待 daemon grant 后才更新角色", async () => {
    daemon.nextAttachRole = "viewer";
    const user = userEvent.setup();
    render(<App />);

    await user.clear(await screen.findByLabelText("WS URL"));
    await user.type(screen.getByLabelText("WS URL"), daemon.url);
    await user.type(screen.getByLabelText("Pairing token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Pair" }));
    await screen.findByText(daemon.serverId);
    await user.click(await screen.findByRole("button", { name: "Refresh" }));
    await user.click(await screen.findByRole("button", { name: "Attach" }));

    await screen.findAllByText("viewer");
    await user.click(screen.getByRole("button", { name: "Steal control" }));
    await screen.findAllByText("controller");
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
    await screen.findByText(daemon.serverId);
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await user.click(await screen.findByRole("button", { name: "Attach" }));
    await screen.findAllByText("controller");

    const terminalInput = document.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
    expect(terminalInput).not.toBeNull();

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
});
