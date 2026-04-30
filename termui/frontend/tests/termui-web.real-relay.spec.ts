import { expect, test } from "@playwright/test";
import { startRealRelayFixture } from "./real-relay-fixture";

test("浏览器通过真实 relay 连接 daemon 完成 pairing 和 session list", async ({ page }) => {
  const fixture = await startRealRelayFixture();

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(fixture.relayClientUrl);
    await page.getByLabel("Pairing token").fill(fixture.token);
    await page.getByRole("button", { name: "Pair" }).click();

    await expect(page.getByText(fixture.serverId)).toBeVisible();
    await expect(page.getByLabel("Pairing token")).toHaveValue("");

    await page.getByRole("button", { name: "Refresh" }).click();
    await expect(page.getByLabel("sessions").getByText("No sessions")).toBeVisible();

    const localStorageText = await page.evaluate(() => JSON.stringify(window.localStorage));
    expect(localStorageText).not.toContain(fixture.token);
  } finally {
    await fixture.stop();
  }
});
