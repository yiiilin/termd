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

function pairingInviteCode(daemon: MockDaemon): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 2,
    token: "secret-token",
    server_id: daemon.serverId,
    daemon_public_key: daemon.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v2:${Buffer.from(payload, "utf8").toString("base64url")}`;
}

async function resetBrowserState(page: Page): Promise<void> {
  await page.addInitScript(() => {
    if (window.name === "__TERMD_TEST_STATE_RESET_DONE__") {
      return;
    }
    window.name = "__TERMD_TEST_STATE_RESET_DONE__";
    window.localStorage.clear();
    window.sessionStorage.clear();
    indexedDB.deleteDatabase("termd-termui-web");
  });
}

test("xterm viewport 背景跟随终端主题，不回退到默认黑底", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chrome", "该回归只需要桌面 xterm 样式覆盖");
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-000000000541",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: "surface-theme-ready\n",
  });

  try {
    await resetBrowserState(page);
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");
    await expect
      .poll(async () => page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? ""))
      .toContain("surface-theme-ready");
    await page.locator(".terminal-host .xterm-viewport").waitFor();

    const backgrounds = await page.locator(".terminal-host").evaluate((host) => {
      const hostElement = host as HTMLElement;
      const xterm = hostElement.querySelector<HTMLElement>(".xterm");
      const screen = hostElement.querySelector<HTMLElement>(".xterm-screen");
      const viewport = hostElement.querySelector<HTMLElement>(".xterm-viewport");
      if (!xterm || !screen || !viewport) {
        throw new Error("xterm surface is incomplete");
      }
      return {
        host: getComputedStyle(hostElement).backgroundColor,
        xterm: getComputedStyle(xterm).backgroundColor,
        screen: getComputedStyle(screen).backgroundColor,
        viewport: getComputedStyle(viewport).backgroundColor,
      };
    });

    const sampleTerminalBottomEdge = async () =>
      page.locator(".terminal-host").evaluate((host) => {
        const hostElement = host as HTMLElement;
        const hostRect = hostElement.getBoundingClientRect();
        const sampleX = hostRect.left + hostRect.width * 0.78;
        const sampleY = hostRect.bottom - 2;
        const hit = document.elementFromPoint(sampleX, sampleY) as HTMLElement | null;
        return {
          hostBackground: getComputedStyle(hostElement).backgroundColor,
          tagName: hit?.tagName ?? "",
          className: hit?.className ?? "",
          background: hit ? getComputedStyle(hit).backgroundColor : "",
        };
      });

    let bottomEdgeSample = await sampleTerminalBottomEdge();
    if (!String(bottomEdgeSample.className).includes("xterm")) {
      for (const viewportHeight of [777, 781, 789]) {
        await page.setViewportSize({ width: 1366, height: viewportHeight });
        await expect
          .poll(async () => page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? ""))
          .toContain("surface-theme-ready");
        bottomEdgeSample = await sampleTerminalBottomEdge();
        if (String(bottomEdgeSample.className).includes("xterm")) {
          break;
        }
      }
    }

    // 中文注释：xterm 官方 CSS 默认把 viewport 背景写死成黑色；一旦 canvas 因取整
    // 露出 1px 空隙，用户就会在终端底边看到黑线。这里除了检查样式合同，还直接抽样
    // 终端底边的真实可见表面，确认用户肉眼看到的那层底色也跟随 host。
    expect(String(bottomEdgeSample.className)).toContain("xterm");
    expect(backgrounds.viewport).toBe(backgrounds.host);
    expect(backgrounds.xterm).toBe(backgrounds.host);
    expect(backgrounds.screen).toBe(backgrounds.host);
    expect(bottomEdgeSample.hostBackground).toBe(backgrounds.host);
    expect(bottomEdgeSample.background).toBe(backgrounds.host);
  } finally {
    await daemon.stop();
  }
});
