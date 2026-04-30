import { expect, test, type Page } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

async function activateButton(page: Page, name: string): Promise<void> {
  const button = page.getByRole("button", { name });
  await expect(button).toBeVisible();
  await expect(button).toBeEnabled();
  await button.focus();
  await expect(button).toBeFocused();
  await page.keyboard.press("Enter");
}

test("pair、list、attach、control 的浏览器 smoke", async ({ page }) => {
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
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill("secret-token");
    await expect(page.getByLabel("Pairing token")).toHaveValue("secret-token");
    await activateButton(page, "Pair");

    await expect(page.getByLabel("Pairing token")).toHaveValue("");
    await expect(page.getByText(daemon.serverId)).toBeVisible();

    await activateButton(page, "Refresh");
    await expect(page.getByLabel("sessions").getByText("00000000-0000-0000-0000-000000000501")).toBeVisible();

    await activateButton(page, "Attach");
    const terminalPane = page.getByTestId("terminal-pane");
    await expect(terminalPane.getByText("controller")).toBeVisible();
    await expect(terminalPane).toHaveAttribute("data-output-chunks", "1");

    await page.getByRole("textbox", { name: "Terminal input" }).focus();
    await page.keyboard.type("terminal-secret");
    await page.keyboard.press("Enter");
    await expect
      .poll(() => daemon.decryptedInputs.join(""))
      .toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("terminal-secret");
    expect(daemon.outerWireText()).not.toContain("secret-token");

    await activateButton(page, "Steal control");
    await expect(page.getByTestId("terminal-pane").getByText("controller")).toBeVisible();

    await page.reload();
    await expect(page.getByText(daemon.serverId)).toBeVisible();
    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain("secret-token");
  } finally {
    await daemon.stop();
  }
});
