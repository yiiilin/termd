import { expect, test, type Page, type TestInfo } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

async function activateButton(page: Page, name: string): Promise<void> {
  const button = page.getByRole("button", { name });
  await expect(button).toBeVisible();
  await expect(button).toBeEnabled();
  await button.focus();
  await expect(button).toBeFocused();
  await page.keyboard.press("Enter");
}

async function openMobileMenu(page: Page) {
  await activateButton(page, "Open mobile workspace menu");
  const menu = page.getByRole("navigation", { name: "mobile workspace menu" });
  await expect(menu).toBeVisible();
  return menu;
}

test("pair、list、attach 的浏览器 smoke", async ({ page }, testInfo: TestInfo) => {
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-000000000501",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "termd-e2e-ready\n",
  });

  try {
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Scan QR" })).toBeVisible();
    await page.getByRole("button", { name: "Scan QR" }).click();
    await expect(page.getByRole("dialog", { name: "Scan pairing QR" })).toBeVisible();
    await page.getByRole("button", { name: "Close scanner" }).click();
    await expect(page.getByRole("dialog", { name: "Scan pairing QR" })).toBeHidden();

    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await expect(page.getByLabel("Pairing token")).toHaveValue(/termd-pair:v1:/);
    await activateButton(page, "Pair");

    await expect(page.getByLabel("Pairing token")).toBeHidden();
    const terminalPane = page.getByTestId("terminal-pane");
    await expect(terminalPane.getByText("termd-e2e-ready")).toBeVisible();

    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toHaveCount(0);
      const menu = await openMobileMenu(page);
      await expect(menu.getByRole("button", { name: "Daemons" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Sessions" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Files" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "New" })).toBeVisible();
      await expect(menu.getByRole("button", { name: /Refresh/ })).toHaveCount(0);
      await menu.getByRole("button", { name: "Daemons" }).click();
      await expect(page.getByRole("main", { name: "daemon admin" })).toBeVisible();
      await expect(page.getByRole("region", { name: "connection" })).toBeVisible();
      await expect(page.getByLabel("daemon manager")).toBeVisible();
      await activateButton(page, "Open workspace");

      const reopenedMenu = await openMobileMenu(page);
      await reopenedMenu.getByRole("button", { name: "Sessions" }).click();
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeVisible();
      await activateButton(page, "Refresh sessions");

      // 回归断言：移动端顶部入口必须留在左侧，避免刷新后被布局规则顶到右边。
      const mobileMenuButton = page.getByRole("button", { name: "Open mobile workspace menu" });
      const menuBox = await mobileMenuButton.boundingBox();
      expect(menuBox?.x ?? 0).toBeLessThan(48);
    } else {
      const daemonStatus = page.getByRole("contentinfo", { name: "daemon server status" });
      await expect(daemonStatus).toBeVisible();
      await expect(daemonStatus.getByText("CPU")).toBeVisible();
      await expect(daemonStatus.getByRole("button", { name: "Refresh server status" })).toHaveCount(0);
    }
    const sessionsPanel = page.getByRole("region", { name: "sessions" });
    // session UUID 已从 UI 隐藏；测试按用户实际看到的可访问名称打开会话。
    const sessionRow = sessionsPanel.getByRole("button", { name: "Open Lagrange" });
    await expect(sessionRow).toBeVisible();

    await sessionRow.click();
    await expect(terminalPane.getByText("termd-e2e-ready")).toBeVisible();

    if (testInfo.project.name !== "mobile-chrome") {
      daemon.pushSessionData(
        "00000000-0000-0000-0000-000000000501",
        Array.from({ length: 96 }, (_, index) => `resize-scroll-bottom-${index}\n`).join(""),
      );
      await expect(terminalPane.getByText("resize-scroll-bottom-95")).toBeVisible();
      const resizer = page.getByRole("separator", { name: "Resize files panel" });
      const box = await resizer.boundingBox();
      expect(box).not.toBeNull();
      await page.mouse.move((box?.x ?? 0) + (box?.width ?? 1) / 2, (box?.y ?? 0) + 20);
      await page.mouse.down();
      await page.mouse.move((box?.x ?? 0) - 120, (box?.y ?? 0) + 20);
      await page.mouse.up();
      await terminalPane.click();
      await expect(terminalPane.getByText("resize-scroll-bottom-95")).toBeVisible();
      await expect
        .poll(async () =>
          page.locator(".terminal-scrollport").evaluate((element) => {
            const maxScrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
            return element.scrollTop >= maxScrollTop - 2;
          }),
        )
        .toBe(true);
    }

    if (testInfo.project.name === "mobile-chrome") {
      await expect
        .poll(async () => (await terminalPane.boundingBox())?.height ?? 0)
        .toBeGreaterThan(280);
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeHidden();
      const menu = await openMobileMenu(page);
      const files = menu.getByRole("button", { name: "Files" });
      await expect(files).toBeEnabled();
      await files.click();
      const filesPanel = page.getByLabel("session files");
      await expect(filesPanel).toBeVisible();
      await expect.poll(async () => (await filesPanel.boundingBox())?.height ?? 0).toBeGreaterThan(280);
      await activateButton(page, "Hide files panel");
      await expect(filesPanel).toBeHidden();
      await page.screenshot({ path: "test-results/mobile-termui-smoke.png", fullPage: true });
    }

    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    if (testInfo.project.name === "mobile-chrome") {
      await expect(terminalPane).toHaveAttribute("data-viewer-mode", "false");
      await expect
        .poll(async () => (await terminalPane.boundingBox())?.height ?? 0)
        .toBeGreaterThan(280);
    }
    await page.keyboard.type("terminal-secret");
    await page.keyboard.press("Enter");
    await expect
      .poll(() => daemon.decryptedInputs.join(""))
      .toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("secret-token");

    if (testInfo.project.name === "mobile-chrome") {
      await page.evaluate(() => {
        // 移动端软键盘收起或输入框 blur 后，终端不能被同步成一行或丢失可视高度。
        if (document.activeElement instanceof HTMLElement) {
          document.activeElement.blur();
        }
      });
      await expect(terminalPane).toHaveAttribute("data-viewer-mode", "false");
      await expect
        .poll(async () => (await terminalPane.boundingBox())?.height ?? 0)
        .toBeGreaterThan(280);
    }

    await page.reload();
    await expect(page.getByTestId("terminal-pane").getByText("termd-e2e-ready")).toBeVisible();
    if (testInfo.project.name === "mobile-chrome") {
      await activateButton(page, "Open mobile workspace menu");
      const menu = page.getByRole("navigation", { name: "mobile workspace menu" });
      await expect(menu).toBeVisible();
      await menu.getByRole("button", { name: "Daemons" }).click();
      await expect(page.getByRole("main", { name: "daemon admin" })).toBeVisible();
      await expect(page.getByRole("button", { name: "Open workspace" })).toBeEnabled();
    }
    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain("secret-token");
  } finally {
    await daemon.stop();
  }
});

test("direct Web 慢普通 RPC 超时后终端仍可输入", async ({ page }, testInfo: TestInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "差网络 direct 回归只需要桌面布局覆盖");
  test.setTimeout(25_000);
  const sessionId = "00000000-0000-0000-0000-000000000511";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: sessionId,
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "direct-slow-ready\n",
    daemonStatusDelayMs: 5_600,
    sessionFilesDelayMs: 5_600,
    sessionFiles: {
      [sessionId]: {
        session_id: sessionId,
        path: "/slow/files",
        entries: [],
      },
    },
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    const terminalPane = page.getByTestId("terminal-pane");
    await expect(terminalPane.getByText("direct-slow-ready")).toBeVisible();
    // 中文注释：files/status 都是非终端 segment；超过普通 UI deadline 后，
    // 页面应只把对应 panel 标成不可用，terminal stream 仍保持可输入。
    await expect(page.getByLabel("session files").getByText("unavailable")).toBeVisible({ timeout: 8_000 });
    await expect(page.getByRole("alert", { name: "Connection error" })).toHaveCount(0);

    await terminalPane.click();
    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    await page.keyboard.type("direct-after-timeout");
    await page.keyboard.press("Enter");
    await expect.poll(() => daemon.decryptedInputs.join("")).toContain("direct-after-timeout");
    expect(daemon.activeConnectionCount()).toBe(1);
  } finally {
    await daemon.stop();
  }
});

function pairingInviteCode(daemon: MockDaemon): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 1,
    token: "secret-token",
    server_id: daemon.serverId,
    daemon_public_key: daemon.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}
