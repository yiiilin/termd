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
    await page.getByLabel("Pairing token").fill("secret-token");
    await expect(page.getByLabel("Pairing token")).toHaveValue("secret-token");
    await activateButton(page, "Pair");

    await expect(page.getByLabel("Pairing token")).toBeHidden();
    const connectionStatus = page.getByLabel("connection status");
    await expect(connectionStatus.getByText(daemon.url)).toBeVisible();

    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toBeVisible();
      await activateButton(page, "Open sessions");
    }

    await activateButton(page, "Refresh");
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
      await expect(sessionsPanel).toBeHidden();
      await activateButton(page, "Open files");
      await expect(page.getByLabel("session files")).toBeVisible();
      await activateButton(page, "Hide files panel");
      await expect(page.getByLabel("session files")).toBeHidden();
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
    await expect(page.getByLabel("connection status").getByText(daemon.url)).toBeVisible();
    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain("secret-token");
  } finally {
    await daemon.stop();
  }
});
