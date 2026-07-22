import { expect, test, type Locator, type Page } from "@playwright/test";

const viewports = [
  {
    name: "320x480 narrow portrait",
    viewport: { width: 320, height: 480 },
    safeArea: { top: 24, right: 0, bottom: 20, left: 0 },
  },
  {
    name: "844x390 short landscape",
    viewport: { width: 844, height: 390 },
    safeArea: { top: 0, right: 34, bottom: 21, left: 34 },
  },
] as const;

async function expectActionInsideViewport(
  page: Page,
  action: Locator,
  safeArea: { top: number; right: number; bottom: number; left: number },
): Promise<void> {
  await expect(action).toBeVisible();
  await expect(action).toBeEnabled();

  const viewport = page.viewportSize();
  const box = await action.boundingBox();
  expect(viewport).not.toBeNull();
  expect(box).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(safeArea.left);
  expect(box!.y).toBeGreaterThanOrEqual(safeArea.top);
  expect(box!.x + box!.width).toBeLessThanOrEqual(viewport!.width - safeArea.right);
  expect(box!.y + box!.height).toBeLessThanOrEqual(viewport!.height - safeArea.bottom);
  expect(box!.height).toBeGreaterThanOrEqual(44);
}

test.beforeEach(async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile-chrome", "touch layout only");
  await page.addInitScript(() => {
    localStorage.clear();
    sessionStorage.clear();
    indexedDB.deleteDatabase("termd-termui-web");
  });
});

for (const scenario of viewports) {
  test(`settings actions stay reachable in ${scenario.name}`, async ({ page }) => {
    await page.setViewportSize(scenario.viewport);
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: scenario.safeArea });

    await page.goto("/");
    await page.waitForLoadState("networkidle");
    const settingsTrigger = page.getByRole("button", { name: "Settings" });
    await settingsTrigger.click();

    const dialog = page.getByRole("dialog", { name: "Settings" });
    const textarea = dialog.getByRole("textbox", { name: "Mobile shortcuts" });
    const cancel = dialog.getByRole("button", { name: "Cancel" });
    const apply = dialog.getByRole("button", { name: "Apply" });
    await expect(dialog).toBeVisible();

    if (scenario.viewport.width === 320) {
      const notificationGroup = dialog.getByRole("radiogroup", { name: "Notifications" });
      const notificationLabels = notificationGroup.locator("label");
      await expect(notificationLabels).toHaveCount(3);
      await expect(notificationLabels).toHaveText(["Off", "Needs attention", "All AI activity"]);
      const labelGeometry = await notificationLabels.evaluateAll((labels) =>
        labels.map((label) => {
          const text = label.querySelector("span");
          const rect = label.getBoundingClientRect();
          return {
            left: rect.left,
            right: rect.right,
            height: rect.height,
            textFits: text
              ? text.scrollWidth <= text.clientWidth && text.scrollHeight <= text.clientHeight
              : false,
          };
        }),
      );
      expect(labelGeometry.every(({ height, textFits }) => height >= 44 && textFits)).toBe(true);
      for (let index = 1; index < labelGeometry.length; index += 1) {
        expect(labelGeometry[index - 1].right).toBeLessThanOrEqual(labelGeometry[index].left);
      }
    }

    await textarea.fill("Pending=abc");
    await expectActionInsideViewport(page, cancel, scenario.safeArea);
    await expectActionInsideViewport(page, apply, scenario.safeArea);

    await cancel.click();
    await expect(textarea).toHaveValue("");

    await textarea.fill("Esc=\\e");
    await apply.click();
    await expect(apply).toBeDisabled();
    await expect.poll(() => dialog.evaluate((element) => element.contains(document.activeElement))).toBe(true);

    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(settingsTrigger).toBeFocused();
  });
}
