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

    await activateButton(page, "Edit address");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await expect(page.getByLabel("Pairing token")).toHaveValue(/termd-pair:v1:/);
    await activateButton(page, "Pair");

    await expect(page.getByLabel("Pairing token")).toBeHidden();
    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toHaveCount(0);
      const menu = await openMobileMenu(page);
      await expect(menu.getByRole("button", { name: "Connection" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Sessions" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Files" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "New" })).toBeVisible();
      await expect(menu.getByRole("button", { name: /Refresh/ })).toHaveCount(0);
      await menu.getByRole("button", { name: "Connection" }).click();
      const connectionPanel = await page.getByRole("region", { name: "connection panel" });
      await expect(connectionPanel.getByLabel("connection status")).toBeVisible();
      await activateButton(page, "Manage daemons");
      await expect(page.getByLabel("daemon manager")).toBeVisible();
      await activateButton(page, "Close connection panel");

      const reopenedMenu = await openMobileMenu(page);
      await reopenedMenu.getByRole("button", { name: "Sessions" }).click();
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeVisible();
      await activateButton(page, "Refresh sessions");

      // 回归断言：移动端顶部菜单按钮和连接状态必须留在左侧，避免刷新后被挤到右上角。
      const mobileMenuButton = page.getByRole("button", { name: "Open mobile workspace menu" });
      const menuBox = await mobileMenuButton.boundingBox();
      expect(menuBox?.x ?? 0).toBeLessThan(48);

      const daemonStatus = page.getByText("paired daemon");
      const daemonBox = await daemonStatus.boundingBox();
      expect(daemonBox?.x ?? 0).toBeLessThan(180);
    } else {
      const connectionStatus = page.getByLabel("connection status");
      await expect(connectionStatus.getByText(daemon.url)).toBeVisible();
      await activateButton(page, "Refresh");
    }
    const sessionsPanel = page.getByRole("region", { name: "sessions" });
    const sessionRow = sessionsPanel.getByText("00000000-0000-0000-0000-000000000501");
    await expect(sessionRow).toBeVisible();

    await sessionRow.click();
    const terminalPane = page.getByTestId("terminal-pane");
    await expect(terminalPane).toHaveAttribute("data-output-chunks", "1");
    await expect(terminalPane.getByText("termd-e2e-ready")).toBeVisible();

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
        // 移动端失焦后会回到 viewer 面板；高度不能被刚才的输入态同步成一行。
        if (document.activeElement instanceof HTMLElement) {
          document.activeElement.blur();
        }
      });
      await expect(terminalPane).toHaveAttribute("data-viewer-mode", "true");
      await expect
        .poll(async () => (await terminalPane.boundingBox())?.height ?? 0)
        .toBeGreaterThan(280);
    }

    await page.reload();
    if (testInfo.project.name === "mobile-chrome") {
      await activateButton(page, "Open mobile workspace menu");
      const menu = page.getByRole("navigation", { name: "mobile workspace menu" });
      await expect(menu).toBeVisible();
      await menu.getByRole("button", { name: "Connection" }).click();
      const connectionPanel = page.getByRole("region", { name: "connection panel" });
      await expect(connectionPanel.getByLabel("connection status").getByText(daemon.url)).toBeVisible();
    } else {
      await expect(page.getByLabel("connection status").getByText(daemon.url)).toBeVisible();
    }
    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain("secret-token");
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
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}
