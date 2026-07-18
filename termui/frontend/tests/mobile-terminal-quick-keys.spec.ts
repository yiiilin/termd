import { expect, test, type Locator, type Page } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

async function installMutableVisualViewport(page: Page, layoutHeight: number): Promise<void> {
  await page.addInitScript((initialLayoutHeight) => {
    const target = new EventTarget();
    const state = { layoutHeight: initialLayoutHeight, visualHeight: initialLayoutHeight, offsetTop: 0 };
    const scope = window as typeof window & {
      __TERMD_TEST_SET_VISUAL_VIEWPORT__?: (next: typeof state) => void;
    };
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      get: () => state.layoutHeight,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: {
        get height() { return state.visualHeight; },
        get width() { return window.innerWidth; },
        get offsetTop() { return state.offsetTop; },
        get offsetLeft() { return 0; },
        get pageTop() { return state.offsetTop; },
        get pageLeft() { return 0; },
        get scale() { return 1; },
        addEventListener: target.addEventListener.bind(target),
        removeEventListener: target.removeEventListener.bind(target),
        dispatchEvent: target.dispatchEvent.bind(target),
      },
      writable: true,
    });
    scope.__TERMD_TEST_SET_VISUAL_VIEWPORT__ = (next) => {
      Object.assign(state, next);
      target.dispatchEvent(new Event("resize"));
      window.dispatchEvent(new Event("resize"));
    };
  }, layoutHeight);
}

async function setVisualViewport(page: Page, input: { layoutHeight: number; visualHeight: number; offsetTop: number }) {
  await page.evaluate((next) => {
    (window as typeof window & {
      __TERMD_TEST_SET_VISUAL_VIEWPORT__?: (value: typeof next) => void;
    }).__TERMD_TEST_SET_VISUAL_VIEWPORT__?.(next);
  }, input);
}

async function pressTouch(button: Locator, pointerId: number): Promise<void> {
  await button.evaluate((element, id) => {
    element.dispatchEvent(new PointerEvent("pointerdown", {
      bubbles: true,
      cancelable: true,
      pointerId: id,
      pointerType: "touch",
      button: 0,
      buttons: 1,
      clientX: 20,
      clientY: 20,
    }));
    element.dispatchEvent(new PointerEvent("pointerup", {
      bubbles: true,
      cancelable: true,
      pointerId: id,
      pointerType: "touch",
      button: 0,
      buttons: 0,
      clientX: 20,
      clientY: 20,
    }));
  }, pointerId);
}

async function quickKeysGeometry(page: Page) {
  return page.locator(".terminal-pane").evaluate((pane) => {
    const scrollport = pane.querySelector<HTMLElement>(".terminal-scrollport");
    const quickKeys = pane.querySelector<HTMLElement>(".terminal-mobile-shortcuts");
    if (!scrollport || !quickKeys) {
      return undefined;
    }
    const paneRect = pane.getBoundingClientRect();
    const scrollportRect = scrollport.getBoundingClientRect();
    const quickKeysRect = quickKeys.getBoundingClientRect();
    const visualBottom = (window.visualViewport?.offsetTop ?? 0) + (window.visualViewport?.height ?? window.innerHeight);
    return {
      paneBottom: paneRect.bottom,
      visualBottom,
      quickKeysHeight: quickKeysRect.height,
      quickKeysBottom: quickKeysRect.bottom,
      terminalBottom: scrollportRect.bottom,
      terminalEndsAtQuickKeys: Math.abs(scrollportRect.bottom - quickKeysRect.top) <= 2,
      quickKeysEndsAtViewport: Math.abs(quickKeysRect.bottom - visualBottom) <= 2,
      insidePane: quickKeysRect.left >= paneRect.left - 1 && quickKeysRect.right <= paneRect.right + 1,
    };
  });
}

async function compactQuickKeysGeometry(quickKeys: Locator) {
  return quickKeys.locator(".terminal-quick-keys-main").evaluate((main) => {
    const mainRect = main.getBoundingClientRect();
    const requiredButtons = Array.from(main.querySelectorAll<HTMLElement>("button")).slice(0, 10);
    return {
      clientWidth: main.clientWidth,
      scrollWidth: main.scrollWidth,
      requiredButtonsInside: requiredButtons.every((button) => {
        const rect = button.getBoundingClientRect();
        return rect.left >= mainRect.left - 1 && rect.right <= mainRect.right + 1;
      }),
      buttonContentFits: requiredButtons.every((button) => (
        button.scrollWidth <= button.clientWidth + 1 &&
        button.scrollHeight <= button.clientHeight + 1
      )),
    };
  });
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

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.clear();
    sessionStorage.clear();
    indexedDB.deleteDatabase("termd-termui-web");
  });
});

test("mobile terminal quick keys follow keyboard viewport in portrait and landscape", async ({ page }, testInfo) => {
  test.skip(
    testInfo.project.name !== "mobile-chrome" && testInfo.project.name !== "mobile-iphone-layout",
    "mobile device layout only",
  );
  test.setTimeout(60_000);

  const initialViewport = page.viewportSize();
  if (!initialViewport) {
    throw new Error("mobile project has no viewport");
  }
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Emulation.setSafeAreaInsetsOverride", {
    insets: { top: 0, right: 0, bottom: 34, left: 0 },
  });
  await installMutableVisualViewport(page, initialViewport.height);
  const sessionId = "00000000-0000-0000-0000-0000000006a1";
  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [{
      session_id: sessionId,
      state: "running",
      size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    }],
    attachOutput: "quick-keys-ready\n",
  });

  try {
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await page.getByRole("button", { name: "Pair" }).click();
    await expect.poll(() => page.locator(".terminal-host").getAttribute("data-termd-buffer"))
      .toContain("quick-keys-ready");

    const terminalInput = page.locator('.terminal-host textarea[aria-label="Terminal input"]');
    await page.locator(".terminal-host .xterm-screen, .terminal-host canvas").first().click({ position: { x: 20, y: 20 } });
    await expect(terminalInput).toBeFocused();
    await setVisualViewport(page, {
      layoutHeight: initialViewport.height,
      visualHeight: Math.min(500, initialViewport.height - 220),
      offsetTop: 12,
    });

    const shell = page.locator(".app-shell");
    const quickKeys = page.locator(".terminal-mobile-shortcuts");
    await expect(shell).toHaveClass(/mobile-terminal-input/);
    await expect(shell).toHaveClass(/mobile-keyboard-open/);
    await expect(quickKeys).toBeVisible();
    await expect(terminalInput).toBeFocused();
    await expect.poll(() => quickKeysGeometry(page)).toMatchObject({
      terminalEndsAtQuickKeys: true,
      quickKeysEndsAtViewport: true,
      insidePane: true,
      quickKeysHeight: 42,
    });

    const compactLabels = await quickKeys.locator(".terminal-quick-keys-main button").evaluateAll((buttons) =>
      buttons.map((button) => button.textContent?.trim()).filter(Boolean).slice(0, 9),
    );
    expect(compactLabels).toEqual(["ESC", "TAB", "CTRL", "ALT", "SHIFT", "←", "↑", "↓", "→"]);
    const compactGeometry = await compactQuickKeysGeometry(quickKeys);
    expect(compactGeometry.scrollWidth).toBeLessThanOrEqual(compactGeometry.clientWidth + 1);
    expect(compactGeometry.requiredButtonsInside).toBe(true);
    expect(compactGeometry.buttonContentFits).toBe(true);

    await page.setViewportSize({ width: 320, height: initialViewport.height });
    await setVisualViewport(page, {
      layoutHeight: initialViewport.height,
      visualHeight: Math.min(500, initialViewport.height - 220),
      offsetTop: 12,
    });
    await expect.poll(() => compactQuickKeysGeometry(quickKeys)).toMatchObject({
      requiredButtonsInside: true,
      buttonContentFits: true,
    });
    const narrowCompactGeometry = await compactQuickKeysGeometry(quickKeys);
    expect(narrowCompactGeometry.scrollWidth).toBeLessThanOrEqual(narrowCompactGeometry.clientWidth + 1);

    await page.setViewportSize(initialViewport);
    await setVisualViewport(page, {
      layoutHeight: initialViewport.height,
      visualHeight: Math.min(500, initialViewport.height - 220),
      offsetTop: 12,
    });
    await expect(terminalInput).toBeFocused();

    const baseline = daemon.decryptedInputs.length;
    await pressTouch(page.getByRole("button", { name: "Ctrl" }), 100);
    await terminalInput.press("c");
    await expect.poll(() => daemon.decryptedInputs.slice(baseline)).toEqual(["\x03"]);
    await expect(page.getByRole("button", { name: "Ctrl" })).toHaveAttribute("aria-pressed", "false");

    await pressTouch(page.getByRole("button", { name: "Shift" }), 101);
    await pressTouch(page.getByRole("button", { name: "Tab" }), 102);
    await expect.poll(() => daemon.decryptedInputs.slice(baseline)).toEqual(["\x03", "\x1b[Z"]);
    await expect(terminalInput).toBeFocused();

    await pressTouch(page.getByRole("button", { name: "Expand terminal keys" }), 103);
    await expect(page.getByRole("button", { name: "HOME" })).toBeVisible();
    await expect.poll(() => quickKeysGeometry(page)).toMatchObject({
      terminalEndsAtQuickKeys: true,
      quickKeysEndsAtViewport: true,
      insidePane: true,
      quickKeysHeight: 120,
    });
    await pressTouch(page.getByRole("tab", { name: "Ctrl combinations" }), 104);
    await pressTouch(page.getByRole("button", { name: "^C" }), 105);
    await expect.poll(() => daemon.decryptedInputs.slice(baseline)).toEqual(["\x03", "\x1b[Z", "\x03"]);

    daemon.pushSessionData(sessionId, "\x1b[?1happlication-cursor-ready\n");
    await expect.poll(() => page.locator(".terminal-host").getAttribute("data-termd-buffer"))
      .toContain("application-cursor-ready");
    await pressTouch(page.getByRole("button", { name: "Arrow up" }), 106);
    await pressTouch(page.getByRole("button", { name: "Alt" }), 107);
    await pressTouch(page.getByRole("button", { name: "Arrow left" }), 108);
    await expect.poll(() => daemon.decryptedInputs.slice(baseline)).toEqual([
      "\x03",
      "\x1b[Z",
      "\x03",
      "\x1bOA",
      "\x1b[1;3D",
    ]);

    const heldArrow = page.getByRole("button", { name: "Arrow down" });
    const repeatBaseline = daemon.decryptedInputs.length;
    await heldArrow.dispatchEvent("pointerdown", {
      pointerId: 109,
      pointerType: "touch",
      button: 0,
      buttons: 1,
      clientX: 20,
      clientY: 20,
    });
    await page.waitForTimeout(650);
    await heldArrow.dispatchEvent("pointerup", {
      pointerId: 109,
      pointerType: "touch",
      button: 0,
      buttons: 0,
      clientX: 20,
      clientY: 20,
    });
    await expect.poll(() => daemon.decryptedInputs.length).toBeGreaterThanOrEqual(repeatBaseline + 3);
    expect(daemon.decryptedInputs.slice(repeatBaseline).every((data) => data === "\x1bOB")).toBe(true);

    const landscape = { width: Math.max(initialViewport.width, initialViewport.height), height: Math.min(initialViewport.width, initialViewport.height) };
    await page.setViewportSize(landscape);
    await setVisualViewport(page, {
      layoutHeight: landscape.height,
      visualHeight: Math.max(220, landscape.height - 130),
      offsetTop: 0,
    });
    await expect(shell).toHaveClass(/mobile-terminal-input/);
    await expect(shell).toHaveClass(/mobile-keyboard-open/);
    await expect(quickKeys).toBeVisible();
    await expect.poll(() => quickKeysGeometry(page)).toMatchObject({
      terminalEndsAtQuickKeys: true,
      quickKeysEndsAtViewport: true,
      insidePane: true,
      quickKeysHeight: 106,
    });

    await setVisualViewport(page, {
      layoutHeight: landscape.height,
      visualHeight: landscape.height,
      offsetTop: 0,
    });
    await expect(shell).not.toHaveClass(/mobile-keyboard-open/);
    await expect(quickKeys).toHaveCount(0);
  } finally {
    await daemon.stop();
  }
});
