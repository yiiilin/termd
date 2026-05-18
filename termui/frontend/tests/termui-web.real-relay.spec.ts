import { expect, test, type Page } from "@playwright/test";
import { startRealRelayFixture } from "./real-relay-fixture";

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

test("浏览器通过真实 relay 连接 daemon 完成 pairing 和 session list", async ({ page }, testInfo) => {
  const fixture = await startRealRelayFixture();

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(fixture));
    await page.getByRole("button", { name: "Pair" }).click();

    await expect(page.getByLabel("Pairing token")).toBeHidden();

    if (testInfo.project.name === "mobile-chrome") {
      await expect(page.getByRole("navigation", { name: "mobile workspace actions" })).toHaveCount(0);
      const menu = await openMobileMenu(page);
      await expect(menu.getByRole("button", { name: "Daemons" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Sessions" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "Files" })).toBeVisible();
      await expect(menu.getByRole("button", { name: "New" })).toBeVisible();
      await menu.getByRole("button", { name: "Daemons" }).click();
      await expect(page.getByRole("main", { name: "daemon admin" })).toBeVisible();
      await expect(page.getByRole("region", { name: "connection" })).toBeVisible();
      await expect(page.getByLabel("daemon manager")).toBeVisible();
      await activateButton(page, "Open workspace");

      const reopenedMenu = await openMobileMenu(page);
      await reopenedMenu.getByRole("button", { name: "Sessions" }).click();
      await expect(page.getByRole("region", { name: "sessions panel" })).toBeVisible();
      await activateButton(page, "Refresh sessions");

      // 移动端刷新后，顶部入口仍然必须保持在左侧，不允许被布局规则顶到右边。
      const mobileMenuButton = page.getByRole("button", { name: "Open mobile workspace menu" });
      const menuBox = await mobileMenuButton.boundingBox();
      expect(menuBox?.x ?? 0).toBeLessThan(48);
    }

    await expect(page.getByLabel("sessions").getByText("No sessions")).toBeVisible();

    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain(fixture.token);
  } finally {
    await fixture.stop();
  }
});

function pairingInviteCode(fixture: { relayClientUrl: string; serverId: string; token: string; daemonPublicKey: string }): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 1,
    ws_url: fixture.relayClientUrl,
    token: fixture.token,
    server_id: fixture.serverId,
    daemon_public_key: fixture.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}
